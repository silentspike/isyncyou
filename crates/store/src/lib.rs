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
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("another instance already holds the store lock at {0}")]
    AlreadyRunning(String),
}

pub type Result<T> = std::result::Result<T, StoreError>;

/// Current schema version. Bump + add a migration step when the schema changes.
pub const SCHEMA_VERSION: i64 = 5;

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
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
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
    /// `dest` must not already exist (VACUUM INTO refuses to overwrite).
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
}
