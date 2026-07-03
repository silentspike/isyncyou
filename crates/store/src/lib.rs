//! `isyncyou-store` — id-based SQLite store with FTS5, migrations and a
//! single-instance lock.
//!
//! Everything is tracked by stable id (`(account_id, service, remote_id)`), never
//! by path, so moves/renames don't lose identity. Delta cursors are persisted per
//! `(account, service, scope)` so incremental sync survives restarts. File names
//! are full-text indexed via an external-content FTS5 table kept in sync by
//! triggers.
//!
//! WAL journaling + a `<db>.lock` advisory lock (single writer per store) protect
//! against corruption from concurrent daemons.

use fs2::FileExt;
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use std::fs::{File, OpenOptions};
use std::path::Path;
// PathBuf is only used by the encrypted-store migrate/credential paths.
#[cfg(feature = "encrypted-store")]
use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("another instance already holds the store lock at {0}")]
    AlreadyRunning(String),
    #[error("store encryption secret is invalid: {0}")]
    InvalidStoreSecret(String),
    #[error("store at {0} is already encrypted (not a plaintext SQLite file)")]
    AlreadyEncrypted(String),
    #[error("illegal restore-operation transition: {0}")]
    IllegalTransition(String),
}

pub type Result<T> = std::result::Result<T, StoreError>;

/// Current schema version. Bump + add a migration step when the schema changes.
pub const SCHEMA_VERSION: i64 = 15;

pub const STORE_KEY_ENV: &str = "ISYNCYOU_STORE_KEY";
pub const STORE_KEY_FILE_ENV: &str = "ISYNCYOU_STORE_KEY_FILE";
pub const STORE_SYSTEMD_CREDENTIAL: &str = "isyncyou-store-key";
pub const SYSTEMD_CREDENTIALS_DIR_ENV: &str = "CREDENTIALS_DIRECTORY";

const MIGRATION_V1: &str = r#"
CREATE TABLE items (
    id                INTEGER PRIMARY KEY,
    account_id        TEXT NOT NULL,
    service           TEXT NOT NULL,
    remote_id         TEXT NOT NULL,
    parent_remote_id  TEXT,
    name              TEXT NOT NULL,
    local_path        TEXT,
    item_type         TEXT NOT NULL,            -- 'file' | 'folder'
    etag              TEXT,
    ctag              TEXT,
    quickxorhash      TEXT,
    size              INTEGER,
    remote_mtime      TEXT,
    sync_state        TEXT NOT NULL DEFAULT 'clean',
    deleted_at        TEXT,
    UNIQUE(account_id, service, remote_id)
);
CREATE INDEX idx_items_parent ON items(account_id, service, parent_remote_id);

CREATE TABLE delta_state (
    account_id  TEXT NOT NULL,
    service     TEXT NOT NULL,
    scope       TEXT NOT NULL DEFAULT '',       -- folder/calendar id; '' = whole service
    cursor      TEXT NOT NULL,
    generation  INTEGER NOT NULL DEFAULT 1,
    PRIMARY KEY (account_id, service, scope)
);

-- External-content FTS5 over file names, kept in sync by triggers.
CREATE VIRTUAL TABLE items_fts USING fts5(
    name,
    content='items',
    content_rowid='id',
    tokenize='unicode61 remove_diacritics 2'
);
CREATE TRIGGER items_ai AFTER INSERT ON items BEGIN
    INSERT INTO items_fts(rowid, name) VALUES (new.id, new.name);
END;
CREATE TRIGGER items_ad AFTER DELETE ON items BEGIN
    INSERT INTO items_fts(items_fts, rowid, name) VALUES ('delete', old.id, old.name);
END;
CREATE TRIGGER items_au AFTER UPDATE ON items BEGIN
    INSERT INTO items_fts(items_fts, rowid, name) VALUES ('delete', old.id, old.name);
    INSERT INTO items_fts(rowid, name) VALUES (new.id, new.name);
END;
"#;

/// Schema v2: a separate, optional body-text index (plan §9 — search must cover
/// mail bodies, not just names). Kept out of `items` so the metadata table stays
/// lean and the body index can be rebuilt or disabled independently. The
/// `bodies_fts` external-content table mirrors `bodies` via triggers, exactly as
/// `items_fts` mirrors `items`.
const MIGRATION_V2: &str = r#"
CREATE TABLE bodies (
    id          INTEGER PRIMARY KEY,
    account_id  TEXT NOT NULL,
    service     TEXT NOT NULL,
    remote_id   TEXT NOT NULL,
    body        TEXT NOT NULL,
    UNIQUE(account_id, service, remote_id)
);
CREATE VIRTUAL TABLE bodies_fts USING fts5(
    body,
    content='bodies',
    content_rowid='id',
    tokenize='unicode61 remove_diacritics 2'
);
CREATE TRIGGER bodies_ai AFTER INSERT ON bodies BEGIN
    INSERT INTO bodies_fts(rowid, body) VALUES (new.id, new.body);
END;
CREATE TRIGGER bodies_ad AFTER DELETE ON bodies BEGIN
    INSERT INTO bodies_fts(bodies_fts, rowid, body) VALUES ('delete', old.id, old.body);
END;
CREATE TRIGGER bodies_au AFTER UPDATE ON bodies BEGIN
    INSERT INTO bodies_fts(bodies_fts, rowid, body) VALUES ('delete', old.id, old.body);
    INSERT INTO bodies_fts(rowid, body) VALUES (new.id, new.body);
END;
"#;

const MIGRATION_V3: &str = r#"
CREATE TABLE runs (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    account_id  TEXT NOT NULL,
    kind        TEXT NOT NULL,
    started_at  TEXT NOT NULL,
    finished_at TEXT NOT NULL,
    status      TEXT NOT NULL,
    summary     TEXT NOT NULL
);
CREATE INDEX runs_account ON runs(account_id, id DESC);
"#;

/// Schema v4: the Outlook immutable-ID policy (plan §6). With the
/// `Prefer: IdType="ImmutableId"` request header the stored `remote_id` is itself
/// the immutable id (stable across folder moves); these columns hold the companion
/// identifiers the policy lists for richer de-duplication and restore fidelity:
/// `changeKey` (optimistic concurrency), `internetMessageId` (mail), `iCalUId`
/// (calendar). Added with `ALTER TABLE` so existing stores upgrade in place.
const MIGRATION_V4: &str = r#"
ALTER TABLE items ADD COLUMN change_key TEXT;
ALTER TABLE items ADD COLUMN internet_message_id TEXT;
ALTER TABLE items ADD COLUMN ical_uid TEXT;
"#;

/// Schema v5: calendar series tracking (plan §6 — series-master/instance/exception
/// separation). For a recurring event's occurrence/exception this holds the id of
/// its series master; it is `NULL` for single-instance events and for the master
/// row itself. Lets the model keep the recurring series (the master, carrying the
/// recurrence rule) distinct from its expanded occurrences.
const MIGRATION_V5: &str = r#"
ALTER TABLE items ADD COLUMN series_master_id TEXT;
"#;

/// Schema v6: persisted resumable upload sessions (plan §6/§9). A large OneDrive
/// upload's session (`uploadUrl` + total + last next-offset) is recorded here so a
/// process kill mid-upload resumes from the server's `nextExpectedRanges` instead
/// of restarting. The row is cleared on completion. Keyed by destination path.
const MIGRATION_V6: &str = r#"
CREATE TABLE upload_sessions (
    account_id   TEXT NOT NULL,
    service      TEXT NOT NULL,
    dest_path    TEXT NOT NULL,
    upload_url   TEXT NOT NULL,
    total        INTEGER NOT NULL,
    next_offset  INTEGER NOT NULL DEFAULT 0,
    updated_at   TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (account_id, service, dest_path)
);
"#;

/// Schema v7: the crash-safe cloud-restore operation ledger (see
/// `docs/adr/001-restore-semantics.md`). `restore_operations` records one row per
/// restore intent with an explicit state machine; intent is written *before* the
/// Graph call and the outcome *after* it, so recovery can tell that a `POST` may
/// have landed and reconcile instead of blind-retrying. `idempotency_key` is unique
/// per account (the database backstop against duplicate creates), and the lease
/// columns give a single owner at a time. `restore_steps` is an append-only audit of
/// every transition (evidence + debuggability).
const MIGRATION_V7: &str = r#"
CREATE TABLE restore_operations (
    op_id            TEXT PRIMARY KEY,
    account_id       TEXT NOT NULL,
    service          TEXT NOT NULL,
    source_item_id   TEXT NOT NULL,
    idempotency_key  TEXT NOT NULL,
    state            TEXT NOT NULL,
    new_cloud_id     TEXT,
    marker           TEXT,
    attempts         INTEGER NOT NULL DEFAULT 0,
    lease_owner      TEXT,
    lease_expires_at INTEGER,
    created_at       INTEGER NOT NULL,
    updated_at       INTEGER NOT NULL,
    last_error       TEXT,
    UNIQUE(account_id, idempotency_key)
);
CREATE INDEX idx_restore_ops_open ON restore_operations(account_id, state);

CREATE TABLE restore_steps (
    op_id      TEXT NOT NULL REFERENCES restore_operations(op_id),
    seq        INTEGER NOT NULL,
    from_state TEXT,
    to_state   TEXT NOT NULL,
    at         INTEGER NOT NULL,
    detail     TEXT,
    PRIMARY KEY (op_id, seq)
);
"#;

/// v8: the **last-synced on-disk reference** per item (size / mtime / hash of
/// the file as it was when last downloaded or uploaded). The delta ingest
/// overwrites an item's metadata with the NEW remote values, so without this
/// reference a sync pass cannot tell "stale but clean local file" from
/// "locally edited file" — which is what download keep-both needs. Set only by
/// the materialize/upload paths, never by the delta ingest.
const MIGRATION_V8: &str = r#"
ALTER TABLE items ADD COLUMN synced_size INTEGER;
ALTER TABLE items ADD COLUMN synced_mtime_unix INTEGER;
ALTER TABLE items ADD COLUMN synced_hash TEXT;
"#;

/// v9: per-item **archive integrity** state (the real backing for the web UI's
/// "Integrity verified" / "Verified" signals). `body_sha256` is the SHA-256 of
/// the archived body file (`local_path`) as recorded at the last verify pass —
/// the integrity baseline; `verify_status` is the outcome of the last check
/// (`verified` / `changed` / `failed`) and `verified_at` its RFC3339 timestamp.
/// Set ONLY by the verify pass (never by the delta ingest), so a re-ingest that
/// changes a body leaves the old hash in place and the next verify detects the
/// drift. NULL = never verified.
const MIGRATION_V9: &str = r#"
ALTER TABLE items ADD COLUMN body_sha256 TEXT;
ALTER TABLE items ADD COLUMN verified_at TEXT;
ALTER TABLE items ADD COLUMN verify_status TEXT;
"#;

/// v10: the cloud version the archived body corresponds to. `body_etag` is set to
/// the item's `etag` when the body is archived (`set_local_path`). The delta ingest
/// overwrites `etag` with the new remote value but NOT `body_etag`, so the web UI's
/// 4-state status can detect a **stale** backup (`etag != body_etag` = cloud changed
/// since the body was archived) vs. an up-to-date one. NULL = no body archived.
const MIGRATION_V10: &str = r#"
ALTER TABLE items ADD COLUMN body_etag TEXT;
"#;

/// v11: the display sender of a mail item (`"Name <addr>"`), captured at ingest
/// straight from the delta payload — so the list shows who a message is from even
/// when its `.eml` body isn't cached (the mobile cache caps bodies; without this the
/// row reads "(unknown sender)"). Set by the mail connector's ingest; `NULL` for
/// every non-mail service. Read with the item — no per-request body/sidecar I/O.
const MIGRATION_V11: &str = r#"
ALTER TABLE items ADD COLUMN sender TEXT;
"#;

/// v12: the item's list-row `preview` (the exact JSON the web UI's list consumes),
/// computed once at ingest from the archived body + sidecar and cached here. Without
/// it `/api/v1/items` re-reads and re-parses every `.eml` (and base64-decodes every
/// attachment just for its size) on every mailbox load — hundreds of file reads +
/// MIME passes per view. Read straight with the item = no per-request body/sidecar
/// I/O. `NULL` = not yet computed (falls back to on-the-fly parse, which then
/// back-fills this column). Refreshed whenever the body is re-archived.
const MIGRATION_V12: &str = r#"
ALTER TABLE items ADD COLUMN preview_json TEXT;
"#;

/// v13: the `items_fts` index only covers `name`, but the original `items_au` trigger
/// fired on **every** `UPDATE items` — including the metadata-only writes a sync does
/// in bulk (`set_local_path`, `set_preview_json`, `set_sender`, body/verify columns),
/// none of which touch `name`. Each firing deleted + re-inserted the FTS row, amplifying
/// WAL/FTS writes during the sync burst for no benefit. Rebuild the trigger to fire only
/// when `name` is in the UPDATE and actually changed — functionally identical for name
/// search, far less write churn while a sync runs.
const MIGRATION_V13: &str = r#"
DROP TRIGGER IF EXISTS items_au;
CREATE TRIGGER items_au AFTER UPDATE OF name ON items
WHEN old.name IS NOT new.name
BEGIN
    INSERT INTO items_fts(items_fts, rowid, name) VALUES ('delete', old.id, old.name);
    INSERT INTO items_fts(rowid, name) VALUES (new.id, new.name);
END;
"#;

/// v14: OneDrive per-item content state (mobile modes online/sync/offline, #onedrive-mobile).
/// `local_path.is_some()` conflates "sync path known" with "body present", which breaks a
/// metadata-only (Mode 2) row that has no downloaded body. These columns model the body
/// explicitly so `has_body` derives from `body_state=='available'` (a valid, decryptable
/// body) rather than a filesystem probe. Generic on `items` but only populated for OneDrive;
/// every other service leaves them NULL and keeps its `local_path`-based `has_body`.
/// `body_etag` already exists (v10) and is reused. Backfill: an existing OneDrive body
/// (a present `local_path`) is a materialized/available body; folders are not-applicable.
const MIGRATION_V14: &str = r#"
ALTER TABLE items ADD COLUMN content_state TEXT;
ALTER TABLE items ADD COLUMN body_location TEXT;
ALTER TABLE items ADD COLUMN body_state TEXT;
ALTER TABLE items ADD COLUMN plaintext_size INTEGER;
ALTER TABLE items ADD COLUMN plaintext_hash TEXT;
ALTER TABLE items ADD COLUMN encrypted_blob_version INTEGER;
ALTER TABLE items ADD COLUMN materialized_at TEXT;
ALTER TABLE items ADD COLUMN last_download_error TEXT;
ALTER TABLE items ADD COLUMN conflict_state TEXT;
UPDATE items SET content_state='materialized', body_location='sync', body_state='available'
  WHERE service='onedrive' AND item_type != 'folder' AND local_path IS NOT NULL;
UPDATE items SET content_state='not_applicable'
  WHERE service='onedrive' AND item_type = 'folder';
"#;

/// v15: the **cloud-write operation ledger** (#onedrive-mobile 0D). Every mutating
/// OneDrive op (create/upload/replace/rename/move/delete/share) records an idempotent
/// intent BEFORE it hits Graph, so a crash *after* the Graph mutation but *before* the
/// local store update is recoverable: boot recovery re-probes each `pending`/`inflight`
/// op and reconciles. `idempotency_key` (UNIQUE per account) dedups a re-issued intent to
/// the same row — never a second cloud effect. `if_match_etag` carries the optimistic-
/// concurrency guard for replace/rename/move/delete. Modeled on `restore_operations` (v7).
const MIGRATION_V15: &str = r#"
CREATE TABLE cloud_write_operations (
    op_id            TEXT PRIMARY KEY,
    account_id       TEXT NOT NULL,
    service          TEXT NOT NULL,
    op_kind          TEXT NOT NULL,
    target_id        TEXT,
    idempotency_key  TEXT NOT NULL,
    if_match_etag    TEXT,
    state            TEXT NOT NULL,
    result_id        TEXT,
    intent_json      TEXT,
    attempts         INTEGER NOT NULL DEFAULT 0,
    created_at       INTEGER NOT NULL,
    updated_at       INTEGER NOT NULL,
    last_error       TEXT,
    UNIQUE(account_id, idempotency_key)
);
CREATE INDEX idx_cloud_write_ops_open ON cloud_write_operations(account_id, state);
"#;

/// A recorded engine run (one sync/backup/… pass) — the activity history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    pub id: i64,
    pub account_id: String,
    pub kind: String,
    pub started_at: String,
    pub finished_at: String,
    pub status: String,
    pub summary: String,
}

/// A tracked item, keyed by `(account_id, service, remote_id)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub account_id: String,
    pub service: String,
    pub remote_id: String,
    pub parent_remote_id: Option<String>,
    pub name: String,
    pub local_path: Option<String>,
    pub item_type: String,
    pub etag: Option<String>,
    pub ctag: Option<String>,
    pub quickxorhash: Option<String>,
    pub size: Option<i64>,
    pub remote_mtime: Option<String>,
    pub sync_state: String,
    pub deleted_at: Option<String>,
    /// Outlook optimistic-concurrency tag (`changeKey`), if known.
    pub change_key: Option<String>,
    /// RFC 2822 `internetMessageId` for mail items, if known.
    pub internet_message_id: Option<String>,
    /// `iCalUId` for calendar events, if known (stable across the series).
    pub ical_uid: Option<String>,
    /// For a recurring event's occurrence/exception: the id of its series master
    /// (`NULL` for single-instance events and for the master row itself).
    pub series_master_id: Option<String>,
    /// SHA-256 of the archived body (`local_path`) recorded at the last verify
    /// pass — the integrity baseline. `None` = never verified.
    pub body_sha256: Option<String>,
    /// RFC3339 timestamp of the last verify pass for this item.
    pub verified_at: Option<String>,
    /// Outcome of the last verify pass: `verified` / `changed` / `failed`.
    pub verify_status: Option<String>,
    /// The item's `etag` at the time its body was archived (`set_local_path`).
    /// `etag != body_etag` (with a body present) = the cloud changed since the
    /// backup → the web UI shows a **stale** status. `None` = no body archived.
    pub body_etag: Option<String>,
    /// Display sender of a mail item (`"Name <addr>"`), captured at ingest so the
    /// list shows the sender without the `.eml` body. `None` for non-mail / unknown.
    pub sender: Option<String>,
    /// The item's cached list-row `preview` as a JSON string (the exact object the
    /// web UI list consumes), computed once from the body + sidecar so `/api/v1/items`
    /// never re-parses `.eml`/`.json` per row. `None` = not yet computed (on-the-fly
    /// fallback + back-fill). Refreshed when the body is re-archived. (Schema v12.)
    pub preview_json: Option<String>,
    // ---- OneDrive per-item content state (schema v14) -----------------------
    // Only populated for OneDrive (the mobile online/sync/offline modes); NULL for
    // every other service, whose `has_body` stays `local_path`-based.
    /// `online` | `cached` | `materialized` | `not_applicable` (folders).
    pub content_state: Option<String>,
    /// Where the body lives: `none` | `cache` (Mode 1/2 lazy) | `sync` (Mode 3 offline).
    pub body_location: Option<String>,
    /// `downloading` | `available` | `failed` | `missing` | `conflict`. `has_body` is
    /// `body_state == "available"` (a valid, decryptable body), not a filesystem probe.
    pub body_state: Option<String>,
    /// Plaintext byte length of the body (the on-disk blob is a ciphertext container).
    pub plaintext_size: Option<i64>,
    /// Hash of the *plaintext* body (integrity across the encrypted envelope).
    pub plaintext_hash: Option<String>,
    /// Version of the encrypted-body envelope format (for key rotation / re-wrap).
    pub encrypted_blob_version: Option<i64>,
    /// RFC3339 time the body was materialized locally.
    pub materialized_at: Option<String>,
    /// Last download error message, if `body_state == "failed"`.
    pub last_download_error: Option<String>,
    /// Non-null when a local↔cloud conflict is pending (keep-both semantics).
    pub conflict_state: Option<String>,
}

impl Item {
    /// Minimal constructor with sensible defaults (`sync_state = "clean"`).
    pub fn new(
        account_id: impl Into<String>,
        service: impl Into<String>,
        remote_id: impl Into<String>,
        name: impl Into<String>,
        item_type: impl Into<String>,
    ) -> Self {
        Item {
            account_id: account_id.into(),
            service: service.into(),
            remote_id: remote_id.into(),
            parent_remote_id: None,
            name: name.into(),
            local_path: None,
            item_type: item_type.into(),
            etag: None,
            ctag: None,
            quickxorhash: None,
            size: None,
            remote_mtime: None,
            sync_state: "clean".into(),
            deleted_at: None,
            change_key: None,
            internet_message_id: None,
            ical_uid: None,
            series_master_id: None,
            body_sha256: None,
            body_etag: None,
            verified_at: None,
            verify_status: None,
            sender: None,
            preview_json: None,
            content_state: None,
            body_location: None,
            body_state: None,
            plaintext_size: None,
            plaintext_hash: None,
            encrypted_blob_version: None,
            materialized_at: None,
            last_download_error: None,
            conflict_state: None,
        }
    }
}

/// A OneDrive cloud-write op kind (#onedrive-mobile 0D). Each kind has a defined
/// crash-recovery probe, run by boot recovery in the engine when it finds a
/// `pending`/`inflight` ledger row after a crash:
/// - **Create / Upload**: probe the parent listing / an idempotency marker; if the item
///   is already present in the cloud → `applied` (adopt its id), else re-issue.
/// - **Replace**: re-send guarded by `if_match_etag`; a `412` means the cloud moved on
///   → **conflict** (keep-both), never a blind overwrite.
/// - **Rename / Move**: probe the item's current name/parent; if already at the target →
///   `applied`, else re-issue (id-stable).
/// - **Delete**: a `404` == success (already gone) → `applied`; the ONLY kind safe to
///   blindly re-send.
/// - **Share**: probe existing links/permissions first → never a duplicate invite/link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloudOpKind {
    Create,
    Upload,
    Replace,
    Rename,
    Move,
    Delete,
    Share,
}

impl CloudOpKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            CloudOpKind::Create => "create",
            CloudOpKind::Upload => "upload",
            CloudOpKind::Replace => "replace",
            CloudOpKind::Rename => "rename",
            CloudOpKind::Move => "move",
            CloudOpKind::Delete => "delete",
            CloudOpKind::Share => "share",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "create" => CloudOpKind::Create,
            "upload" => CloudOpKind::Upload,
            "replace" => CloudOpKind::Replace,
            "rename" => CloudOpKind::Rename,
            "move" => CloudOpKind::Move,
            "delete" => CloudOpKind::Delete,
            "share" => CloudOpKind::Share,
            _ => return None,
        })
    }
    /// Whether a crashed op of this kind is safe to **blindly re-send** during recovery.
    /// Only `Delete` is (a repeat delete is a no-op / 404=success); every other kind must
    /// be **probed** before re-issue to avoid a duplicate cloud effect.
    pub fn is_blind_replay_safe(&self) -> bool {
        matches!(self, CloudOpKind::Delete)
    }
}

/// One row of the cloud-write operation ledger (#0D): an idempotent record of a mutating
/// OneDrive op, written BEFORE the Graph call so a crash mid-op is recoverable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloudWriteOp {
    pub op_id: String,
    pub account_id: String,
    pub service: String,
    /// One of [`CloudOpKind`]'s wire strings.
    pub op_kind: String,
    /// The item id the op targets (or the parent for `create`), if known before the call.
    pub target_id: Option<String>,
    /// Dedup key: the same intent re-issued yields the same row, never a second effect.
    pub idempotency_key: String,
    /// Optimistic-concurrency guard for replace/rename/move/delete.
    pub if_match_etag: Option<String>,
    /// `pending` → `inflight` → `applied` | `failed` | `conflict` | `superseded`.
    pub state: String,
    /// The resulting cloud id (create/upload), once applied.
    pub result_id: Option<String>,
    /// The op payload (path/name/new-parent/recipient/…) as JSON.
    pub intent_json: Option<String>,
    pub attempts: i64,
    pub last_error: Option<String>,
}

/// The store. Holds the DB connection and (for on-disk stores) the instance lock.
pub struct Store {
    conn: Connection,
    _lock: Option<File>,
}

impl Store {
    /// Open (or create) an on-disk store. Acquires an exclusive `<path>.lock`;
    /// returns [`StoreError::AlreadyRunning`] if another instance holds it.
    ///
    /// If a store encryption secret is configured via
    /// [`STORE_KEY_FILE_ENV`], systemd credential
    /// [`STORE_SYSTEMD_CREDENTIAL`], or [`STORE_KEY_ENV`], the database is opened
    /// with SQLCipher before any schema access. Existing encrypted stores fail
    /// closed when the key is absent or wrong; new stores are created encrypted
    /// when the key is present.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        // With `plain-store` (Android) there is no key path — open plaintext. The
        // SQLCipher `sqlite3_key` FFI symbol does not exist in the bundled-plain
        // build, so the encrypted branch must be compiled out entirely.
        #[cfg(feature = "encrypted-store")]
        if let Some(secret) = configured_store_key()? {
            return Self::open_encrypted(path, &secret);
        }
        Self::open_plain(path)
    }

    /// Open (or create) a SQLCipher-encrypted store using `secret`.
    #[cfg(feature = "encrypted-store")]
    pub fn open_encrypted(path: impl AsRef<Path>, secret: &[u8]) -> Result<Self> {
        if secret.is_empty() {
            return Err(StoreError::InvalidStoreSecret(
                "secret must not be empty".into(),
            ));
        }
        let path = path.as_ref();
        let lock = Self::acquire_lock(path)?;
        let conn = Connection::open(path)?;
        apply_sqlcipher_key(&conn, secret)?;
        Self::init(&conn)?;
        Ok(Store {
            conn,
            _lock: Some(lock),
        })
    }

    fn open_plain(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let lock = Self::acquire_lock(path)?;
        let conn = Connection::open(path)?;
        Self::init(&conn)?;
        Ok(Store {
            conn,
            _lock: Some(lock),
        })
    }

    /// Open the store for **concurrent reads**: no exclusive `<db>.lock` and no schema
    /// migration, so a GET endpoint never has to wait out the writer's instance lock —
    /// the fix for a mailbox load stalling behind an in-flight sync.
    ///
    /// Deliberately opened **read-write** (not `READ_ONLY`): a WAL reader must be able to
    /// update the `-shm` index to see a live snapshot while a writer is active. A pure
    /// `READ_ONLY` handle can't touch `-shm` and, during heavy concurrent writes, falls
    /// back to a slow/blocking path — which is exactly the multi-second stall we're
    /// removing. WAL already allows many connections in one process; the `.lock` only
    /// guarded against a *second process*, and this handle only ever runs SELECTs, so
    /// skipping it keeps the single-writer invariant intact. The DB must already exist
    /// and be migrated by a writer. **Never call a mutating method on the returned store.**
    pub fn open_readonly(path: impl AsRef<Path>) -> Result<Self> {
        // No CREATE flag: a missing DB errors here so the caller falls back to a
        // writable open that creates + migrates it. NO_MUTEX: single-threaded per handle.
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let path = path.as_ref();
        let conn = Connection::open_with_flags(path, flags)?;
        #[cfg(feature = "encrypted-store")]
        if let Some(secret) = configured_store_key()? {
            apply_sqlcipher_key(&conn, &secret)?;
        }
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        Ok(Store {
            conn,
            _lock: None,
        })
    }

    /// In-memory store for tests (no lock, no WAL).
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        migrate(&conn)?;
        Ok(Store { conn, _lock: None })
    }

    /// Write a consistent snapshot of the database to `dest` via SQLite
    /// `VACUUM INTO`. Safe while the store is open and being written — it reads a
    /// single consistent view — so it needs no quiesce. Used for PBS snapshots.
    /// If `dest` already exists it is removed first, because SQLite's
    /// `VACUUM INTO` refuses to overwrite.
    pub fn backup_to(&self, dest: impl AsRef<Path>) -> Result<()> {
        let dest = dest.as_ref();
        if dest.exists() {
            std::fs::remove_file(dest)?;
        }
        let dest_str = dest.to_str().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "non-UTF-8 backup path")
        })?;
        self.conn.execute("VACUUM INTO ?1", params![dest_str])?;
        Ok(())
    }

    /// Migrate an existing **plaintext** store at `path` to a SQLCipher-encrypted
    /// store, in place and atomically (`<path>.encrypting` + rename + fsync). All
    /// rows are preserved; the FTS indexes are rebuilt from their content tables.
    ///
    /// Key semantics are identical to [`Store::open_encrypted`] by construction:
    /// only `apply_sqlcipher_key` ever touches the key, and the plaintext source
    /// is attached *from the keyed connection* with an empty `KEY ''` (SQLCipher's
    /// documented plaintext attach) — so the migrated store opens with the same
    /// secret later.
    ///
    /// Crash-safe + idempotent: the original file is never modified before the
    /// final atomic rename (a crash mid-migration leaves the plaintext store fully
    /// usable, plus a stale temp file that the next run removes); migrating an
    /// already-encrypted store fails with [`StoreError::AlreadyEncrypted`] so a
    /// re-run after success is a clean, detectable no-op for the caller.
    #[cfg(feature = "encrypted-store")]
    pub fn migrate_to_encrypted(path: impl AsRef<Path>, secret: &[u8]) -> Result<()> {
        let path = path.as_ref();
        if secret.is_empty() {
            return Err(StoreError::InvalidStoreSecret(
                "secret must not be empty".into(),
            ));
        }
        if !is_plaintext_sqlite(path)? {
            return Err(StoreError::AlreadyEncrypted(path.display().to_string()));
        }
        let path_str = path.to_str().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "non-UTF-8 store path")
        })?;

        // 1. Open the source plaintext store normally: takes the instance lock,
        //    brings the schema to the current version, and on clean close
        //    checkpoints + removes the WAL so the file is complete on its own.
        let copy_tables: Vec<String>;
        {
            let src = Self::open_plain(path)?;
            // Ordinary data tables only: the FTS5 virtual tables and their shadow
            // tables (`*_fts`, `*_fts_*`) are rebuilt on the target instead.
            let mut stmt = src.conn.prepare(
                "SELECT name FROM sqlite_master WHERE type='table' \
                 AND name NOT LIKE 'sqlite_%' AND sql NOT LIKE 'CREATE VIRTUAL%' \
                 AND name NOT LIKE '%\\_fts' ESCAPE '\\' \
                 AND name NOT LIKE '%\\_fts\\_%' ESCAPE '\\'",
            )?;
            copy_tables = stmt
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<std::result::Result<_, _>>()?;
            drop(stmt);
            src.conn.pragma_update(None, "wal_checkpoint", "TRUNCATE")?;
        }

        // 2. Build the encrypted target at a sibling temp path. A stale temp from
        //    a crashed earlier run is removed first (resume = start fresh).
        let tmp = PathBuf::from(format!("{path_str}.encrypting"));
        for stale in [
            tmp.clone(),
            PathBuf::from(format!("{}-wal", tmp.display())),
            PathBuf::from(format!("{}-shm", tmp.display())),
            PathBuf::from(format!("{}.lock", tmp.display())),
        ] {
            match std::fs::remove_file(&stale) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        {
            let dst = Self::open_encrypted(&tmp, secret)?;
            dst.conn
                .execute("ATTACH DATABASE ?1 AS src KEY ''", params![path_str])?;
            // FK enforcement is per-connection; disable during the bulk copy so
            // table order cannot matter, re-enable afterwards.
            dst.conn.pragma_update(None, "foreign_keys", "OFF")?;
            let copy = || -> Result<()> {
                dst.conn.execute_batch("BEGIN")?;
                for t in &copy_tables {
                    dst.conn.execute_batch(&format!(
                        "INSERT INTO main.\"{t}\" SELECT * FROM src.\"{t}\""
                    ))?;
                }
                dst.conn
                    .execute_batch("INSERT INTO items_fts(items_fts) VALUES('rebuild')")?;
                dst.conn
                    .execute_batch("INSERT INTO bodies_fts(bodies_fts) VALUES('rebuild')")?;
                dst.conn.execute_batch("COMMIT")?;
                Ok(())
            };
            if let Err(e) = copy() {
                let _ = dst.conn.execute_batch("ROLLBACK");
                return Err(e);
            }
            dst.conn.pragma_update(None, "foreign_keys", "ON")?;
            dst.conn.execute_batch("DETACH DATABASE src")?;
            dst.conn.pragma_update(None, "wal_checkpoint", "TRUNCATE")?;
        }
        File::open(&tmp)?.sync_all()?;

        // 3. Atomic swap. The source was closed cleanly (no -wal/-shm), but remove
        //    any stale sidecars defensively: a leftover WAL paired with the new
        //    encrypted file would be treated as hot journal garbage.
        for stale in [
            PathBuf::from(format!("{path_str}-wal")),
            PathBuf::from(format!("{path_str}-shm")),
        ] {
            match std::fs::remove_file(&stale) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        std::fs::rename(&tmp, path)?;
        if let Some(dir) = path.parent() {
            File::open(dir)?.sync_all()?;
        }
        let _ = std::fs::remove_file(format!("{}.lock", tmp.display()));
        Ok(())
    }

    fn acquire_lock(path: &Path) -> Result<File> {
        let lock_path = format!("{}.lock", path.display());
        let f = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)?;
        // The lock is held only for a Store's lifetime (a single request or sync
        // pass), so concurrent opens within one process — the multi-threaded web
        // server firing several store reads at once — just need to wait out the
        // current holder. Retry briefly (~1s); a genuinely long-held lock (a second
        // daemon instance) still fails after the window, preserving single-writer.
        for attempt in 0..40 {
            if f.try_lock_exclusive().is_ok() {
                return Ok(f);
            }
            if attempt < 39 {
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
        }
        Err(StoreError::AlreadyRunning(lock_path))
    }

    fn init(conn: &Connection) -> Result<()> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        migrate(conn)?;
        Ok(())
    }

    pub fn schema_version(&self) -> Result<i64> {
        Ok(self
            .conn
            .pragma_query_value(None, "user_version", |r| r.get(0))?)
    }

    /// Insert or update an item (by `(account, service, remote_id)`). FTS stays in
    /// sync via triggers.
    pub fn upsert_item(&self, it: &Item) -> Result<()> {
        self.conn.execute(
            r#"INSERT INTO items
                 (account_id, service, remote_id, parent_remote_id, name, local_path,
                  item_type, etag, ctag, quickxorhash, size, remote_mtime, sync_state, deleted_at,
                  change_key, internet_message_id, ical_uid, series_master_id, sender)
               VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19)
               ON CONFLICT(account_id, service, remote_id) DO UPDATE SET
                 parent_remote_id    = excluded.parent_remote_id,
                 name                = excluded.name,
                 local_path          = excluded.local_path,
                 item_type           = excluded.item_type,
                 etag                = excluded.etag,
                 ctag                = excluded.ctag,
                 quickxorhash        = excluded.quickxorhash,
                 size                = excluded.size,
                 remote_mtime        = excluded.remote_mtime,
                 sync_state          = excluded.sync_state,
                 deleted_at          = excluded.deleted_at,
                 change_key          = excluded.change_key,
                 internet_message_id = excluded.internet_message_id,
                 ical_uid            = excluded.ical_uid,
                 series_master_id    = excluded.series_master_id,
                 sender              = COALESCE(excluded.sender, sender)"#,
            params![
                it.account_id,
                it.service,
                it.remote_id,
                it.parent_remote_id,
                it.name,
                it.local_path,
                it.item_type,
                it.etag,
                it.ctag,
                it.quickxorhash,
                it.size,
                it.remote_mtime,
                it.sync_state,
                it.deleted_at,
                it.change_key,
                it.internet_message_id,
                it.ical_uid,
                it.series_master_id,
                it.sender
            ],
        )?;
        Ok(())
    }

    /// Set just the `sender` column for one item (CC-1 backfill) without rewriting
    /// the row or disturbing FTS/sync-state. No-op if the item doesn't exist.
    pub fn set_sender(
        &self,
        account: &str,
        service: &str,
        remote_id: &str,
        sender: &str,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE items SET sender=?4 WHERE account_id=?1 AND service=?2 AND remote_id=?3",
            params![account, service, remote_id, sender],
        )?;
        Ok(())
    }

    /// Cache one item's computed list-row `preview` JSON (schema v12) without
    /// rewriting the row or disturbing FTS/sync-state. Set by the read path once the
    /// body/sidecar have been parsed, so later mailbox loads read it straight from the
    /// DB instead of re-parsing the `.eml`. No-op if the item doesn't exist.
    /// Invalidated by [`set_local_path`](Self::set_local_path) when the body changes.
    pub fn set_preview_json(
        &self,
        account: &str,
        service: &str,
        remote_id: &str,
        preview_json: &str,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE items SET preview_json=?4 WHERE account_id=?1 AND service=?2 AND remote_id=?3",
            params![account, service, remote_id, preview_json],
        )?;
        Ok(())
    }

    /// Update a OneDrive item's content-state fields (schema v14): the body lifecycle
    /// (`content_state`, `body_location`, `body_state`, `materialized_at`). Written by the
    /// download/materialize paths so `has_body` derives from `body_state=='available'`
    /// rather than a filesystem probe. No-op if the item doesn't exist. Any argument left
    /// `None` clears that column.
    #[allow(clippy::too_many_arguments)]
    pub fn set_content_state(
        &self,
        account: &str,
        service: &str,
        remote_id: &str,
        content_state: Option<&str>,
        body_location: Option<&str>,
        body_state: Option<&str>,
        materialized_at: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE items SET content_state=?4, body_location=?5, body_state=?6, materialized_at=?7 \
             WHERE account_id=?1 AND service=?2 AND remote_id=?3",
            params![
                account,
                service,
                remote_id,
                content_state,
                body_location,
                body_state,
                materialized_at
            ],
        )?;
        Ok(())
    }

    // ---- Cloud-write operation ledger (#onedrive-mobile 0D) -----------------

    /// Record a cloud-write intent BEFORE issuing it to Graph. Idempotent by
    /// `(account_id, idempotency_key)`: a re-issued intent maps to the existing row and
    /// does NOT create a second op. Returns `true` when a new row was inserted, `false`
    /// when the key already existed (the caller then recovers/reuses that op).
    pub fn record_cloud_write(&self, op: &CloudWriteOp, now: i64) -> Result<bool> {
        let n = self.conn.execute(
            "INSERT OR IGNORE INTO cloud_write_operations \
             (op_id, account_id, service, op_kind, target_id, idempotency_key, if_match_etag, \
              state, result_id, intent_json, attempts, created_at, updated_at, last_error) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?12,?13)",
            params![
                op.op_id,
                op.account_id,
                op.service,
                op.op_kind,
                op.target_id,
                op.idempotency_key,
                op.if_match_etag,
                op.state,
                op.result_id,
                op.intent_json,
                op.attempts,
                now,
                op.last_error,
            ],
        )?;
        Ok(n > 0)
    }

    /// Fetch a ledger op by its dedup key (to recover/reuse a re-issued intent).
    pub fn cloud_write_by_key(
        &self,
        account: &str,
        idempotency_key: &str,
    ) -> Result<Option<CloudWriteOp>> {
        self.conn
            .query_row(
                &format!(
                    "SELECT {CLOUD_WRITE_COLS} FROM cloud_write_operations \
                     WHERE account_id=?1 AND idempotency_key=?2"
                ),
                params![account, idempotency_key],
                row_to_cloud_write,
            )
            .optional()
            .map_err(Into::into)
    }

    /// Advance a ledger op's state (`pending`→`inflight`→`applied|failed|conflict`),
    /// recording the resulting cloud id / error and bumping the attempt counter.
    pub fn set_cloud_write_state(
        &self,
        op_id: &str,
        state: &str,
        result_id: Option<&str>,
        last_error: Option<&str>,
        now: i64,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE cloud_write_operations \
             SET state=?2, result_id=COALESCE(?3, result_id), last_error=?4, \
                 attempts=attempts+1, updated_at=?5 \
             WHERE op_id=?1",
            params![op_id, state, result_id, last_error, now],
        )?;
        Ok(())
    }

    /// All not-yet-terminal ops for an account (`pending`/`inflight`) — the boot-recovery
    /// work-list. Each is re-probed per [`CloudOpKind`]'s recovery semantics.
    pub fn pending_cloud_writes(&self, account: &str) -> Result<Vec<CloudWriteOp>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {CLOUD_WRITE_COLS} FROM cloud_write_operations \
             WHERE account_id=?1 AND state IN ('pending','inflight') ORDER BY created_at"
        ))?;
        let rows = stmt.query_map(params![account], row_to_cloud_write)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Diagnostic export (#0E): the cloud-write ledger's state distribution for an account
    /// as `(state, count)` pairs. Secret-free by construction (only states + counts); a
    /// caller that also surfaces `last_error` strings redacts them via `core::obs::redact`
    /// (store is a low-level crate with no core dependency).
    pub fn cloud_write_ledger_summary(&self, account: &str) -> Result<Vec<(String, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT state, COUNT(*) FROM cloud_write_operations \
             WHERE account_id=?1 GROUP BY state ORDER BY state",
        )?;
        let rows = stmt.query_map(params![account], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn get_item(&self, account: &str, service: &str, remote_id: &str) -> Result<Option<Item>> {
        let it = self
            .conn
            .query_row(
                &format!(
                    "SELECT {COLS} FROM items WHERE account_id=?1 AND service=?2 AND remote_id=?3"
                ),
                params![account, service, remote_id],
                row_to_item,
            )
            .optional()?;
        Ok(it)
    }

    /// Direct children of a parent (`None` parent = service root). Excludes tombstones.
    pub fn children(
        &self,
        account: &str,
        service: &str,
        parent: Option<&str>,
    ) -> Result<Vec<Item>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {COLS} FROM items
             WHERE account_id=?1 AND service=?2 AND deleted_at IS NULL
               AND parent_remote_id IS ?3
             ORDER BY name"
        ))?;
        let rows = stmt.query_map(params![account, service, parent], row_to_item)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Top-level items of a service: those whose `parent_remote_id` is `NULL`
    /// **or** points at something that is not itself a tracked item (e.g. the
    /// OneDrive drive root, which is never stored as an item). Lets a file
    /// explorer show the top of the tree without knowing the drive-root id.
    /// Excludes tombstones; ordered by name.
    pub fn roots(&self, account: &str, service: &str) -> Result<Vec<Item>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {COLS} FROM items
             WHERE account_id=?1 AND service=?2 AND deleted_at IS NULL
               AND (parent_remote_id IS NULL OR parent_remote_id NOT IN
                    (SELECT remote_id FROM items
                     WHERE account_id=?1 AND service=?2 AND deleted_at IS NULL))
             ORDER BY name"
        ))?;
        let rows = stmt.query_map(params![account, service], row_to_item)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Mark an item as a tombstone (sets `deleted_at`, `sync_state='deleted'`).
    pub fn mark_deleted(
        &self,
        account: &str,
        service: &str,
        remote_id: &str,
        when: &str,
    ) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE items SET deleted_at=?4, sync_state='deleted'
             WHERE account_id=?1 AND service=?2 AND remote_id=?3",
            params![account, service, remote_id, when],
        )?;
        Ok(n > 0)
    }

    /// Persist a delta cursor, bumping its generation. Tokens are stored opaque.
    pub fn set_delta_cursor(
        &self,
        account: &str,
        service: &str,
        scope: &str,
        cursor: &str,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO delta_state (account_id, service, scope, cursor, generation)
             VALUES (?1,?2,?3,?4,1)
             ON CONFLICT(account_id, service, scope) DO UPDATE SET
               cursor = excluded.cursor,
               generation = delta_state.generation + 1",
            params![account, service, scope, cursor],
        )?;
        Ok(())
    }

    pub fn get_delta_cursor(
        &self,
        account: &str,
        service: &str,
        scope: &str,
    ) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT cursor FROM delta_state WHERE account_id=?1 AND service=?2 AND scope=?3",
                params![account, service, scope],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn delta_generation(
        &self,
        account: &str,
        service: &str,
        scope: &str,
    ) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row(
                "SELECT generation FROM delta_state WHERE account_id=?1 AND service=?2 AND scope=?3",
                params![account, service, scope],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Drop the delta cursor for one `(account, service, scope)`. Returns whether a
    /// row was removed. Used on a `410 Gone` resync (discard the stale token so a
    /// crash mid-resync restarts cleanly from the base delta URL) and to GC the
    /// cursor of a folder that is no longer scoped (sync/offline). Sibling scopes
    /// are untouched — the primary key is `(account_id, service, scope)`.
    pub fn clear_delta_cursor(&self, account: &str, service: &str, scope: &str) -> Result<bool> {
        let n = self.conn.execute(
            "DELETE FROM delta_state WHERE account_id=?1 AND service=?2 AND scope=?3",
            params![account, service, scope],
        )?;
        Ok(n > 0)
    }

    /// All non-deleted items of a service (any type), ordered by type then name.
    /// Drives the web UI's per-service listing.
    pub fn items_by_service(&self, account: &str, service: &str) -> Result<Vec<Item>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {COLS} FROM items
             WHERE account_id=?1 AND service=?2 AND deleted_at IS NULL
             ORDER BY item_type, name"
        ))?;
        let rows = stmt.query_map(params![account, service], row_to_item)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// One page of a service's non-deleted items (same stable order as
    /// [`Self::items_by_service`]), so a UI never has to load a whole large
    /// mailbox at once. `offset`/`limit` are clamped to non-negative by the
    /// caller's `u32` type.
    pub fn items_by_service_page(
        &self,
        account: &str,
        service: &str,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<Item>> {
        let mut stmt = self.conn.prepare(&format!(
            // include cloud-deleted items that still have an archived body so the
            // UI can show the 'backup-only' state (the backup's whole point);
            // truly-gone items (deleted + never archived) stay hidden.
            "SELECT {COLS} FROM items
             WHERE account_id=?1 AND service=?2
               AND (deleted_at IS NULL OR local_path IS NOT NULL)
             ORDER BY item_type, name
             LIMIT ?3 OFFSET ?4"
        ))?;
        let rows = stmt.query_map(params![account, service, limit, offset], row_to_item)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Count of a service's non-deleted items (the pagination total).
    pub fn count_by_service(&self, account: &str, service: &str) -> Result<u64> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM items
             WHERE account_id=?1 AND service=?2
               AND (deleted_at IS NULL OR local_path IS NOT NULL)",
            params![account, service],
            |r| r.get::<_, i64>(0),
        )? as u64)
    }

    /// All non-deleted items of a given `item_type` for a service, ordered by
    /// `remote_id` (stable). Used to drive content downloads (e.g. fetch the MIME
    /// for every stored mail `message`) and listings.
    pub fn items_by_type(
        &self,
        account: &str,
        service: &str,
        item_type: &str,
    ) -> Result<Vec<Item>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {COLS} FROM items
             WHERE account_id=?1 AND service=?2 AND item_type=?3 AND deleted_at IS NULL
             ORDER BY remote_id"
        ))?;
        let rows = stmt.query_map(params![account, service, item_type], row_to_item)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Set (or clear) an item's `local_path` — the on-disk location of its
    /// downloaded body. Returns whether a row matched.
    pub fn set_local_path(
        &self,
        account: &str,
        service: &str,
        remote_id: &str,
        local_path: Option<&str>,
    ) -> Result<bool> {
        // Record the cloud version this archived body corresponds to: when a body
        // is set, `body_etag` snapshots the item's current `etag`; when cleared, it
        // resets to NULL. The delta ingest never touches `body_etag`, so a later
        // `etag` change (cloud edit) makes `etag != body_etag` → a stale backup.
        //
        // Invalidate the cached `preview_json` ONLY when the body actually changed —
        // i.e. the current `etag` differs from the `body_etag` recorded at the last
        // archive (`IS NOT` so a first archive, body_etag=NULL, also refreshes). A sync
        // that re-archives an *unchanged* body (etag == body_etag) keeps the cache, so a
        // mailbox load during a sync stays on the fast path instead of re-parsing every
        // `.eml`. `CASE` reads the OLD row values, evaluated before the SET applies.
        let n = self.conn.execute(
            "UPDATE items
             SET preview_json=CASE WHEN etag IS NOT body_etag THEN NULL ELSE preview_json END,
                 local_path=?4,
                 body_etag=CASE WHEN ?4 IS NOT NULL THEN etag ELSE NULL END
             WHERE account_id=?1 AND service=?2 AND remote_id=?3",
            params![account, service, remote_id, local_path],
        )?;
        Ok(n > 0)
    }

    /// All live items for an account that have an archived body (`local_path`
    /// set), across every service — the verify pass's work-list. Folders are
    /// excluded: they have a path but no body to hash.
    pub fn items_with_body(&self, account: &str) -> Result<Vec<Item>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {COLS} FROM items
             WHERE account_id=?1 AND deleted_at IS NULL AND local_path IS NOT NULL
               AND item_type != 'folder'
             ORDER BY service, remote_id"
        ))?;
        let rows = stmt.query_map(params![account], row_to_item)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Record the outcome of a verify pass for one item: the integrity baseline
    /// hash (`sha`, SHA-256 of the archived body), the `status`
    /// (`verified`/`changed`/`failed`) and the timestamp `at` (RFC3339). Written
    /// only by the verify pass — never by the delta ingest. Returns whether a row
    /// matched.
    pub fn set_verify(
        &self,
        account: &str,
        service: &str,
        remote_id: &str,
        sha: Option<&str>,
        status: &str,
        at: &str,
    ) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE items SET body_sha256=?4, verify_status=?5, verified_at=?6
             WHERE account_id=?1 AND service=?2 AND remote_id=?3",
            params![account, service, remote_id, sha, status, at],
        )?;
        Ok(n > 0)
    }

    /// Aggregate integrity counts for an account: `(checked, verified)` where
    /// `checked` is the number of live items that have actually been through a
    /// verify pass (`verify_status` set — present bodies that were read + hashed;
    /// cloud-only OneDrive placeholders that the pass skips are excluded) and
    /// `verified` is how many of those last passed (`verify_status = 'verified'`).
    /// The web UI derives "Integrity verified N%" as `verified / checked`.
    pub fn verify_counts(&self, account: &str) -> Result<(i64, i64)> {
        let checked: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM items
             WHERE account_id=?1 AND deleted_at IS NULL AND verify_status IS NOT NULL",
            params![account],
            |r| r.get(0),
        )?;
        let verified: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM items
             WHERE account_id=?1 AND deleted_at IS NULL AND verify_status='verified'",
            params![account],
            |r| r.get(0),
        )?;
        Ok((checked, verified))
    }

    /// **Every** item of a service, including tombstones (`deleted_at` set). Used
    /// by local-delete materialization, which must both find tombstoned items and
    /// walk their (possibly also-deleted) ancestor chain to rebuild local paths.
    pub fn all_items_by_service(&self, account: &str, service: &str) -> Result<Vec<Item>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {COLS} FROM items
             WHERE account_id=?1 AND service=?2
             ORDER BY item_type, name"
        ))?;
        let rows = stmt.query_map(params![account, service], row_to_item)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Set an item's `sync_state` (e.g. mark a downloaded item `clean`). Returns
    /// whether a row matched.
    pub fn set_sync_state(
        &self,
        account: &str,
        service: &str,
        remote_id: &str,
        state: &str,
    ) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE items SET sync_state=?4
             WHERE account_id=?1 AND service=?2 AND remote_id=?3",
            params![account, service, remote_id, state],
        )?;
        Ok(n > 0)
    }

    /// Record the **last-synced on-disk reference** for an item: size, mtime and
    /// content hash of the local file as it was when last downloaded or uploaded.
    /// Set only by the materialize/upload paths — the delta ingest overwrites the
    /// item's metadata with new remote values, so this column trio is the only
    /// way a later pass can tell "stale but clean" from "locally edited".
    pub fn set_synced_state(
        &self,
        account: &str,
        service: &str,
        remote_id: &str,
        size: i64,
        mtime_unix: i64,
        hash: Option<&str>,
    ) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE items SET synced_size=?4, synced_mtime_unix=?5, synced_hash=?6
             WHERE account_id=?1 AND service=?2 AND remote_id=?3",
            params![account, service, remote_id, size, mtime_unix, hash],
        )?;
        Ok(n > 0)
    }

    /// The last-synced on-disk reference, if one was ever recorded
    /// (`(size, mtime_unix, hash)`); `None` for items from pre-v8 stores or that
    /// were never materialized/uploaded.
    pub fn get_synced_state(
        &self,
        account: &str,
        service: &str,
        remote_id: &str,
    ) -> Result<Option<(i64, i64, Option<String>)>> {
        let row = self
            .conn
            .query_row(
                "SELECT synced_size, synced_mtime_unix, synced_hash FROM items
                 WHERE account_id=?1 AND service=?2 AND remote_id=?3",
                params![account, service, remote_id],
                |r| {
                    Ok((
                        r.get::<_, Option<i64>>(0)?,
                        r.get::<_, Option<i64>>(1)?,
                        r.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .optional()?;
        Ok(match row {
            Some((Some(size), Some(mtime), hash)) => Some((size, mtime, hash)),
            _ => None,
        })
    }

    /// The `sync_state` of the live (non-tombstone) item at an on-disk path, or
    /// `None` if no item maps to that exact `local_path`. Used by the desktop
    /// DBus status provider to answer Dolphin overlay-icon queries by path.
    pub fn sync_state_for_local_path(&self, local_path: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT sync_state FROM items
                 WHERE local_path = ?1 AND deleted_at IS NULL
                 LIMIT 1",
                params![local_path],
                |r| r.get::<_, String>(0),
            )
            .optional()?)
    }

    /// Persist a resumable upload session for `dest_path` (plan §6/§9) so a process
    /// kill mid-upload can resume from the server instead of restarting.
    pub fn save_upload_session(
        &self,
        account: &str,
        service: &str,
        dest_path: &str,
        upload_url: &str,
        total: i64,
        next_offset: i64,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO upload_sessions
                 (account_id, service, dest_path, upload_url, total, next_offset)
               VALUES (?1,?2,?3,?4,?5,?6)
               ON CONFLICT(account_id, service, dest_path) DO UPDATE SET
                 upload_url  = excluded.upload_url,
                 total       = excluded.total,
                 next_offset = excluded.next_offset",
            params![account, service, dest_path, upload_url, total, next_offset],
        )?;
        Ok(())
    }

    /// The persisted upload session for `dest_path`, as `(upload_url, total)`, or
    /// `None` if there is no in-flight session.
    pub fn get_upload_session(
        &self,
        account: &str,
        service: &str,
        dest_path: &str,
    ) -> Result<Option<(String, u64)>> {
        Ok(self
            .conn
            .query_row(
                "SELECT upload_url, total FROM upload_sessions
                 WHERE account_id=?1 AND service=?2 AND dest_path=?3",
                params![account, service, dest_path],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64)),
            )
            .optional()?)
    }

    /// Remove a persisted upload session (called on completion or when abandoned).
    pub fn clear_upload_session(
        &self,
        account: &str,
        service: &str,
        dest_path: &str,
    ) -> Result<()> {
        self.conn.execute(
            "DELETE FROM upload_sessions
             WHERE account_id=?1 AND service=?2 AND dest_path=?3",
            params![account, service, dest_path],
        )?;
        Ok(())
    }

    // --- Cloud-restore operation ledger (ADR-001) ---

    /// Create a new restore operation in state `pending`, recording the first audit
    /// step. `idempotency_key` is unique per account — a second attempt to restore
    /// identical content collides at the database (returns a `Sqlite` error) rather
    /// than in the user's mailbox. `now` is unix seconds.
    pub fn create_restore_operation(
        &self,
        op_id: &str,
        account: &str,
        service: &str,
        source_item_id: &str,
        idempotency_key: &str,
        now: i64,
    ) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO restore_operations
                 (op_id, account_id, service, source_item_id, idempotency_key,
                  state, attempts, created_at, updated_at)
               VALUES (?1,?2,?3,?4,?5,'pending',0,?6,?6)",
            params![
                op_id,
                account,
                service,
                source_item_id,
                idempotency_key,
                now
            ],
        )?;
        tx.execute(
            "INSERT INTO restore_steps (op_id, seq, from_state, to_state, at, detail)
               VALUES (?1, 1, NULL, 'pending', ?2, 'created')",
            params![op_id, now],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Fetch one restore operation by id.
    pub fn get_restore_operation(&self, op_id: &str) -> Result<Option<RestoreOperation>> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {RESTORE_OP_COLS} FROM restore_operations WHERE op_id=?1"),
                params![op_id],
                map_restore_op,
            )
            .optional()?)
    }

    /// Apply a state transition, enforcing the recovery-safe state machine and
    /// appending an audit step. Optionally persists the created cloud id and/or the
    /// marker (each only overwrites when `Some`). Rejects an illegal transition with
    /// [`StoreError::IllegalTransition`] and makes no change. Entering `committing`
    /// increments `attempts`. `now` is unix seconds.
    pub fn transition_restore(
        &self,
        op_id: &str,
        to: RestoreState,
        now: i64,
        detail: Option<&str>,
        new_cloud_id: Option<&str>,
        marker: Option<&str>,
    ) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        let cur: String = tx.query_row(
            "SELECT state FROM restore_operations WHERE op_id=?1",
            params![op_id],
            |r| r.get(0),
        )?;
        let from = RestoreState::parse(&cur).ok_or_else(|| {
            StoreError::IllegalTransition(format!("unknown stored state '{cur}' for {op_id}"))
        })?;
        if !from.can_transition_to(to) {
            return Err(StoreError::IllegalTransition(format!(
                "{} -> {} ({op_id})",
                from.as_str(),
                to.as_str()
            )));
        }
        let bump: i64 = if to == RestoreState::Committing { 1 } else { 0 };
        tx.execute(
            "UPDATE restore_operations
                SET state=?2, updated_at=?3, attempts=attempts+?4,
                    new_cloud_id=COALESCE(?5,new_cloud_id),
                    marker=COALESCE(?6,marker)
              WHERE op_id=?1",
            params![op_id, to.as_str(), now, bump, new_cloud_id, marker],
        )?;
        let seq: i64 = tx.query_row(
            "SELECT COALESCE(MAX(seq),0)+1 FROM restore_steps WHERE op_id=?1",
            params![op_id],
            |r| r.get(0),
        )?;
        tx.execute(
            "INSERT INTO restore_steps (op_id, seq, from_state, to_state, at, detail)
               VALUES (?1,?2,?3,?4,?5,?6)",
            params![op_id, seq, from.as_str(), to.as_str(), now, detail],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Record the most recent error message on an operation (does not change state).
    pub fn set_restore_error(&self, op_id: &str, error: &str, now: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE restore_operations SET last_error=?2, updated_at=?3 WHERE op_id=?1",
            params![op_id, error, now],
        )?;
        Ok(())
    }

    /// Try to take the operation's lease for `owner`, valid for `ttl_secs`. Succeeds
    /// only if no lease is held or the current one has expired (`<= now`). Returns
    /// whether the lease was acquired. `now` is unix seconds.
    pub fn acquire_restore_lease(
        &self,
        op_id: &str,
        owner: &str,
        now: i64,
        ttl_secs: i64,
    ) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE restore_operations
                SET lease_owner=?2, lease_expires_at=?3
              WHERE op_id=?1
                AND (lease_owner IS NULL OR lease_expires_at IS NULL OR lease_expires_at <= ?4)",
            params![op_id, owner, now + ttl_secs, now],
        )?;
        Ok(n == 1)
    }

    /// Release the lease on an operation.
    pub fn release_restore_lease(&self, op_id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE restore_operations SET lease_owner=NULL, lease_expires_at=NULL WHERE op_id=?1",
            params![op_id],
        )?;
        Ok(())
    }

    /// All non-terminal restore operations for an account, oldest first — the set the
    /// daemon must drive to a terminal state on startup (auto-recovery).
    pub fn recoverable_restore_operations(&self, account: &str) -> Result<Vec<RestoreOperation>> {
        let sql = format!(
            "SELECT {RESTORE_OP_COLS} FROM restore_operations
              WHERE account_id=?1 AND state NOT IN ('committed','cancelled')
              ORDER BY created_at, op_id"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params![account], map_restore_op)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// The ordered audit trail of an operation as `(from_state, to_state)` pairs.
    pub fn restore_history(&self, op_id: &str) -> Result<Vec<(Option<String>, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT from_state, to_state FROM restore_steps WHERE op_id=?1 ORDER BY seq",
        )?;
        let rows = stmt.query_map(params![op_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Record a completed engine run (one sync/backup/… pass) in the activity
    /// history. Returns the new run id.
    pub fn add_run(
        &self,
        account: &str,
        kind: &str,
        started_at: &str,
        finished_at: &str,
        status: &str,
        summary: &str,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO runs(account_id, kind, started_at, finished_at, status, summary)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![account, kind, started_at, finished_at, status, summary],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// The most recent runs for an account, newest first (the activity history).
    pub fn recent_runs(&self, account: &str, limit: u32) -> Result<Vec<Run>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, account_id, kind, started_at, finished_at, status, summary
             FROM runs WHERE account_id=?1 ORDER BY id DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![account, limit], |r| {
            Ok(Run {
                id: r.get(0)?,
                account_id: r.get(1)?,
                kind: r.get(2)?,
                started_at: r.get(3)?,
                finished_at: r.get(4)?,
                status: r.get(5)?,
                summary: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// All non-deleted remote ids for a service. Used by delta-less connectors
    /// (e.g. OneNote) to reconcile deletions: ids present here but absent from a
    /// fresh full list have been removed remotely.
    pub fn live_remote_ids(&self, account: &str, service: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT remote_id FROM items
             WHERE account_id=?1 AND service=?2 AND deleted_at IS NULL",
        )?;
        let rows = stmt.query_map(params![account, service], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Full-text search over file names within an account (FTS5 MATCH).
    pub fn search_names(&self, account: &str, query: &str) -> Result<Vec<Item>> {
        let cols_q = COLS
            .split(", ")
            .map(|c| format!("items.{}", c.trim()))
            .collect::<Vec<_>>()
            .join(", ");
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {cols_q} FROM items
             JOIN items_fts ON items_fts.rowid = items.id
             WHERE items.account_id=?1 AND items.deleted_at IS NULL AND items_fts MATCH ?2
             ORDER BY rank"
        ))?;
        let rows = stmt.query_map(params![account, query], row_to_item)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Index (or re-index) an item's extracted body text for full-text search
    /// (plan §9). Idempotent per `(account, service, remote_id)`.
    pub fn index_body(
        &self,
        account: &str,
        service: &str,
        remote_id: &str,
        body: &str,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO bodies (account_id, service, remote_id, body)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(account_id, service, remote_id) DO UPDATE SET body = excluded.body",
            params![account, service, remote_id, body],
        )?;
        Ok(())
    }

    /// Full-text search over indexed bodies within an account. Returns matching
    /// `(service, remote_id)` pairs, best-ranked first.
    pub fn search_bodies(&self, account: &str, query: &str) -> Result<Vec<(String, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT b.service, b.remote_id
             FROM bodies_fts f JOIN bodies b ON b.id = f.rowid
             WHERE bodies_fts MATCH ?2 AND b.account_id = ?1
             ORDER BY rank",
        )?;
        let rows = stmt.query_map(params![account, query], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}

/// A process-installed SQLCipher key — the Android Keystore-unwrapped key on mobile —
/// taking precedence over the env/keyring sources so the standalone app opens SQLCipher with
/// a hardware-backed key and no env vars. Set once at startup via [`set_store_key`]. Always
/// present so mobile can install the key regardless of the compiled store profile; only the
/// (encrypted-store) `configured_store_key` reads it.
static INSTALLED_STORE_KEY: std::sync::OnceLock<std::sync::Mutex<Option<Vec<u8>>>> =
    std::sync::OnceLock::new();

/// Install the process store key (mobile: from the Keystore, #0B). Overrides the env/keyring
/// sources for every subsequent [`Store::open`]. The key stays in process memory only.
pub fn set_store_key(secret: Vec<u8>) {
    let cell = INSTALLED_STORE_KEY.get_or_init(|| std::sync::Mutex::new(None));
    *cell.lock().unwrap_or_else(|e| e.into_inner()) = Some(secret);
}

#[cfg(feature = "encrypted-store")]
fn installed_store_key() -> Option<Vec<u8>> {
    INSTALLED_STORE_KEY
        .get()
        .and_then(|c| c.lock().unwrap_or_else(|e| e.into_inner()).clone())
}

#[cfg(feature = "encrypted-store")]
pub fn configured_store_key() -> Result<Option<Vec<u8>>> {
    if let Some(k) = installed_store_key() {
        return Ok(Some(k)); // process-installed (mobile Keystore) wins
    }
    if let Some(path) = std::env::var_os(STORE_KEY_FILE_ENV) {
        return read_store_secret_file(Path::new(&path)).map(Some);
    }
    if let Some(dir) = std::env::var_os(SYSTEMD_CREDENTIALS_DIR_ENV) {
        let path = PathBuf::from(dir).join(STORE_SYSTEMD_CREDENTIAL);
        if path.exists() {
            return read_store_secret_file(&path).map(Some);
        }
    }
    match std::env::var(STORE_KEY_ENV) {
        Ok(s) if !s.is_empty() => Ok(Some(s.into_bytes())),
        _ => Ok(None),
    }
}

pub fn read_store_secret_file(path: &Path) -> Result<Vec<u8>> {
    let bytes = std::fs::read(path)?;
    let trimmed = trim_ascii_whitespace(&bytes);
    if trimmed.is_empty() {
        return Err(StoreError::InvalidStoreSecret(format!(
            "{} is empty",
            path.display()
        )));
    }
    Ok(trimmed.to_vec())
}

fn trim_ascii_whitespace(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|b| !b.is_ascii_whitespace())
        .map(|idx| idx + 1)
        .unwrap_or(start);
    &bytes[start..end]
}

#[cfg(feature = "encrypted-store")]
fn apply_sqlcipher_key(conn: &Connection, secret: &[u8]) -> Result<()> {
    let len: i32 = secret
        .len()
        .try_into()
        .map_err(|_| StoreError::InvalidStoreSecret("secret is too large".into()))?;
    let rc = unsafe { rusqlite::ffi::sqlite3_key(conn.handle(), secret.as_ptr().cast(), len) };
    if rc == rusqlite::ffi::SQLITE_OK {
        Ok(())
    } else {
        Err(StoreError::Sqlite(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rc),
            Some("apply SQLCipher key failed".into()),
        )))
    }
}

/// Whether the file at `path` starts with the plaintext SQLite magic. A
/// SQLCipher-encrypted database encrypts the header too, so this distinguishes a
/// plaintext store from an encrypted one without a key.
#[cfg(feature = "encrypted-store")]
fn is_plaintext_sqlite(path: &Path) -> Result<bool> {
    use std::io::Read;
    let mut f = File::open(path)?;
    let mut magic = [0u8; 16];
    match f.read_exact(&mut magic) {
        Ok(()) => Ok(&magic == b"SQLite format 3\0"),
        // shorter than a header: not a valid SQLite file either way
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Ok(false),
        Err(e) => Err(e.into()),
    }
}

const COLS: &str = "account_id, service, remote_id, parent_remote_id, name, local_path, \
                    item_type, etag, ctag, quickxorhash, size, remote_mtime, sync_state, deleted_at, \
                    change_key, internet_message_id, ical_uid, series_master_id, \
                    body_sha256, verified_at, verify_status, body_etag, sender, preview_json, \
                    content_state, body_location, body_state, plaintext_size, plaintext_hash, \
                    encrypted_blob_version, materialized_at, last_download_error, conflict_state";

fn row_to_item(r: &rusqlite::Row) -> rusqlite::Result<Item> {
    Ok(Item {
        account_id: r.get(0)?,
        service: r.get(1)?,
        remote_id: r.get(2)?,
        parent_remote_id: r.get(3)?,
        name: r.get(4)?,
        local_path: r.get(5)?,
        item_type: r.get(6)?,
        etag: r.get(7)?,
        ctag: r.get(8)?,
        quickxorhash: r.get(9)?,
        size: r.get(10)?,
        remote_mtime: r.get(11)?,
        sync_state: r.get(12)?,
        deleted_at: r.get(13)?,
        change_key: r.get(14)?,
        internet_message_id: r.get(15)?,
        ical_uid: r.get(16)?,
        series_master_id: r.get(17)?,
        body_sha256: r.get(18)?,
        verified_at: r.get(19)?,
        verify_status: r.get(20)?,
        body_etag: r.get(21)?,
        sender: r.get(22)?,
        preview_json: r.get(23)?,
        content_state: r.get(24)?,
        body_location: r.get(25)?,
        body_state: r.get(26)?,
        plaintext_size: r.get(27)?,
        plaintext_hash: r.get(28)?,
        encrypted_blob_version: r.get(29)?,
        materialized_at: r.get(30)?,
        last_download_error: r.get(31)?,
        conflict_state: r.get(32)?,
    })
}

const CLOUD_WRITE_COLS: &str = "op_id, account_id, service, op_kind, target_id, \
    idempotency_key, if_match_etag, state, result_id, intent_json, attempts, last_error";

fn row_to_cloud_write(r: &rusqlite::Row) -> rusqlite::Result<CloudWriteOp> {
    Ok(CloudWriteOp {
        op_id: r.get(0)?,
        account_id: r.get(1)?,
        service: r.get(2)?,
        op_kind: r.get(3)?,
        target_id: r.get(4)?,
        idempotency_key: r.get(5)?,
        if_match_etag: r.get(6)?,
        state: r.get(7)?,
        result_id: r.get(8)?,
        intent_json: r.get(9)?,
        attempts: r.get(10)?,
        last_error: r.get(11)?,
    })
}

/// The state of one cloud-restore operation in the ledger (ADR-001).
///
/// Legal transitions form the recovery-safe machine:
/// `Pending → PreflightChecked → Committing → Committed | FailedAfterGraphCommit`,
/// with `FailedAfterGraphCommit` reconciling to `Committed` or resuming to
/// `Committing` (never a blind retry), and `Pending`/`PreflightChecked` able to
/// `Cancelled`. `Committed` and `Cancelled` are terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreState {
    Pending,
    PreflightChecked,
    Committing,
    Committed,
    FailedAfterGraphCommit,
    Cancelled,
}

impl RestoreState {
    /// Stable string used in the database.
    pub fn as_str(self) -> &'static str {
        match self {
            RestoreState::Pending => "pending",
            RestoreState::PreflightChecked => "preflight_checked",
            RestoreState::Committing => "committing",
            RestoreState::Committed => "committed",
            RestoreState::FailedAfterGraphCommit => "failed_after_graph_commit",
            RestoreState::Cancelled => "cancelled",
        }
    }

    /// Parse the database string back to a state.
    pub fn parse(s: &str) -> Option<RestoreState> {
        Some(match s {
            "pending" => RestoreState::Pending,
            "preflight_checked" => RestoreState::PreflightChecked,
            "committing" => RestoreState::Committing,
            "committed" => RestoreState::Committed,
            "failed_after_graph_commit" => RestoreState::FailedAfterGraphCommit,
            "cancelled" => RestoreState::Cancelled,
            _ => return None,
        })
    }

    /// Terminal states accept no further transitions.
    pub fn is_terminal(self) -> bool {
        matches!(self, RestoreState::Committed | RestoreState::Cancelled)
    }

    /// Whether `self → to` is a legal transition in the recovery-safe machine.
    pub fn can_transition_to(self, to: RestoreState) -> bool {
        use RestoreState::*;
        matches!(
            (self, to),
            (Pending, PreflightChecked)
                | (Pending, Cancelled)
                | (PreflightChecked, Committing)
                | (PreflightChecked, Cancelled)
                | (Committing, Committed)
                | (Committing, FailedAfterGraphCommit)
                | (FailedAfterGraphCommit, Committed)
                | (FailedAfterGraphCommit, Committing)
        )
    }
}

/// One row of the cloud-restore operation ledger (ADR-001).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreOperation {
    pub op_id: String,
    pub account_id: String,
    pub service: String,
    pub source_item_id: String,
    pub idempotency_key: String,
    pub state: RestoreState,
    /// The created cloud id, once a commit is confirmed.
    pub new_cloud_id: Option<String>,
    /// Service-native id / stamped probe token used only to *find* a possibly
    /// created item during reconciliation (the ledger row stays authoritative).
    pub marker: Option<String>,
    pub attempts: i64,
    pub lease_owner: Option<String>,
    pub lease_expires_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
    pub last_error: Option<String>,
}

/// Column list shared by every `restore_operations` SELECT, in `map_restore_op` order.
const RESTORE_OP_COLS: &str = "op_id, account_id, service, source_item_id, \
     idempotency_key, state, new_cloud_id, marker, attempts, lease_owner, \
     lease_expires_at, created_at, updated_at, last_error";

fn map_restore_op(r: &rusqlite::Row) -> rusqlite::Result<RestoreOperation> {
    let state_s: String = r.get(5)?;
    let state = RestoreState::parse(&state_s).ok_or_else(|| {
        rusqlite::Error::InvalidColumnType(5, "state".into(), rusqlite::types::Type::Text)
    })?;
    Ok(RestoreOperation {
        op_id: r.get(0)?,
        account_id: r.get(1)?,
        service: r.get(2)?,
        source_item_id: r.get(3)?,
        idempotency_key: r.get(4)?,
        state,
        new_cloud_id: r.get(6)?,
        marker: r.get(7)?,
        attempts: r.get(8)?,
        lease_owner: r.get(9)?,
        lease_expires_at: r.get(10)?,
        created_at: r.get(11)?,
        updated_at: r.get(12)?,
        last_error: r.get(13)?,
    })
}

fn migrate(conn: &Connection) -> Result<()> {
    let v: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if v < 1 {
        conn.execute_batch(MIGRATION_V1)?;
    }
    if v < 2 {
        conn.execute_batch(MIGRATION_V2)?;
    }
    if v < 3 {
        conn.execute_batch(MIGRATION_V3)?;
    }
    if v < 4 {
        conn.execute_batch(MIGRATION_V4)?;
    }
    if v < 5 {
        conn.execute_batch(MIGRATION_V5)?;
    }
    if v < 6 {
        conn.execute_batch(MIGRATION_V6)?;
    }
    if v < 7 {
        conn.execute_batch(MIGRATION_V7)?;
    }
    if v < 8 {
        conn.execute_batch(MIGRATION_V8)?;
    }
    if v < 9 {
        conn.execute_batch(MIGRATION_V9)?;
    }
    if v < 10 {
        conn.execute_batch(MIGRATION_V10)?;
    }
    if v < 11 {
        conn.execute_batch(MIGRATION_V11)?;
    }
    if v < 12 {
        conn.execute_batch(MIGRATION_V12)?;
    }
    if v < 13 {
        conn.execute_batch(MIGRATION_V13)?;
    }
    if v < 14 {
        conn.execute_batch(MIGRATION_V14)?;
    }
    if v < 15 {
        conn.execute_batch(MIGRATION_V15)?;
    }
    conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(acc: &str, id: &str, name: &str) -> Item {
        Item::new(acc, "onedrive", id, name, "file")
    }

    #[test]
    fn migrates_to_current_version() {
        let s = Store::open_in_memory().unwrap();
        assert_eq!(s.schema_version().unwrap(), SCHEMA_VERSION);
    }

    #[test]
    fn set_local_path_records_body_etag_for_stale_detection() {
        let s = Store::open_in_memory().unwrap();
        let mut a = item("acc", "id-a", "A.json");
        a.etag = Some("E1".into());
        s.upsert_item(&a).unwrap();
        // no body yet → no body_etag
        let g = s.get_item("acc", "onedrive", "id-a").unwrap().unwrap();
        assert_eq!(g.body_etag, None);
        // archive the body → body_etag snapshots the current etag
        s.set_local_path("acc", "onedrive", "id-a", Some("ct/aa/id-a.json"))
            .unwrap();
        let g = s.get_item("acc", "onedrive", "id-a").unwrap().unwrap();
        assert_eq!(g.body_etag.as_deref(), Some("E1"));
        // a cloud edit (delta ingest) overwrites etag but NOT body_etag → stale
        let mut a2 = item("acc", "id-a", "A.json");
        a2.etag = Some("E2".into());
        a2.local_path = Some("ct/aa/id-a.json".into());
        s.upsert_item(&a2).unwrap();
        let g = s.get_item("acc", "onedrive", "id-a").unwrap().unwrap();
        assert_eq!(g.etag.as_deref(), Some("E2"));
        assert_eq!(
            g.body_etag.as_deref(),
            Some("E1"),
            "stale: cloud etag moved but the archived body's etag is pinned"
        );
        // clearing the body resets body_etag
        s.set_local_path("acc", "onedrive", "id-a", None).unwrap();
        assert_eq!(
            s.get_item("acc", "onedrive", "id-a")
                .unwrap()
                .unwrap()
                .body_etag,
            None
        );
    }

    #[test]
    fn preview_json_roundtrip_and_body_change_invalidation() {
        let s = Store::open_in_memory().unwrap();
        let mut m = Item::new("acc", "mail", "m1", "Subject", "message");
        m.etag = Some("E1".into());
        s.upsert_item(&m).unwrap();
        // freshly ingested: no cached preview
        assert_eq!(
            s.get_item("acc", "mail", "m1").unwrap().unwrap().preview_json,
            None
        );
        // the read path caches the computed preview
        s.set_preview_json("acc", "mail", "m1", r#"{"from":"a@b.c"}"#)
            .unwrap();
        assert_eq!(
            s.get_item("acc", "mail", "m1")
                .unwrap()
                .unwrap()
                .preview_json
                .as_deref(),
            Some(r#"{"from":"a@b.c"}"#)
        );
        // a metadata-only re-sync (upsert) must PRESERVE the cached preview
        let mut m2 = Item::new("acc", "mail", "m1", "Subject edited", "message");
        m2.etag = Some("E1".into());
        s.upsert_item(&m2).unwrap();
        assert_eq!(
            s.get_item("acc", "mail", "m1")
                .unwrap()
                .unwrap()
                .preview_json
                .as_deref(),
            Some(r#"{"from":"a@b.c"}"#),
            "a metadata upsert must not clobber the cached preview"
        );
        // first archive (body_etag was NULL) refreshes the cache → recompute next read
        s.set_local_path("acc", "mail", "m1", Some("mail/aa/m1.eml"))
            .unwrap();
        assert_eq!(
            s.get_item("acc", "mail", "m1").unwrap().unwrap().preview_json,
            None,
            "the first body archive drops any stale cached preview"
        );
        // re-cache, then a sync re-archives the SAME body (etag unchanged, still E1 ==
        // body_etag): the cache MUST be kept so a mailbox load during a sync stays on the
        // fast path instead of re-parsing the .eml. This is the cold-start-hang fix.
        s.set_preview_json("acc", "mail", "m1", r#"{"from":"x"}"#)
            .unwrap();
        s.set_local_path("acc", "mail", "m1", Some("mail/aa/m1.eml"))
            .unwrap();
        assert_eq!(
            s.get_item("acc", "mail", "m1")
                .unwrap()
                .unwrap()
                .preview_json
                .as_deref(),
            Some(r#"{"from":"x"}"#),
            "re-archiving an unchanged body must keep the cached preview"
        );
        // a cloud edit (new etag E2) then re-archive: the body changed → cache dropped
        let mut m3 = Item::new("acc", "mail", "m1", "Subject", "message");
        m3.etag = Some("E2".into());
        s.upsert_item(&m3).unwrap();
        s.set_local_path("acc", "mail", "m1", Some("mail/aa/m1.eml"))
            .unwrap();
        assert_eq!(
            s.get_item("acc", "mail", "m1").unwrap().unwrap().preview_json,
            None,
            "a changed body (new etag) must invalidate the cached preview"
        );
    }

    #[test]
    fn onedrive_content_state_roundtrips_and_backfills_available() {
        // Schema v14: OneDrive per-item body state. Fresh rows start with no state
        // (Mode 1 online, nothing cached); the download/materialize path transitions
        // them to available; a metadata re-upsert must not clobber that state.
        let s = Store::open_in_memory().unwrap();
        assert_eq!(s.schema_version().unwrap(), SCHEMA_VERSION);
        let it = Item::new("a", "onedrive", "f1", "file.txt", "file");
        s.upsert_item(&it).unwrap();
        let g = s.get_item("a", "onedrive", "f1").unwrap().unwrap();
        assert_eq!(g.body_state, None);
        assert_eq!(g.content_state, None);
        // transition to a materialized/offline body
        s.set_content_state(
            "a",
            "onedrive",
            "f1",
            Some("materialized"),
            Some("sync"),
            Some("available"),
            Some("2026-07-02T00:00:00Z"),
        )
        .unwrap();
        let g = s.get_item("a", "onedrive", "f1").unwrap().unwrap();
        assert_eq!(g.content_state.as_deref(), Some("materialized"));
        assert_eq!(g.body_location.as_deref(), Some("sync"));
        assert_eq!(g.body_state.as_deref(), Some("available"));
        assert_eq!(g.materialized_at.as_deref(), Some("2026-07-02T00:00:00Z"));
        // a metadata-only re-upsert (rename) must PRESERVE the content state
        s.upsert_item(&Item::new("a", "onedrive", "f1", "renamed.txt", "file"))
            .unwrap();
        assert_eq!(
            s.get_item("a", "onedrive", "f1")
                .unwrap()
                .unwrap()
                .body_state
                .as_deref(),
            Some("available"),
            "a metadata upsert must not clobber OneDrive content state"
        );

        // Backfill (MIGRATION_V14): an existing OneDrive body present on disk pre-v14
        // (a set `local_path`) is materialized/available. Simulate the pre-migration
        // shape by clearing the state, then re-running the same backfill statements.
        s.conn
            .execute_batch(
                "UPDATE items SET content_state=NULL, body_location=NULL, body_state=NULL WHERE remote_id='f1';
                 UPDATE items SET local_path='onedrive/aa/f1' WHERE remote_id='f1';
                 UPDATE items SET content_state='materialized', body_location='sync', body_state='available'
                   WHERE service='onedrive' AND item_type != 'folder' AND local_path IS NOT NULL;",
            )
            .unwrap();
        assert_eq!(
            s.get_item("a", "onedrive", "f1")
                .unwrap()
                .unwrap()
                .body_state
                .as_deref(),
            Some("available"),
            "backfill must mark an existing OneDrive body as available"
        );
    }

    #[test]
    fn cloud_write_ledger_is_idempotent_and_recovers_pending() {
        // #0D: a mutating OneDrive op is recorded idempotently before it hits Graph, so a
        // crash mid-op is recoverable and a re-issued intent never causes a second effect.
        let s = Store::open_in_memory().unwrap();
        assert_eq!(s.schema_version().unwrap(), 15);
        let op = CloudWriteOp {
            op_id: "op1".into(),
            account_id: "a".into(),
            service: "onedrive".into(),
            op_kind: CloudOpKind::Delete.as_str().into(),
            target_id: Some("f1".into()),
            idempotency_key: "del-f1-E1".into(),
            if_match_etag: Some("E1".into()),
            state: "pending".into(),
            result_id: None,
            intent_json: None,
            attempts: 0,
            last_error: None,
        };
        assert!(s.record_cloud_write(&op, 100).unwrap(), "first record inserts");
        // re-issue the SAME intent (same idempotency_key, different op_id) → deduped
        let mut dup = op.clone();
        dup.op_id = "op2".into();
        assert!(
            !s.record_cloud_write(&dup, 101).unwrap(),
            "same idempotency_key must dedup — no second cloud op"
        );
        // exactly one pending op recoverable at boot
        let pending = s.pending_cloud_writes("a").unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].op_id, "op1");
        assert_eq!(pending[0].op_kind, "delete");
        assert_eq!(
            s.cloud_write_by_key("a", "del-f1-E1").unwrap().unwrap().op_id,
            "op1"
        );
        // advance to applied → leaves the pending work-list, records result + attempt
        s.set_cloud_write_state("op1", "applied", Some("f1"), None, 200)
            .unwrap();
        assert!(s.pending_cloud_writes("a").unwrap().is_empty());
        let done = s.cloud_write_by_key("a", "del-f1-E1").unwrap().unwrap();
        assert_eq!(done.state, "applied");
        assert_eq!(done.result_id.as_deref(), Some("f1"));
        assert_eq!(done.attempts, 1);
        // recovery-probe semantics: only delete is safe to blindly re-send
        assert!(CloudOpKind::Delete.is_blind_replay_safe());
        assert!(!CloudOpKind::Create.is_blind_replay_safe());
        assert!(!CloudOpKind::Replace.is_blind_replay_safe());
        assert_eq!(CloudOpKind::parse("share"), Some(CloudOpKind::Share));
        assert_eq!(CloudOpKind::parse("bogus"), None);
    }

    #[test]
    fn cloud_write_ledger_summary_counts_by_state() {
        // Diagnostic export (#0E): state distribution for a support dump.
        let s = Store::open_in_memory().unwrap();
        let mk = |id: &str, key: &str, state: &str| CloudWriteOp {
            op_id: id.into(),
            account_id: "a".into(),
            service: "onedrive".into(),
            op_kind: "delete".into(),
            target_id: None,
            idempotency_key: key.into(),
            if_match_etag: None,
            state: state.into(),
            result_id: None,
            intent_json: None,
            attempts: 0,
            last_error: None,
        };
        s.record_cloud_write(&mk("o1", "k1", "pending"), 1).unwrap();
        s.record_cloud_write(&mk("o2", "k2", "pending"), 1).unwrap();
        s.record_cloud_write(&mk("o3", "k3", "applied"), 1).unwrap();
        let sum = s.cloud_write_ledger_summary("a").unwrap();
        assert_eq!(
            sum,
            vec![("applied".to_string(), 1), ("pending".to_string(), 2)]
        );
        assert!(
            s.cloud_write_ledger_summary("other").unwrap().is_empty(),
            "summary is per-account"
        );
    }

    #[test]
    fn set_verify_roundtrip_and_counts() {
        let s = Store::open_in_memory().unwrap();
        // two items with an archived body, one without
        let mut a = item("acc", "id-a", "A.json");
        a.local_path = Some("ct/aa/id-a.json".into());
        let mut b = item("acc", "id-b", "B.json");
        b.local_path = Some("ct/bb/id-b.json".into());
        let c = item("acc", "id-c", "C"); // no body
        for it in [&a, &b, &c] {
            s.upsert_item(it).unwrap();
        }
        // fresh items: never verified → nothing checked yet
        let got = s.get_item("acc", "onedrive", "id-a").unwrap().unwrap();
        assert_eq!(got.verify_status, None);
        assert_eq!(got.body_sha256, None);
        assert_eq!(s.verify_counts("acc").unwrap(), (0, 0)); // nothing verified yet

        // baseline a, fail b
        assert!(s
            .set_verify(
                "acc",
                "onedrive",
                "id-a",
                Some("abc123"),
                "verified",
                "2026-06-16T00:00:00Z"
            )
            .unwrap());
        assert!(s
            .set_verify(
                "acc",
                "onedrive",
                "id-b",
                None,
                "failed",
                "2026-06-16T00:00:00Z"
            )
            .unwrap());
        let ga = s.get_item("acc", "onedrive", "id-a").unwrap().unwrap();
        assert_eq!(ga.verify_status.as_deref(), Some("verified"));
        assert_eq!(ga.body_sha256.as_deref(), Some("abc123"));
        assert_eq!(ga.verified_at.as_deref(), Some("2026-06-16T00:00:00Z"));
        assert_eq!(s.verify_counts("acc").unwrap(), (2, 1)); // 1 of 2 verified

        // a re-ingest (delta upsert) must NOT clear the verify baseline
        s.upsert_item(&a).unwrap();
        let ga2 = s.get_item("acc", "onedrive", "id-a").unwrap().unwrap();
        assert_eq!(ga2.verify_status.as_deref(), Some("verified"));
        assert_eq!(ga2.body_sha256.as_deref(), Some("abc123"));
    }

    #[test]
    fn runs_record_and_list_newest_first() {
        let s = Store::open_in_memory().unwrap();
        s.add_run("a", "sync", "t1", "t2", "ok", "1 up").unwrap();
        s.add_run("a", "backup", "t3", "t4", "ok", "mail").unwrap();
        s.add_run("b", "sync", "t5", "t6", "error", "boom").unwrap();
        let runs = s.recent_runs("a", 10).unwrap();
        assert_eq!(runs.len(), 2, "only account a's runs");
        // newest first
        assert_eq!(runs[0].kind, "backup");
        assert_eq!(runs[0].summary, "mail");
        assert_eq!(runs[1].kind, "sync");
        assert_eq!(runs[1].status, "ok");
        // limit is honored
        assert_eq!(s.recent_runs("a", 1).unwrap().len(), 1);
        assert_eq!(s.recent_runs("b", 10).unwrap()[0].status, "error");
    }

    #[test]
    fn upsert_then_get_roundtrips() {
        let s = Store::open_in_memory().unwrap();
        let mut it = item("a", "r1", "Photo.jpg");
        it.quickxorhash = Some("abc=".into());
        it.size = Some(1234);
        s.upsert_item(&it).unwrap();
        assert_eq!(s.get_item("a", "onedrive", "r1").unwrap(), Some(it));
        assert_eq!(s.get_item("a", "onedrive", "missing").unwrap(), None);
    }

    #[test]
    fn upsert_updates_in_place() {
        let s = Store::open_in_memory().unwrap();
        s.upsert_item(&item("a", "r1", "old.txt")).unwrap();
        let mut changed = item("a", "r1", "new.txt");
        changed.etag = Some("e2".into());
        s.upsert_item(&changed).unwrap();
        let got = s.get_item("a", "onedrive", "r1").unwrap().unwrap();
        assert_eq!(got.name, "new.txt");
        assert_eq!(got.etag.as_deref(), Some("e2"));
        // still a single row
        assert_eq!(s.search_names("a", "new").unwrap().len(), 1);
    }

    #[test]
    fn children_by_parent_excludes_tombstones() {
        let s = Store::open_in_memory().unwrap();
        let mut c1 = item("a", "c1", "b.txt");
        c1.parent_remote_id = Some("root".into());
        let mut c2 = item("a", "c2", "a.txt");
        c2.parent_remote_id = Some("root".into());
        s.upsert_item(&c1).unwrap();
        s.upsert_item(&c2).unwrap();
        let kids = s.children("a", "onedrive", Some("root")).unwrap();
        assert_eq!(
            kids.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(),
            ["a.txt", "b.txt"]
        );
        s.mark_deleted("a", "onedrive", "c1", "2026-06-02T00:00:00Z")
            .unwrap();
        assert_eq!(s.children("a", "onedrive", Some("root")).unwrap().len(), 1);
    }

    #[test]
    fn roots_returns_items_whose_parent_is_not_tracked() {
        let s = Store::open_in_memory().unwrap();
        // "DR" is the (untracked) drive root; F1 + top.txt hang off it.
        let mut f1 = item("a", "F1", "Folder One");
        f1.item_type = "folder".into();
        f1.parent_remote_id = Some("DR".into());
        let mut top = item("a", "top", "top.txt");
        top.parent_remote_id = Some("DR".into());
        let mut child = item("a", "child", "nested.txt");
        child.parent_remote_id = Some("F1".into()); // parent IS a tracked item
        s.upsert_item(&f1).unwrap();
        s.upsert_item(&top).unwrap();
        s.upsert_item(&child).unwrap();
        // roots = the two items whose parent ("DR") is not itself an item
        let roots = s.roots("a", "onedrive").unwrap();
        assert_eq!(
            roots.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(),
            ["Folder One", "top.txt"]
        );
        // the nested file is reachable only via children of F1
        let kids = s.children("a", "onedrive", Some("F1")).unwrap();
        assert_eq!(
            kids.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(),
            ["nested.txt"]
        );
        // a tombstoned root drops out
        s.mark_deleted("a", "onedrive", "top", "2026-06-02T00:00:00Z")
            .unwrap();
        assert_eq!(s.roots("a", "onedrive").unwrap().len(), 1);
    }

    #[test]
    fn delta_cursor_persists_and_bumps_generation() {
        let s = Store::open_in_memory().unwrap();
        assert_eq!(s.get_delta_cursor("a", "onedrive", "").unwrap(), None);
        s.set_delta_cursor("a", "onedrive", "", "TOKEN1").unwrap();
        assert_eq!(
            s.get_delta_cursor("a", "onedrive", "").unwrap().as_deref(),
            Some("TOKEN1")
        );
        assert_eq!(s.delta_generation("a", "onedrive", "").unwrap(), Some(1));
        s.set_delta_cursor("a", "onedrive", "", "TOKEN2").unwrap();
        assert_eq!(
            s.get_delta_cursor("a", "onedrive", "").unwrap().as_deref(),
            Some("TOKEN2")
        );
        assert_eq!(s.delta_generation("a", "onedrive", "").unwrap(), Some(2));
    }

    #[test]
    fn per_folder_delta_scopes_are_isolated() {
        // Two folder scopes under the same (account, service) keep independent
        // cursors + generations — the delta_state PK is (account, service, scope).
        let s = Store::open_in_memory().unwrap();
        s.set_delta_cursor("a", "onedrive", "F1", "T1").unwrap();
        s.set_delta_cursor("a", "onedrive", "F2", "T2").unwrap();
        assert_eq!(
            s.get_delta_cursor("a", "onedrive", "F1").unwrap().as_deref(),
            Some("T1")
        );
        assert_eq!(
            s.get_delta_cursor("a", "onedrive", "F2").unwrap().as_deref(),
            Some("T2")
        );
        // Bumping F1 does not touch F2's generation.
        s.set_delta_cursor("a", "onedrive", "F1", "T1b").unwrap();
        assert_eq!(s.delta_generation("a", "onedrive", "F1").unwrap(), Some(2));
        assert_eq!(s.delta_generation("a", "onedrive", "F2").unwrap(), Some(1));
        assert_eq!(
            s.get_delta_cursor("a", "onedrive", "F2").unwrap().as_deref(),
            Some("T2")
        );
    }

    #[test]
    fn clear_delta_cursor_removes_only_the_target_scope() {
        let s = Store::open_in_memory().unwrap();
        s.set_delta_cursor("a", "onedrive", "F1", "T1").unwrap();
        s.set_delta_cursor("a", "onedrive", "F2", "T2").unwrap();
        // Clearing a present scope returns true and removes just that row.
        assert!(s.clear_delta_cursor("a", "onedrive", "F1").unwrap());
        assert_eq!(s.get_delta_cursor("a", "onedrive", "F1").unwrap(), None);
        // Sibling scope untouched.
        assert_eq!(
            s.get_delta_cursor("a", "onedrive", "F2").unwrap().as_deref(),
            Some("T2")
        );
        // Clearing an absent scope returns false, no error.
        assert!(!s.clear_delta_cursor("a", "onedrive", "F1").unwrap());
    }

    #[test]
    fn body_fts_indexes_searches_updates_and_scopes() {
        let s = Store::open_in_memory().unwrap();
        s.index_body(
            "a",
            "mail",
            "m1",
            "The quarterly invoice is attached, total 1200 EUR",
        )
        .unwrap();
        s.index_body("a", "mail", "m2", "Lunch on Friday?").unwrap();
        s.index_body("b", "mail", "m3", "invoice for the other account")
            .unwrap();

        // match by body content, account-scoped
        let hits = s.search_bodies("a", "invoice").unwrap();
        assert_eq!(hits, vec![("mail".to_string(), "m1".to_string())]);
        // other account isolated
        assert_eq!(
            s.search_bodies("b", "invoice").unwrap(),
            vec![("mail".to_string(), "m3".to_string())]
        );
        // diacritics-insensitive tokenizer
        s.index_body("a", "calendar", "e1", "Geschäftsführung Meeting")
            .unwrap();
        assert_eq!(s.search_bodies("a", "geschaftsfuhrung").unwrap().len(), 1);

        // re-index replaces (old terms gone, new terms found) — single row
        s.index_body("a", "mail", "m1", "now about shipping logistics")
            .unwrap();
        assert!(s.search_bodies("a", "invoice").unwrap().is_empty());
        assert_eq!(
            s.search_bodies("a", "shipping").unwrap(),
            vec![("mail".to_string(), "m1".to_string())]
        );
    }

    #[test]
    fn fts_search_matches_names_and_respects_account() {
        let s = Store::open_in_memory().unwrap();
        s.upsert_item(&item("a", "r1", "vacation report.txt"))
            .unwrap();
        s.upsert_item(&item("a", "r2", "invoice.pdf")).unwrap();
        s.upsert_item(&item("b", "r3", "report card.txt")).unwrap();
        let hits = s.search_names("a", "report").unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].remote_id, "r1");
        // diacritics-insensitive tokenizer
        s.upsert_item(&item("a", "r4", "Lebenslauf.pdf")).unwrap();
        assert_eq!(s.search_names("a", "lebenslauf").unwrap().len(), 1);
        // v13: the name-only FTS trigger must still track a rename …
        s.upsert_item(&item("a", "r1", "holiday summary.txt")).unwrap();
        assert!(s.search_names("a", "report").unwrap().is_empty(), "old name must drop out");
        assert_eq!(s.search_names("a", "holiday").unwrap()[0].remote_id, "r1");
        // … while a metadata-only update (no name change) leaves the index intact.
        s.set_local_path("a", "onedrive", "r1", Some("od/aa/r1.bin")).unwrap();
        s.set_preview_json("a", "onedrive", "r1", r#"{"x":1}"#).unwrap();
        assert_eq!(s.search_names("a", "holiday").unwrap()[0].remote_id, "r1");
    }

    #[test]
    fn items_by_service_returns_all_types_non_deleted() {
        let s = Store::open_in_memory().unwrap();
        s.upsert_item(&Item::new("a", "mail", "F1", "Inbox", "folder"))
            .unwrap();
        s.upsert_item(&Item::new("a", "mail", "m1", "Hi", "message"))
            .unwrap();
        s.upsert_item(&Item::new("a", "calendar", "e1", "Ev", "event"))
            .unwrap();
        s.mark_deleted("a", "mail", "m1", "2026-06-02T00:00:00Z")
            .unwrap();
        let mail = s.items_by_service("a", "mail").unwrap();
        assert_eq!(mail.len(), 1); // folder only (message tombstoned)
        assert_eq!(mail[0].remote_id, "F1");
        assert_eq!(s.items_by_service("a", "calendar").unwrap().len(), 1);
        assert!(s.items_by_service("a", "todo").unwrap().is_empty());
    }

    #[test]
    fn items_by_type_and_set_local_path() {
        let s = Store::open_in_memory().unwrap();
        let mut m1 = Item::new("a", "mail", "m1", "Hi", "message");
        m1.parent_remote_id = Some("F1".into());
        s.upsert_item(&m1).unwrap();
        s.upsert_item(&Item::new("a", "mail", "F1", "Inbox", "folder"))
            .unwrap();
        s.upsert_item(&Item::new("a", "mail", "m2", "Bye", "message"))
            .unwrap();
        s.mark_deleted("a", "mail", "m2", "2026-06-02T00:00:00Z")
            .unwrap();
        let msgs = s.items_by_type("a", "mail", "message").unwrap();
        assert_eq!(msgs.len(), 1); // m1 only (m2 tombstoned, F1 is a folder)
        assert_eq!(msgs[0].remote_id, "m1");
        assert!(msgs[0].local_path.is_none());

        assert!(s
            .set_local_path("a", "mail", "m1", Some("mail/ab/cd/x.eml"))
            .unwrap());
        let got = s.get_item("a", "mail", "m1").unwrap().unwrap();
        assert_eq!(got.local_path.as_deref(), Some("mail/ab/cd/x.eml"));
        assert!(!s.set_local_path("a", "mail", "missing", Some("x")).unwrap());
    }

    #[test]
    fn live_remote_ids_lists_non_deleted_for_service() {
        let s = Store::open_in_memory().unwrap();
        s.upsert_item(&Item::new("a", "onenote", "p1", "Page 1", "page"))
            .unwrap();
        s.upsert_item(&Item::new("a", "onenote", "p2", "Page 2", "page"))
            .unwrap();
        s.upsert_item(&Item::new("a", "onedrive", "f1", "f.txt", "file"))
            .unwrap();
        s.mark_deleted("a", "onenote", "p2", "2026-06-02T00:00:00Z")
            .unwrap();
        let mut ids = s.live_remote_ids("a", "onenote").unwrap();
        ids.sort();
        assert_eq!(ids, vec!["p1".to_string()]); // p2 tombstoned, f1 other service
        assert!(s.live_remote_ids("a", "contacts").unwrap().is_empty());
    }

    #[test]
    fn persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.db");
        {
            let s = Store::open(&path).unwrap();
            s.upsert_item(&item("a", "r1", "keep.txt")).unwrap();
            s.set_delta_cursor("a", "onedrive", "", "CUR").unwrap();
        } // drop -> releases lock
        let s2 = Store::open(&path).unwrap();
        assert_eq!(
            s2.get_item("a", "onedrive", "r1").unwrap().unwrap().name,
            "keep.txt"
        );
        assert_eq!(
            s2.get_delta_cursor("a", "onedrive", "").unwrap().as_deref(),
            Some("CUR")
        );
    }

    #[test]
    fn single_instance_lock_blocks_second_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.db");
        let _s = Store::open(&path).unwrap();
        match Store::open(&path) {
            Err(StoreError::AlreadyRunning(_)) => {}
            Err(e) => panic!("expected AlreadyRunning, got {e:?}"),
            Ok(_) => panic!("expected AlreadyRunning, got a second store"),
        }
    }

    #[test]
    fn open_readonly_reads_while_a_writer_holds_the_lock() {
        // The GET fast path: a read-only WAL reader must open and read even while a
        // writer (a sync pass) still holds the exclusive instance lock — this is what
        // stops a mailbox load from stalling behind an in-flight sync. A second
        // *writable* open would (correctly) fail with AlreadyRunning; the read-only one
        // must succeed and see committed rows.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.db");
        let writer = Store::open(&path).unwrap(); // holds the .lock for its lifetime
        writer.upsert_item(&item("a", "r1", "keep.txt")).unwrap();
        // a competing writable open is blocked …
        assert!(matches!(
            Store::open(&path),
            Err(StoreError::AlreadyRunning(_))
        ));
        // … but a read-only open goes straight through and sees the committed row.
        let reader = Store::open_readonly(&path).unwrap();
        assert_eq!(
            reader.get_item("a", "onedrive", "r1").unwrap().unwrap().name,
            "keep.txt"
        );
    }

    #[test]
    fn concurrent_opens_from_one_process_all_succeed() {
        // The multi-threaded web server opens the store per request, so several
        // short-lived opens can overlap; the brief retry in acquire_lock must let
        // them all succeed rather than racing the AlreadyRunning lock (#564).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.db");
        Store::open(&path).unwrap(); // create the store, then drop (release lock)
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let p = path.clone();
                std::thread::spawn(move || {
                    let s = Store::open(&p).expect("concurrent open should succeed");
                    let _ = s.count_by_service("a", "onedrive");
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
    }

    #[test]
    fn backup_to_writes_a_readable_consistent_copy() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("live.db")).unwrap();
        store
            .upsert_item(&Item::new("a", "onedrive", "r1", "f.txt", "file"))
            .unwrap();
        let snap = dir.path().join("snap.db");
        store.backup_to(&snap).unwrap();
        assert!(snap.exists(), "snapshot file must be created");
        // the snapshot opens as an independent, valid store with the same data
        let restored = Store::open(&snap).unwrap();
        assert!(restored.get_item("a", "onedrive", "r1").unwrap().is_some());
        drop(restored);
        // VACUUM INTO refuses a pre-existing target; backup_to clears it first
        store.backup_to(&snap).unwrap();
    }

    #[test]
    fn encrypted_store_hides_plaintext_and_requires_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("encrypted.db");
        {
            let store = Store::open_encrypted(&path, b"correct horse battery staple").unwrap();
            store
                .upsert_item(&Item::new(
                    "a",
                    "mail",
                    "r-secret",
                    "Quarterly super secret plan",
                    "message",
                ))
                .unwrap();
            store
                .index_body("a", "mail", "r-secret", "body text with classified details")
                .unwrap();
        }

        let raw = std::fs::read(&path).unwrap();
        assert!(!raw.starts_with(b"SQLite format 3\0"));
        assert!(
            !contains_bytes(&raw, b"super secret"),
            "raw DB leaked item name"
        );
        assert!(
            !contains_bytes(&raw, b"classified details"),
            "raw DB leaked body FTS content"
        );

        assert!(
            Store::open(&path).is_err(),
            "encrypted DB opened without key"
        );
        assert!(
            Store::open_encrypted(&path, b"wrong secret").is_err(),
            "encrypted DB opened with the wrong key"
        );

        let reopened = Store::open_encrypted(&path, b"correct horse battery staple").unwrap();
        assert_eq!(
            reopened
                .get_item("a", "mail", "r-secret")
                .unwrap()
                .unwrap()
                .name,
            "Quarterly super secret plan"
        );
        assert_eq!(
            reopened.search_bodies("a", "classified").unwrap(),
            vec![("mail".to_string(), "r-secret".to_string())]
        );
    }

    #[test]
    fn synced_state_roundtrip_and_default_none() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path().join("s.db")).unwrap();
        s.upsert_item(&Item::new("a", "onedrive", "f1", "doc.txt", "file"))
            .unwrap();
        // never recorded -> None (pre-v8 stores / never-materialized items)
        assert_eq!(s.get_synced_state("a", "onedrive", "f1").unwrap(), None);
        assert!(s
            .set_synced_state("a", "onedrive", "f1", 42, 1_700_000_000, Some("HASH=="))
            .unwrap());
        assert_eq!(
            s.get_synced_state("a", "onedrive", "f1").unwrap(),
            Some((42, 1_700_000_000, Some("HASH==".to_string())))
        );
        // unknown item -> no row updated, still None
        assert!(!s
            .set_synced_state("a", "onedrive", "nope", 1, 1, None)
            .unwrap());
        assert_eq!(s.get_synced_state("a", "onedrive", "nope").unwrap(), None);
    }

    /// Build a plaintext store with data in every copy-relevant shape: items,
    /// FTS-indexed bodies, and a delta cursor.
    fn seeded_plain_store(path: &Path) {
        let store = Store::open(path).unwrap();
        store
            .upsert_item(&Item::new(
                "a",
                "mail",
                "m1",
                "Migration secret subject",
                "message",
            ))
            .unwrap();
        store
            .index_body("a", "mail", "m1", "confidential migration body")
            .unwrap();
        store
            .set_delta_cursor("a", "onedrive", "root", "cursor-token-xyz")
            .unwrap();
    }

    #[test]
    fn migrate_to_encrypted_preserves_rows_and_fts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.db");
        seeded_plain_store(&path);
        assert!(std::fs::read(&path)
            .unwrap()
            .starts_with(b"SQLite format 3\0"));

        Store::migrate_to_encrypted(&path, b"migration key").unwrap();

        // No temp/sidecar leftovers right after the atomic swap (checked before
        // any reopen: an open WAL connection legitimately recreates `-wal`).
        assert!(!dir.path().join("store.db.encrypting").exists());
        assert!(!dir.path().join("store.db-wal").exists());

        // Encrypted at rest: header gone, known plaintext gone, keyless open fails.
        let raw = std::fs::read(&path).unwrap();
        assert!(!raw.starts_with(b"SQLite format 3\0"));
        assert!(!contains_bytes(&raw, b"Migration secret"));
        assert!(!contains_bytes(&raw, b"confidential migration"));
        assert!(Store::open(&path).is_err());

        // Same data through the same key semantics as open_encrypted.
        let store = Store::open_encrypted(&path, b"migration key").unwrap();
        assert_eq!(
            store.get_item("a", "mail", "m1").unwrap().unwrap().name,
            "Migration secret subject"
        );
        assert_eq!(
            store.search_bodies("a", "confidential").unwrap(),
            vec![("mail".to_string(), "m1".to_string())]
        );
        assert!(!store.search_names("a", "Migration").unwrap().is_empty());
        assert_eq!(
            store.get_delta_cursor("a", "onedrive", "root").unwrap(),
            Some("cursor-token-xyz".to_string())
        );
    }

    #[test]
    fn migrate_refuses_empty_secret_and_leaves_store_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.db");
        seeded_plain_store(&path);

        let err = Store::migrate_to_encrypted(&path, b"").unwrap_err();
        assert!(matches!(err, StoreError::InvalidStoreSecret(_)));
        // Original untouched: still plaintext + fully usable.
        assert!(std::fs::read(&path)
            .unwrap()
            .starts_with(b"SQLite format 3\0"));
        let store = Store::open(&path).unwrap();
        assert!(store.get_item("a", "mail", "m1").unwrap().is_some());
    }

    #[test]
    fn migrate_already_encrypted_store_is_a_detectable_noop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.db");
        {
            Store::open_encrypted(&path, b"k1").unwrap();
        }
        let err = Store::migrate_to_encrypted(&path, b"k1").unwrap_err();
        assert!(matches!(err, StoreError::AlreadyEncrypted(_)));
        // Still opens fine afterwards — re-running migration cannot damage it.
        Store::open_encrypted(&path, b"k1").unwrap();
    }

    #[test]
    fn migrate_recovers_from_stale_tmp_of_a_crashed_run() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.db");
        seeded_plain_store(&path);
        // Simulate a crash mid-migration: garbage temp target left behind, the
        // plaintext original untouched (it is never modified before the rename).
        std::fs::write(
            dir.path().join("store.db.encrypting"),
            b"garbage from crash",
        )
        .unwrap();

        Store::migrate_to_encrypted(&path, b"resume key").unwrap();

        let store = Store::open_encrypted(&path, b"resume key").unwrap();
        assert_eq!(
            store.get_item("a", "mail", "m1").unwrap().unwrap().name,
            "Migration secret subject"
        );
    }

    #[test]
    fn encrypted_backup_to_keeps_snapshot_encrypted() {
        let dir = tempfile::tempdir().unwrap();
        let live = dir.path().join("live-encrypted.db");
        let snap = dir.path().join("snap-encrypted.db");
        let store = Store::open_encrypted(&live, b"pbs snapshot key").unwrap();
        store
            .upsert_item(&Item::new(
                "a",
                "onedrive",
                "r1",
                "encrypted snapshot file.txt",
                "file",
            ))
            .unwrap();
        store.backup_to(&snap).unwrap();
        drop(store);

        let raw = std::fs::read(&snap).unwrap();
        assert!(!raw.starts_with(b"SQLite format 3\0"));
        assert!(
            !contains_bytes(&raw, b"encrypted snapshot file"),
            "snapshot DB leaked item name"
        );
        assert!(
            Store::open(&snap).is_err(),
            "encrypted snapshot opened without key"
        );
        let restored = Store::open_encrypted(&snap, b"pbs snapshot key").unwrap();
        assert!(restored.get_item("a", "onedrive", "r1").unwrap().is_some());
    }

    #[test]
    fn store_secret_file_is_trimmed_and_nonempty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store-key");
        std::fs::write(&path, b"\n  sqlcipher secret from credential \t\n").unwrap();
        assert_eq!(
            read_store_secret_file(&path).unwrap(),
            b"sqlcipher secret from credential"
        );

        std::fs::write(&path, b" \n\t ").unwrap();
        let err = read_store_secret_file(&path).unwrap_err();
        assert!(matches!(err, StoreError::InvalidStoreSecret(_)));
    }

    fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
        haystack
            .windows(needle.len())
            .any(|window| window == needle)
    }

    #[test]
    fn upload_session_roundtrips_and_clears() {
        let store = Store::open_in_memory().unwrap();
        assert!(store
            .get_upload_session("a", "onedrive", "/Big.bin")
            .unwrap()
            .is_none());
        store
            .save_upload_session("a", "onedrive", "/Big.bin", "https://up/1", 1_048_576, 0)
            .unwrap();
        assert_eq!(
            store
                .get_upload_session("a", "onedrive", "/Big.bin")
                .unwrap(),
            Some(("https://up/1".to_string(), 1_048_576))
        );
        // saving again updates url + offset in place (one row per dest)
        store
            .save_upload_session(
                "a",
                "onedrive",
                "/Big.bin",
                "https://up/2",
                1_048_576,
                327_680,
            )
            .unwrap();
        assert_eq!(
            store
                .get_upload_session("a", "onedrive", "/Big.bin")
                .unwrap()
                .unwrap()
                .0,
            "https://up/2"
        );
        store
            .clear_upload_session("a", "onedrive", "/Big.bin")
            .unwrap();
        assert!(store
            .get_upload_session("a", "onedrive", "/Big.bin")
            .unwrap()
            .is_none());
    }

    // --- restore operation ledger (ADR-001) ---

    #[test]
    fn restore_op_create_and_get() {
        let s = Store::open_in_memory().unwrap();
        s.create_restore_operation("op1", "a", "mail", "src1", "key1", 100)
            .unwrap();
        let op = s.get_restore_operation("op1").unwrap().unwrap();
        assert_eq!(op.state, RestoreState::Pending);
        assert_eq!(op.account_id, "a");
        assert_eq!(op.idempotency_key, "key1");
        assert_eq!(op.attempts, 0);
        assert_eq!(op.created_at, 100);
        assert_eq!(
            s.restore_history("op1").unwrap(),
            vec![(None, "pending".to_string())]
        );
        assert!(s.get_restore_operation("nope").unwrap().is_none());
    }

    #[test]
    fn restore_op_happy_path_transitions_and_audit() {
        let s = Store::open_in_memory().unwrap();
        s.create_restore_operation("op1", "a", "mail", "src1", "key1", 100)
            .unwrap();
        s.transition_restore(
            "op1",
            RestoreState::PreflightChecked,
            101,
            Some("preflight"),
            None,
            None,
        )
        .unwrap();
        s.transition_restore(
            "op1",
            RestoreState::Committing,
            102,
            None,
            None,
            Some("msgid-123"),
        )
        .unwrap();
        s.transition_restore(
            "op1",
            RestoreState::Committed,
            103,
            None,
            Some("AAMk-new"),
            None,
        )
        .unwrap();
        let op = s.get_restore_operation("op1").unwrap().unwrap();
        assert_eq!(op.state, RestoreState::Committed);
        assert_eq!(op.attempts, 1); // bumped once, on entering committing
        assert_eq!(op.new_cloud_id.as_deref(), Some("AAMk-new"));
        assert_eq!(op.marker.as_deref(), Some("msgid-123"));
        let hist = s.restore_history("op1").unwrap();
        assert_eq!(hist.len(), 4);
        assert_eq!(hist.last().unwrap().1, "committed");
    }

    #[test]
    fn restore_op_illegal_transition_is_rejected_and_state_unchanged() {
        let s = Store::open_in_memory().unwrap();
        s.create_restore_operation("op1", "a", "mail", "src1", "key1", 100)
            .unwrap();
        let err = s
            .transition_restore("op1", RestoreState::Committed, 101, None, None, None)
            .unwrap_err();
        assert!(matches!(err, StoreError::IllegalTransition(_)));
        assert_eq!(
            s.get_restore_operation("op1").unwrap().unwrap().state,
            RestoreState::Pending
        );
        assert_eq!(s.restore_history("op1").unwrap().len(), 1);
    }

    #[test]
    fn restore_op_failed_after_commit_reconciles_or_resumes() {
        let s = Store::open_in_memory().unwrap();
        // reconcile path: failed_after_graph_commit -> committed
        s.create_restore_operation("op1", "a", "mail", "s", "k1", 1)
            .unwrap();
        s.transition_restore("op1", RestoreState::PreflightChecked, 2, None, None, None)
            .unwrap();
        s.transition_restore("op1", RestoreState::Committing, 3, None, None, None)
            .unwrap();
        s.transition_restore(
            "op1",
            RestoreState::FailedAfterGraphCommit,
            4,
            Some("killed"),
            None,
            None,
        )
        .unwrap();
        s.transition_restore(
            "op1",
            RestoreState::Committed,
            5,
            Some("found in cloud"),
            Some("id"),
            None,
        )
        .unwrap();
        assert_eq!(
            s.get_restore_operation("op1").unwrap().unwrap().state,
            RestoreState::Committed
        );

        // resume path: failed_after_graph_commit -> committing
        s.create_restore_operation("op2", "a", "mail", "s", "k2", 1)
            .unwrap();
        s.transition_restore("op2", RestoreState::PreflightChecked, 2, None, None, None)
            .unwrap();
        s.transition_restore("op2", RestoreState::Committing, 3, None, None, None)
            .unwrap();
        s.transition_restore(
            "op2",
            RestoreState::FailedAfterGraphCommit,
            4,
            None,
            None,
            None,
        )
        .unwrap();
        s.transition_restore(
            "op2",
            RestoreState::Committing,
            5,
            Some("not found, resume"),
            None,
            None,
        )
        .unwrap();
        let op2 = s.get_restore_operation("op2").unwrap().unwrap();
        assert_eq!(op2.state, RestoreState::Committing);
        assert_eq!(op2.attempts, 2); // committing entered twice
    }

    #[test]
    fn restore_op_lease_is_single_owner_until_expiry() {
        let s = Store::open_in_memory().unwrap();
        s.create_restore_operation("op1", "a", "mail", "s", "k1", 1)
            .unwrap();
        assert!(s.acquire_restore_lease("op1", "daemon-A", 100, 30).unwrap()); // expires 130
        assert!(!s.acquire_restore_lease("op1", "daemon-B", 110, 30).unwrap()); // still held
        assert!(s.acquire_restore_lease("op1", "daemon-B", 131, 30).unwrap()); // expired -> taken
        assert_eq!(
            s.get_restore_operation("op1")
                .unwrap()
                .unwrap()
                .lease_owner
                .as_deref(),
            Some("daemon-B")
        );
        s.release_restore_lease("op1").unwrap();
        assert!(s
            .get_restore_operation("op1")
            .unwrap()
            .unwrap()
            .lease_owner
            .is_none());
        assert!(s.acquire_restore_lease("op1", "daemon-C", 5, 30).unwrap()); // free after release
    }

    #[test]
    fn restore_op_recoverable_excludes_terminal_and_other_accounts() {
        let s = Store::open_in_memory().unwrap();
        s.create_restore_operation("done", "a", "mail", "s", "k1", 1)
            .unwrap();
        s.transition_restore("done", RestoreState::Cancelled, 2, None, None, None)
            .unwrap();
        s.create_restore_operation("open1", "a", "mail", "s", "k2", 2)
            .unwrap();
        s.transition_restore("open1", RestoreState::PreflightChecked, 3, None, None, None)
            .unwrap();
        s.create_restore_operation("open2", "a", "mail", "s", "k3", 3)
            .unwrap(); // pending
        s.create_restore_operation("other", "b", "mail", "s", "k4", 4)
            .unwrap(); // other account
        let rec = s.recoverable_restore_operations("a").unwrap();
        let ids: Vec<_> = rec.iter().map(|o| o.op_id.as_str()).collect();
        assert_eq!(ids, vec!["open1", "open2"]);
    }

    #[test]
    fn restore_op_idempotency_key_is_unique_per_account() {
        let s = Store::open_in_memory().unwrap();
        s.create_restore_operation("op1", "a", "mail", "s", "samekey", 1)
            .unwrap();
        // same account + same key -> rejected (duplicate-create backstop)
        let err = s
            .create_restore_operation("op2", "a", "mail", "s", "samekey", 2)
            .unwrap_err();
        assert!(matches!(err, StoreError::Sqlite(_)));
        // same key, different account is fine
        s.create_restore_operation("op3", "b", "mail", "s", "samekey", 3)
            .unwrap();
    }

    #[test]
    fn restore_state_machine_rules_and_roundtrip() {
        use RestoreState::*;
        assert!(Pending.can_transition_to(PreflightChecked));
        assert!(!Pending.can_transition_to(Committing));
        assert!(Committing.can_transition_to(FailedAfterGraphCommit));
        assert!(FailedAfterGraphCommit.can_transition_to(Committed));
        assert!(FailedAfterGraphCommit.can_transition_to(Committing));
        assert!(!Committed.can_transition_to(Committing));
        assert!(Committed.is_terminal());
        assert!(Cancelled.is_terminal());
        assert!(!Committing.is_terminal());
        for st in [
            Pending,
            PreflightChecked,
            Committing,
            Committed,
            FailedAfterGraphCommit,
            Cancelled,
        ] {
            assert_eq!(RestoreState::parse(st.as_str()), Some(st));
        }
    }
}
