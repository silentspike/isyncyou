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
use rusqlite::{params, Connection, OptionalExtension};
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

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
pub const SCHEMA_VERSION: i64 = 7;

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
        }
    }
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
        match configured_store_key()? {
            Some(secret) => Self::open_encrypted(path, &secret),
            None => Self::open_plain(path),
        }
    }

    /// Open (or create) a SQLCipher-encrypted store using `secret`.
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
    /// only [`apply_sqlcipher_key`] ever touches the key, and the plaintext source
    /// is attached *from the keyed connection* with an empty `KEY ''` (SQLCipher's
    /// documented plaintext attach) — so the migrated store opens with the same
    /// secret later.
    ///
    /// Crash-safe + idempotent: the original file is never modified before the
    /// final atomic rename (a crash mid-migration leaves the plaintext store fully
    /// usable, plus a stale temp file that the next run removes); migrating an
    /// already-encrypted store fails with [`StoreError::AlreadyEncrypted`] so a
    /// re-run after success is a clean, detectable no-op for the caller.
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
        f.try_lock_exclusive()
            .map_err(|_| StoreError::AlreadyRunning(lock_path.clone()))?;
        Ok(f)
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
                  change_key, internet_message_id, ical_uid, series_master_id)
               VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18)
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
                 series_master_id    = excluded.series_master_id"#,
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
                it.series_master_id
            ],
        )?;
        Ok(())
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
            "SELECT {COLS} FROM items
             WHERE account_id=?1 AND service=?2 AND deleted_at IS NULL
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
             WHERE account_id=?1 AND service=?2 AND deleted_at IS NULL",
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
        let n = self.conn.execute(
            "UPDATE items SET local_path=?4
             WHERE account_id=?1 AND service=?2 AND remote_id=?3",
            params![account, service, remote_id, local_path],
        )?;
        Ok(n > 0)
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

pub fn configured_store_key() -> Result<Option<Vec<u8>>> {
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
                    change_key, internet_message_id, ical_uid, series_master_id";

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
