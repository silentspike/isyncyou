//! Provider-scoped objects and atomic synchronization checkpoints.

use super::{Result, Store, StoreError};
use rusqlite::{params, OptionalExtension};
use sha2::{Digest, Sha256};

pub(super) const MIGRATION: &str = r#"
CREATE TABLE provider_collections (
 account_id TEXT NOT NULL, origin TEXT NOT NULL, service TEXT NOT NULL,
 collection TEXT NOT NULL, cursor TEXT, observed_at INTEGER, accessible INTEGER NOT NULL DEFAULT 1, complete INTEGER NOT NULL DEFAULT 0,
 PRIMARY KEY(account_id, origin, service, collection)
);
CREATE TABLE provider_objects (
 account_id TEXT NOT NULL, origin TEXT NOT NULL, service TEXT NOT NULL,
 collection TEXT NOT NULL, remote_id TEXT NOT NULL, etag TEXT, raw BLOB NOT NULL,
 PRIMARY KEY(account_id, origin, service, collection, remote_id)
);
CREATE VIRTUAL TABLE provider_mail_fts USING fts5(
 account_id UNINDEXED, origin UNINDEXED, collection UNINDEXED, remote_id UNINDEXED,
 searchable, tokenize='unicode61 remove_diacritics 2'
);
CREATE TABLE provider_capture_outbox (
 sequence INTEGER PRIMARY KEY AUTOINCREMENT,
 account_id TEXT NOT NULL, origin TEXT NOT NULL, service TEXT NOT NULL,
 collection TEXT NOT NULL, remote_id TEXT NOT NULL, digest TEXT NOT NULL, version_key TEXT NOT NULL,
 etag TEXT, raw BLOB NOT NULL,
 UNIQUE(account_id, origin, service, collection, remote_id, version_key)
);
CREATE TABLE provider_events (
 sequence INTEGER PRIMARY KEY AUTOINCREMENT,
 account_id TEXT NOT NULL, origin TEXT NOT NULL, service TEXT NOT NULL,
 collection TEXT NOT NULL, remote_id TEXT NOT NULL, kind TEXT NOT NULL,
 version_key TEXT, cursor TEXT NOT NULL
);
CREATE TABLE provider_submissions (
 account_id TEXT NOT NULL, origin TEXT NOT NULL, remote_account TEXT NOT NULL,
 operation_id TEXT NOT NULL, draft_id TEXT NOT NULL, identity_id TEXT NOT NULL,
 email_state TEXT NOT NULL, snapshot_digest TEXT NOT NULL, creation_id TEXT NOT NULL, intent_json TEXT NOT NULL,
 state TEXT NOT NULL, submission_id TEXT, sent_update TEXT,
 PRIMARY KEY(account_id, origin, remote_account, operation_id)
);
CREATE UNIQUE INDEX provider_submission_email_once
 ON provider_submissions(account_id,origin,remote_account,draft_id)
 WHERE state IN ('unknown','confirmed');
CREATE TABLE provider_mutations (
 account_id TEXT NOT NULL, origin TEXT NOT NULL, service TEXT NOT NULL, remote_account TEXT NOT NULL,
 operation_id TEXT NOT NULL, kind TEXT NOT NULL, intent_json TEXT NOT NULL,
 intent_digest TEXT NOT NULL, state TEXT NOT NULL, result_id TEXT,
 PRIMARY KEY(account_id,origin,service,remote_account,operation_id)
);
CREATE TABLE provider_stage_runs (
 account_id TEXT NOT NULL, origin TEXT NOT NULL, service TEXT NOT NULL,
 collection TEXT NOT NULL, generation TEXT NOT NULL, base_cursor TEXT, base_accessible INTEGER,
 PRIMARY KEY(account_id,origin,service,collection,generation)
);
CREATE TABLE provider_stage_objects (
 account_id TEXT NOT NULL, origin TEXT NOT NULL, service TEXT NOT NULL,
 collection TEXT NOT NULL, generation TEXT NOT NULL, remote_id TEXT NOT NULL,
 etag TEXT, raw BLOB NOT NULL,
 PRIMARY KEY(account_id,origin,service,collection,generation,remote_id)
);
"#;

pub(super) const MIGRATION_V18: &str = r#"
ALTER TABLE provider_mutations ADD COLUMN target_id TEXT;
CREATE UNIQUE INDEX provider_mutation_target_unknown ON provider_mutations(account_id,origin,service,remote_account,target_id) WHERE state='unknown' AND target_id IS NOT NULL;
CREATE TABLE provider_dav_index (
 account_id TEXT NOT NULL, origin TEXT NOT NULL, service TEXT NOT NULL,
 collection TEXT NOT NULL, remote_id TEXT NOT NULL,
 valid INTEGER NOT NULL, uid TEXT, kind TEXT, parent_uid TEXT,
 PRIMARY KEY(account_id,origin,service,collection,remote_id)
);
CREATE INDEX provider_dav_uid ON provider_dav_index(account_id,origin,service,collection,uid);
CREATE TABLE provider_stage_dav_index (
 account_id TEXT NOT NULL, origin TEXT NOT NULL, service TEXT NOT NULL,
 collection TEXT NOT NULL, generation TEXT NOT NULL, remote_id TEXT NOT NULL,
 valid INTEGER NOT NULL, uid TEXT, kind TEXT, parent_uid TEXT,
 PRIMARY KEY(account_id,origin,service,collection,generation,remote_id)
);
CREATE TABLE provider_sessions (
 account_id TEXT NOT NULL, origin TEXT NOT NULL, session_id TEXT NOT NULL,
 version INTEGER NOT NULL, manifest BLOB NOT NULL, object_bytes INTEGER NOT NULL DEFAULT 0, object_count INTEGER NOT NULL DEFAULT 0,
 PRIMARY KEY(account_id,origin,session_id)
);
CREATE TABLE provider_session_quotas (
 account_id TEXT NOT NULL, origin TEXT NOT NULL, total_bytes INTEGER NOT NULL DEFAULT 0,
 session_count INTEGER NOT NULL DEFAULT 0, PRIMARY KEY(account_id,origin)
);
CREATE TABLE provider_session_objects (
 account_id TEXT NOT NULL, origin TEXT NOT NULL, session_id TEXT NOT NULL,
 class TEXT NOT NULL, object_id TEXT NOT NULL, payload BLOB NOT NULL,
 PRIMARY KEY(account_id,origin,session_id,class,object_id)
);
CREATE TABLE provider_alarm_claims (
 account_id TEXT NOT NULL, origin TEXT NOT NULL, service TEXT NOT NULL, collection TEXT NOT NULL,
 uid TEXT NOT NULL, recurrence_id TEXT NOT NULL, alarm_id TEXT NOT NULL, trigger_at TEXT NOT NULL,
 state TEXT NOT NULL,
 PRIMARY KEY(account_id,origin,service,collection,uid,recurrence_id,alarm_id,trigger_at)
);
CREATE INDEX provider_alarm_pending ON provider_alarm_claims(account_id,origin,service,collection,state,trigger_at);
CREATE TABLE provider_alarm_quotas (
 account_id TEXT NOT NULL, origin TEXT NOT NULL, claim_count INTEGER NOT NULL DEFAULT 0,
 PRIMARY KEY(account_id,origin)
);
"#;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderScope {
    pub account_id: String,
    pub origin: String,
    pub service: String,
    pub collection: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderObject {
    pub remote_id: String,
    pub etag: Option<String>,
    pub raw: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderDavIndex {
    Valid { uid:String, kind:String, parent_uid:Option<String> },
    Invalid,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderDavIndexEntry {
    pub remote_id: String,
    pub index: ProviderDavIndex,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderAlarmIdentity {
    pub uid:String,
    pub recurrence_id:String,
    pub alarm_id:String,
    pub trigger_at:String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderCapture {
    pub sequence: i64,
    pub scope: ProviderScope,
    pub object: ProviderObject,
    pub digest: String,
    pub version_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderEvent {
    pub sequence: i64,
    pub scope: ProviderScope,
    pub remote_id: String,
    pub version_key: Option<String>,
    pub cursor: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderCollectionSnapshot {
    pub scope: ProviderScope,
    pub cursor: Option<String>,
    pub accessible: bool,
    pub complete: bool,
    pub observed_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderExportSnapshot {
    pub collections: Vec<ProviderCollectionSnapshot>,
    pub capture_highwater: i64,
    pub event_highwater: i64,
}

#[derive(Clone, PartialEq, Eq)]
pub struct ProviderSubmission {
    pub scope: ProviderScope,
    pub operation_id: String,
    pub draft_id: String,
    pub identity_id: String,
    pub email_state: String,
    pub snapshot_digest: String,
    pub creation_id: String,
    pub intent_json: String,
    pub state: String,
    pub submission_id: Option<String>,
    pub sent_update: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderChange {
    Upsert(ProviderObject),
    /// Only authoritative server destruction, never loss of membership/access.
    Destroyed(String),
}

impl Store {
    /// Claim one target while any prior effect on it is unresolved.
    pub fn claim_provider_target_mutation(&self, scope:&ProviderScope, operation:&str, kind:&str, target_id:&str, intent:&str, digest:&str) -> Result<bool> {
        if !self.protected_provider_storage() { return Err(StoreError::MissingStoreKey); }
        if target_id.is_empty() { return Err(StoreError::IllegalTransition("empty mutation target".into())); }
        Ok(self.conn.execute("INSERT OR IGNORE INTO provider_mutations(account_id,origin,service,remote_account,operation_id,kind,intent_json,intent_digest,state,target_id) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,'unknown',?9)",params![scope.account_id,scope.origin,scope.service,scope.collection,operation,kind,intent,digest,target_id])? == 1)
    }
    /// Persist before alarm dispatch. Existing pending claims must be recovered
    /// using an idempotent platform notification identity, never blindly replayed.
    pub fn claim_provider_alarm(&self, scope:&ProviderScope, alarm:&ProviderAlarmIdentity) -> Result<bool> {
        if !self.protected_provider_storage() { return Err(StoreError::MissingStoreKey); }
        if scope.service != "calendar" || alarm.uid.is_empty() || alarm.alarm_id.is_empty() || alarm.trigger_at.is_empty() || [alarm.uid.len(),alarm.recurrence_id.len(),alarm.alarm_id.len(),alarm.trigger_at.len()].into_iter().any(|length| length > 4096) { return Err(StoreError::IllegalTransition("invalid alarm identity".into())); }
        let tx = self.conn.unchecked_transaction()?;
        let inserted = tx.execute("INSERT OR IGNORE INTO provider_alarm_claims(account_id,origin,service,collection,uid,recurrence_id,alarm_id,trigger_at,state) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,'pending')",params![scope.account_id,scope.origin,scope.service,scope.collection,alarm.uid,alarm.recurrence_id,alarm.alarm_id,alarm.trigger_at])? == 1;
        if inserted {
            tx.execute("INSERT OR IGNORE INTO provider_alarm_quotas(account_id,origin) VALUES (?1,?2)",params![scope.account_id,scope.origin])?;
            if tx.execute("UPDATE provider_alarm_quotas SET claim_count=claim_count+1 WHERE account_id=?1 AND origin=?2 AND claim_count<100000",params![scope.account_id,scope.origin])? != 1 { return Err(StoreError::ProviderObjectTooLarge); }
        }
        tx.commit()?; Ok(inserted)
    }

    pub fn finish_provider_alarm(&self, scope:&ProviderScope, alarm:&ProviderAlarmIdentity) -> Result<bool> {
        if !self.protected_provider_storage() { return Err(StoreError::MissingStoreKey); }
        Ok(self.conn.execute("UPDATE provider_alarm_claims SET state='delivered' WHERE account_id=?1 AND origin=?2 AND service=?3 AND collection=?4 AND uid=?5 AND recurrence_id=?6 AND alarm_id=?7 AND trigger_at=?8 AND state='pending'",params![scope.account_id,scope.origin,scope.service,scope.collection,alarm.uid,alarm.recurrence_id,alarm.alarm_id,alarm.trigger_at])? == 1)
    }

    pub fn pending_provider_alarms(&self, scope:&ProviderScope, limit:usize) -> Result<Vec<ProviderAlarmIdentity>> {
        if !self.protected_provider_storage() { return Err(StoreError::MissingStoreKey); }
        let mut stmt=self.conn.prepare("SELECT uid,recurrence_id,alarm_id,trigger_at FROM provider_alarm_claims WHERE account_id=?1 AND origin=?2 AND service=?3 AND collection=?4 AND state='pending' ORDER BY trigger_at,uid LIMIT ?5")?;
        let rows=stmt.query_map(params![scope.account_id,scope.origin,scope.service,scope.collection,limit.min(200) as i64],|r| Ok(ProviderAlarmIdentity {uid:r.get(0)?,recurrence_id:r.get(1)?,alarm_id:r.get(2)?,trigger_at:r.get(3)?}))?;
        Ok(rows.collect::<std::result::Result<Vec<_>,_>>()?)
    }
    /// Detect any persisted provider state before binding an unverified identity.
    pub fn provider_contains_data(&self) -> Result<bool> {
        Ok(self.conn.query_row("SELECT EXISTS(SELECT 1 FROM provider_objects UNION ALL SELECT 1 FROM provider_capture_outbox UNION ALL SELECT 1 FROM provider_stage_objects UNION ALL SELECT 1 FROM provider_submissions UNION ALL SELECT 1 FROM provider_mutations UNION ALL SELECT 1 FROM provider_sessions LIMIT 1)",[],|r| r.get(0))?)
    }
    /// Create a sealed local session manifest; an existing session is never overwritten.
    pub fn local_session_create(&self, scope:&ProviderScope, session:&str, manifest:&[u8]) -> Result<bool> {
        if !self.protected_provider_storage() { return Err(StoreError::MissingStoreKey); }
        validate_session(scope,session)?;
        if manifest.is_empty() || manifest.len() > 96 * 1024 { return Err(StoreError::ProviderObjectTooLarge); }
        let tx = self.conn.unchecked_transaction()?;
        let inserted = tx.execute("INSERT OR IGNORE INTO provider_sessions(account_id,origin,session_id,version,manifest) VALUES (?1,?2,?3,1,?4)",params![scope.account_id,scope.origin,session,manifest])? == 1;
        if inserted {
            tx.execute("INSERT OR IGNORE INTO provider_session_quotas(account_id,origin) VALUES (?1,?2)",params![scope.account_id,scope.origin])?;
            if tx.execute("UPDATE provider_session_quotas SET session_count=session_count+1 WHERE account_id=?1 AND origin=?2 AND session_count<100",params![scope.account_id,scope.origin])? != 1 { return Err(StoreError::ProviderObjectTooLarge); }
        }
        if !inserted {
            let existing:Vec<u8> = tx.query_row("SELECT manifest FROM provider_sessions WHERE account_id=?1 AND origin=?2 AND session_id=?3",params![scope.account_id,scope.origin,session],|r| r.get(0))?;
            if existing != manifest { return Err(StoreError::IllegalTransition("session already exists".into())); }
        }
        tx.commit()?; Ok(inserted)
    }

    pub fn local_session_manifest(&self, scope:&ProviderScope, session:&str) -> Result<Option<(String,Vec<u8>)>> {
        if !self.protected_provider_storage() { return Err(StoreError::MissingStoreKey); }
        validate_session(scope,session)?;
        Ok(self.conn.query_row("SELECT version,manifest FROM provider_sessions WHERE account_id=?1 AND origin=?2 AND session_id=?3",params![scope.account_id,scope.origin,session],|r| Ok((r.get::<_,i64>(0)?.to_string(),r.get(1)?))).optional()?)
    }

    /// Compare-and-swap against a monotonically increasing etag (no ABA).
    pub fn local_session_cas(&self, scope:&ProviderScope, session:&str, expected_etag:&str, new_manifest:&[u8]) -> Result<Option<String>> {
        if !self.protected_provider_storage() { return Err(StoreError::MissingStoreKey); }
        validate_session(scope,session)?;
        if new_manifest.is_empty() || new_manifest.len() > 96 * 1024 { return Err(StoreError::ProviderObjectTooLarge); }
        let version:i64 = expected_etag.parse().map_err(|_| StoreError::IllegalTransition("invalid session etag".into()))?;
        if expected_etag != version.to_string() { return Err(StoreError::IllegalTransition("invalid session etag".into())); }
        let next = version.checked_add(1).filter(|_| version>0).ok_or_else(|| StoreError::IllegalTransition("invalid session etag".into()))?;
        let changed = self.conn.execute("UPDATE provider_sessions SET version=?5,manifest=?6 WHERE account_id=?1 AND origin=?2 AND session_id=?3 AND version=?4",params![scope.account_id,scope.origin,session,version,next,new_manifest])?;
        Ok((changed == 1).then(|| next.to_string()))
    }

    /// Immutable insert is idempotent only for byte-identical payloads.
    pub fn local_session_stage(&self, scope:&ProviderScope, session:&str, class:&str, id:&str, bytes:&[u8]) -> Result<()> {
        if !self.protected_provider_storage() { return Err(StoreError::MissingStoreKey); }
        validate_session(scope,session)?;
        if class.is_empty() || class.len()>128 || id.is_empty() || id.len()>256 { return Err(StoreError::IllegalTransition("invalid session object identity".into())); }
        if bytes.is_empty() || bytes.len() > 5 * 1024 * 1024 { return Err(StoreError::ProviderObjectTooLarge); }
        let tx = self.conn.unchecked_transaction()?;
        let exists:bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM provider_sessions WHERE account_id=?1 AND origin=?2 AND session_id=?3)",params![scope.account_id,scope.origin,session],|r| r.get(0))?;
        if !exists { return Err(StoreError::IllegalTransition("session missing".into())); }
        let inserted = tx.execute("INSERT OR IGNORE INTO provider_session_objects(account_id,origin,session_id,class,object_id,payload) VALUES (?1,?2,?3,?4,?5,?6)",params![scope.account_id,scope.origin,session,class,id,bytes])? == 1;
        if inserted {
            if tx.execute("UPDATE provider_sessions SET object_bytes=object_bytes+?4,object_count=object_count+1 WHERE account_id=?1 AND origin=?2 AND session_id=?3 AND object_bytes+?4<=134217728 AND object_count<20000",params![scope.account_id,scope.origin,session,bytes.len() as i64])? != 1 { return Err(StoreError::ProviderObjectTooLarge); }
            if tx.execute("UPDATE provider_session_quotas SET total_bytes=total_bytes+?3 WHERE account_id=?1 AND origin=?2 AND total_bytes+?3<=1073741824",params![scope.account_id,scope.origin,bytes.len() as i64])? != 1 { return Err(StoreError::ProviderObjectTooLarge); }
        }
        if !inserted {
            let existing:Vec<u8> = tx.query_row("SELECT payload FROM provider_session_objects WHERE account_id=?1 AND origin=?2 AND session_id=?3 AND class=?4 AND object_id=?5",params![scope.account_id,scope.origin,session,class,id],|r| r.get(0))?;
            if existing != bytes { return Err(StoreError::IllegalTransition("immutable session object conflict".into())); }
        }
        tx.commit()?; Ok(())
    }

    pub fn local_session_load(&self, scope:&ProviderScope, session:&str, class:&str, id:&str) -> Result<Option<Vec<u8>>> {
        if !self.protected_provider_storage() { return Err(StoreError::MissingStoreKey); }
        validate_session(scope,session)?;
        Ok(self.conn.query_row("SELECT payload FROM provider_session_objects WHERE account_id=?1 AND origin=?2 AND session_id=?3 AND class=?4 AND object_id=?5",params![scope.account_id,scope.origin,session,class,id],|r| r.get(0)).optional()?)
    }

    pub fn local_sessions(&self, scope:&ProviderScope, after:&str, limit:usize) -> Result<Vec<String>> {
        if !self.protected_provider_storage() { return Err(StoreError::MissingStoreKey); }
        if scope.account_id.is_empty() || scope.origin.is_empty() { return Err(StoreError::IllegalTransition("invalid session identity".into())); }
        let mut stmt=self.conn.prepare("SELECT session_id FROM provider_sessions WHERE account_id=?1 AND origin=?2 AND session_id>?3 ORDER BY session_id LIMIT ?4")?;
        let rows=stmt.query_map(params![scope.account_id,scope.origin,after,limit.min(100) as i64],|r| r.get(0))?;
        Ok(rows.collect::<std::result::Result<Vec<_>,_>>()?)
    }
    /// Apply DAV resource/index changes and checkpoint in one transaction.
    pub fn apply_provider_dav_page(&self, scope:&ProviderScope, changes:&[(ProviderChange,Option<ProviderDavIndex>)], cursor:&str) -> Result<()> {
        if !self.protected_provider_storage() { return Err(StoreError::MissingStoreKey); }
        if scope.service != "calendar" { return Err(StoreError::InvalidProgressiveSearch("DAV index requires calendar scope".into())); }
        let tx = self.conn.unchecked_transaction()?;
        for (change,index) in changes {
            match (change,index) {
                (ProviderChange::Upsert(object),Some(index)) => { apply_changes(&tx,scope,std::slice::from_ref(change),cursor)?; write_dav_index(&tx,scope,&object.remote_id,index)?; }
                (ProviderChange::Destroyed(_),None) => apply_changes(&tx,scope,std::slice::from_ref(change),cursor)?,
                _ => return Err(StoreError::InvalidProgressiveSearch("DAV resource/index mismatch".into())),
            }
        }
        apply_changes(&tx,scope,&[],cursor)?;
        tx.commit()?; Ok(())
    }

    /// Stage the parsed identity only after the corresponding raw resource was staged.
    pub fn stage_provider_dav_index(&self, scope:&ProviderScope, generation:&str, remote_id:&str, index:&ProviderDavIndex) -> Result<()> {
        if !self.protected_provider_storage() { return Err(StoreError::MissingStoreKey); }
        if scope.service != "calendar" { return Err(StoreError::InvalidProgressiveSearch("DAV index requires calendar scope".into())); }
        let tx = self.conn.unchecked_transaction()?;
        let exists:bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM provider_stage_objects WHERE account_id=?1 AND origin=?2 AND service=?3 AND collection=?4 AND generation=?5 AND remote_id=?6)",params![scope.account_id,scope.origin,scope.service,scope.collection,generation,remote_id],|r| r.get(0))?;
        if !exists { return Err(StoreError::InvalidProgressiveSearch("staged DAV resource missing".into())); }
        let (valid,uid,kind,parent) = dav_index_fields(index)?;
        tx.execute("INSERT INTO provider_stage_dav_index(account_id,origin,service,collection,generation,remote_id,valid,uid,kind,parent_uid) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10) ON CONFLICT(account_id,origin,service,collection,generation,remote_id) DO UPDATE SET valid=excluded.valid,uid=excluded.uid,kind=excluded.kind,parent_uid=excluded.parent_uid",params![scope.account_id,scope.origin,scope.service,scope.collection,generation,remote_id,valid,uid,kind,parent])?;
        tx.commit()?; Ok(())
    }

    pub fn provider_dav_uid_lookup(&self, scope:&ProviderScope, uid:&str) -> Result<Vec<ProviderDavIndexEntry>> {
        let mut stmt = self.conn.prepare("SELECT remote_id,kind,parent_uid FROM provider_dav_index WHERE account_id=?1 AND origin=?2 AND service=?3 AND collection=?4 AND uid=?5 AND valid=1 ORDER BY remote_id LIMIT 2")?;
        let rows = stmt.query_map(params![scope.account_id,scope.origin,scope.service,scope.collection,uid],|r| Ok(ProviderDavIndexEntry {remote_id:r.get(0)?,index:ProviderDavIndex::Valid {uid:uid.into(),kind:r.get(1)?,parent_uid:r.get(2)?}}))?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Any invalid or unindexed resource makes parent graph changes fail closed.
    pub fn provider_dav_index_incomplete_count(&self, scope:&ProviderScope) -> Result<i64> {
        Ok(self.conn.query_row("SELECT COUNT(*) FROM provider_objects p LEFT JOIN provider_dav_index i ON i.account_id=p.account_id AND i.origin=p.origin AND i.service=p.service AND i.collection=p.collection AND i.remote_id=p.remote_id WHERE p.account_id=?1 AND p.origin=?2 AND p.service=?3 AND p.collection=?4 AND (i.remote_id IS NULL OR i.valid=0)",params![scope.account_id,scope.origin,scope.service,scope.collection],|r| r.get(0))?)
    }

    pub fn provider_dav_unindexed_count(&self, scope:&ProviderScope) -> Result<i64> {
        Ok(self.conn.query_row("SELECT COUNT(*) FROM provider_objects p LEFT JOIN provider_dav_index i ON i.account_id=p.account_id AND i.origin=p.origin AND i.service=p.service AND i.collection=p.collection AND i.remote_id=p.remote_id WHERE p.account_id=?1 AND p.origin=?2 AND p.service=?3 AND p.collection=?4 AND i.remote_id IS NULL",params![scope.account_id,scope.origin,scope.service,scope.collection],|r| r.get(0))?)
    }
    /// Reclaim interrupted staging only while holding the on-disk writer lock.
    /// Preserve every staged observation in the capture outbox before removal.
    pub fn recover_abandoned_provider_staging(&self) -> Result<()> {
        if !self.protected_provider_storage() { return Err(StoreError::MissingStoreKey); }
        if self._lock.is_none() { return Err(StoreError::InvalidProgressiveSearch("exclusive provider writer lock required".into())); }
        let tx = self.conn.unchecked_transaction()?;
        let mut after = 0i64;
        loop {
            let row: Option<(i64,ProviderScope,ProviderObject)> = tx.query_row("SELECT rowid,account_id,origin,service,collection,remote_id,etag,raw FROM provider_stage_objects WHERE rowid>?1 ORDER BY rowid LIMIT 1",[after],|r| Ok((r.get(0)?,ProviderScope {account_id:r.get(1)?,origin:r.get(2)?,service:r.get(3)?,collection:r.get(4)?},ProviderObject {remote_id:r.get(5)?,etag:r.get(6)?,raw:r.get(7)?}))).optional()?;
            let Some((rowid,scope,object)) = row else { break; };
            after = rowid; capture_observation(&tx,&scope,&object)?;
        }
        tx.execute("DELETE FROM provider_stage_objects",[])?;
        tx.execute("DELETE FROM provider_stage_dav_index",[])?;
        tx.execute("DELETE FROM provider_stage_runs",[])?;
        tx.commit()?; Ok(())
    }
    /// Start an isolated SQLCipher-protected initial enumeration.
    pub fn begin_provider_staging(&self, scope: &ProviderScope, generation: &str) -> Result<()> {
        if !self.protected_provider_storage() { return Err(StoreError::MissingStoreKey); }
        if generation.is_empty() { return Err(StoreError::InvalidProgressiveSearch("empty generation".into())); }
        let tx = self.conn.unchecked_transaction()?;
        let base: Option<(Option<String>, bool)> = tx.query_row("SELECT cursor,accessible FROM provider_collections WHERE account_id=?1 AND origin=?2 AND service=?3 AND collection=?4",params![scope.account_id,scope.origin,scope.service,scope.collection],|r| Ok((r.get(0)?,r.get(1)?))).optional()?;
        tx.execute("INSERT INTO provider_stage_runs(account_id,origin,service,collection,generation,base_cursor,base_accessible) VALUES (?1,?2,?3,?4,?5,?6,?7)",params![scope.account_id,scope.origin,scope.service,scope.collection,generation,base.as_ref().and_then(|v| v.0.as_ref()),base.as_ref().map(|v| v.1)])?;
        tx.commit()?; Ok(())
    }

    /// Insert one bounded page; duplicate IDs fail rather than silently masking
    /// an inconsistent paged server enumeration.
    pub fn stage_provider_objects(&self, scope: &ProviderScope, generation: &str, objects: &[ProviderObject]) -> Result<()> {
        if !self.protected_provider_storage() { return Err(StoreError::MissingStoreKey); }
        let tx = self.conn.unchecked_transaction()?;
        let active: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM provider_stage_runs WHERE account_id=?1 AND origin=?2 AND service=?3 AND collection=?4 AND generation=?5)",params![scope.account_id,scope.origin,scope.service,scope.collection,generation],|r| r.get(0))?;
        if !active { return Err(StoreError::InvalidProgressiveSearch("staging generation not found".into())); }
        let mut bytes = 0usize;
        for object in objects {
            bytes = bytes.checked_add(object.raw.len()).ok_or(StoreError::ProviderObjectTooLarge)?;
            if object.remote_id.is_empty() || bytes > 64 * 1024 * 1024 { return Err(StoreError::ProviderObjectTooLarge); }
            capture_observation(&tx,scope,object)?;
            tx.execute("INSERT INTO provider_stage_objects(account_id,origin,service,collection,generation,remote_id,etag,raw) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",params![scope.account_id,scope.origin,scope.service,scope.collection,generation,object.remote_id,object.etag,object.raw])?;
        }
        tx.commit()?; Ok(())
    }

    /// Publish a fully enumerated generation with scoped diff, capture, events,
    /// and cursor atomically. At most one object's raw bytes are held at once.
    pub fn finish_provider_staging(&self, scope: &ProviderScope, generation: &str, cursor: &str) -> Result<()> {
        if !self.protected_provider_storage() { return Err(StoreError::MissingStoreKey); }
        let tx = self.conn.unchecked_transaction()?;
        let base: Option<(Option<String>,Option<bool>)> = tx.query_row("SELECT base_cursor,base_accessible FROM provider_stage_runs WHERE account_id=?1 AND origin=?2 AND service=?3 AND collection=?4 AND generation=?5",params![scope.account_id,scope.origin,scope.service,scope.collection,generation],|r| Ok((r.get(0)?,r.get(1)?))).optional()?;
        let base = base.ok_or_else(|| StoreError::InvalidProgressiveSearch("staging generation not found".into()))?;
        let current: Option<(Option<String>,bool)> = tx.query_row("SELECT cursor,accessible FROM provider_collections WHERE account_id=?1 AND origin=?2 AND service=?3 AND collection=?4",params![scope.account_id,scope.origin,scope.service,scope.collection],|r| Ok((r.get(0)?,r.get(1)?))).optional()?;
        if base != (current.as_ref().and_then(|v| v.0.clone()), current.as_ref().map(|v| v.1)) { return Err(StoreError::InvalidProgressiveSearch("provider scope changed during staging".into())); }
        if scope.service == "calendar" {
            let missing:bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM provider_stage_objects p LEFT JOIN provider_stage_dav_index i ON i.account_id=p.account_id AND i.origin=p.origin AND i.service=p.service AND i.collection=p.collection AND i.generation=p.generation AND i.remote_id=p.remote_id WHERE p.account_id=?1 AND p.origin=?2 AND p.service=?3 AND p.collection=?4 AND p.generation=?5 AND i.remote_id IS NULL)",params![scope.account_id,scope.origin,scope.service,scope.collection,generation],|r| r.get(0))?;
            if missing { return Err(StoreError::InvalidProgressiveSearch("staged DAV index incomplete".into())); }
        }
        let mut after = String::new();
        loop {
            let id: Option<String> = tx.query_row("SELECT p.remote_id FROM provider_objects p WHERE p.account_id=?1 AND p.origin=?2 AND p.service=?3 AND p.collection=?4 AND p.remote_id>?6 AND NOT EXISTS(SELECT 1 FROM provider_stage_objects s WHERE s.account_id=p.account_id AND s.origin=p.origin AND s.service=p.service AND s.collection=p.collection AND s.generation=?5 AND s.remote_id=p.remote_id) ORDER BY p.remote_id LIMIT 1",params![scope.account_id,scope.origin,scope.service,scope.collection,generation,after],|r| r.get(0)).optional()?;
            let Some(id) = id else { break; };
            after = id.clone(); apply_changes(&tx,scope,&[ProviderChange::Destroyed(id)],cursor)?;
        }
        after.clear();
        loop {
            let object: Option<ProviderObject> = tx.query_row("SELECT remote_id,etag,raw FROM provider_stage_objects WHERE account_id=?1 AND origin=?2 AND service=?3 AND collection=?4 AND generation=?5 AND remote_id>?6 ORDER BY remote_id LIMIT 1",params![scope.account_id,scope.origin,scope.service,scope.collection,generation,after],|r| Ok(ProviderObject {remote_id:r.get(0)?,etag:r.get(1)?,raw:r.get(2)?})).optional()?;
            let Some(object) = object else { break; };
            after = object.remote_id.clone();
            let index = if scope.service == "calendar" {
                Some(tx.query_row("SELECT valid,uid,kind,parent_uid FROM provider_stage_dav_index WHERE account_id=?1 AND origin=?2 AND service=?3 AND collection=?4 AND generation=?5 AND remote_id=?6",params![scope.account_id,scope.origin,scope.service,scope.collection,generation,after],|r| {
                    let valid:bool=r.get(0)?;
                    if valid { Ok(ProviderDavIndex::Valid { uid:r.get(1)?,kind:r.get(2)?,parent_uid:r.get(3)? }) } else { Ok(ProviderDavIndex::Invalid) }
                })?)
            } else { None };
            apply_changes(&tx,scope,&[ProviderChange::Upsert(object)],cursor)?;
            if let Some(index) = index { write_dav_index(&tx,scope,&after,&index)?; }
        }
        apply_changes(&tx,scope,&[],cursor)?;
        tx.execute("UPDATE provider_collections SET complete=1 WHERE account_id=?1 AND origin=?2 AND service=?3 AND collection=?4",params![scope.account_id,scope.origin,scope.service,scope.collection])?;
        tx.execute("DELETE FROM provider_stage_objects WHERE account_id=?1 AND origin=?2 AND service=?3 AND collection=?4 AND generation=?5",params![scope.account_id,scope.origin,scope.service,scope.collection,generation])?;
        tx.execute("DELETE FROM provider_stage_dav_index WHERE account_id=?1 AND origin=?2 AND service=?3 AND collection=?4 AND generation=?5",params![scope.account_id,scope.origin,scope.service,scope.collection,generation])?;
        tx.execute("DELETE FROM provider_stage_runs WHERE account_id=?1 AND origin=?2 AND service=?3 AND collection=?4 AND generation=?5",params![scope.account_id,scope.origin,scope.service,scope.collection,generation])?;
        tx.commit()?; Ok(())
    }

    /// Discard only the caller's incomplete generation; published state is untouched.
    pub fn abort_provider_staging(&self, scope: &ProviderScope, generation: &str) -> Result<()> {
        if !self.protected_provider_storage() { return Err(StoreError::MissingStoreKey); }
        let tx = self.conn.unchecked_transaction()?;
        let mut after = String::new();
        loop {
            let object: Option<ProviderObject> = tx.query_row("SELECT remote_id,etag,raw FROM provider_stage_objects WHERE account_id=?1 AND origin=?2 AND service=?3 AND collection=?4 AND generation=?5 AND remote_id>?6 ORDER BY remote_id LIMIT 1",params![scope.account_id,scope.origin,scope.service,scope.collection,generation,after],|r| Ok(ProviderObject {remote_id:r.get(0)?,etag:r.get(1)?,raw:r.get(2)?})).optional()?;
            let Some(object) = object else { break; };
            after = object.remote_id.clone(); capture_observation(&tx,scope,&object)?;
        }
        tx.execute("DELETE FROM provider_stage_objects WHERE account_id=?1 AND origin=?2 AND service=?3 AND collection=?4 AND generation=?5",params![scope.account_id,scope.origin,scope.service,scope.collection,generation])?;
        tx.execute("DELETE FROM provider_stage_dav_index WHERE account_id=?1 AND origin=?2 AND service=?3 AND collection=?4 AND generation=?5",params![scope.account_id,scope.origin,scope.service,scope.collection,generation])?;
        tx.execute("DELETE FROM provider_stage_runs WHERE account_id=?1 AND origin=?2 AND service=?3 AND collection=?4 AND generation=?5",params![scope.account_id,scope.origin,scope.service,scope.collection,generation])?;
        tx.commit()?; Ok(())
    }

    /// Apply a confirmed membership removal within the staged generation only.
    pub fn remove_provider_staged_object(&self, scope: &ProviderScope, generation: &str, id: &str) -> Result<()> {
        if !self.protected_provider_storage() { return Err(StoreError::MissingStoreKey); }
        let tx = self.conn.unchecked_transaction()?;
        let active: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM provider_stage_runs WHERE account_id=?1 AND origin=?2 AND service=?3 AND collection=?4 AND generation=?5)",params![scope.account_id,scope.origin,scope.service,scope.collection,generation],|r| r.get(0))?;
        if !active { return Err(StoreError::InvalidProgressiveSearch("staging generation not found".into())); }
        tx.execute("DELETE FROM provider_stage_objects WHERE account_id=?1 AND origin=?2 AND service=?3 AND collection=?4 AND generation=?5 AND remote_id=?6",params![scope.account_id,scope.origin,scope.service,scope.collection,generation,id])?;
        tx.execute("DELETE FROM provider_stage_dav_index WHERE account_id=?1 AND origin=?2 AND service=?3 AND collection=?4 AND generation=?5 AND remote_id=?6",params![scope.account_id,scope.origin,scope.service,scope.collection,generation,id])?;
        tx.commit()?; Ok(())
    }

    /// Replace a staged resource when a later ordered server page updates it.
    pub fn replace_provider_staged_object(&self, scope: &ProviderScope, generation: &str, object: &ProviderObject) -> Result<()> {
        if !self.protected_provider_storage() { return Err(StoreError::MissingStoreKey); }
        if object.remote_id.is_empty() || object.raw.len() > 64 * 1024 * 1024 { return Err(StoreError::ProviderObjectTooLarge); }
        let tx = self.conn.unchecked_transaction()?;
        let active: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM provider_stage_runs WHERE account_id=?1 AND origin=?2 AND service=?3 AND collection=?4 AND generation=?5)",params![scope.account_id,scope.origin,scope.service,scope.collection,generation],|r| r.get(0))?;
        if !active { return Err(StoreError::InvalidProgressiveSearch("staging generation not found".into())); }
        capture_observation(&tx,scope,object)?;
        tx.execute("DELETE FROM provider_stage_dav_index WHERE account_id=?1 AND origin=?2 AND service=?3 AND collection=?4 AND generation=?5 AND remote_id=?6",params![scope.account_id,scope.origin,scope.service,scope.collection,generation,object.remote_id])?;
        tx.execute("INSERT INTO provider_stage_objects(account_id,origin,service,collection,generation,remote_id,etag,raw) VALUES (?1,?2,?3,?4,?5,?6,?7,?8) ON CONFLICT(account_id,origin,service,collection,generation,remote_id) DO UPDATE SET etag=excluded.etag,raw=excluded.raw",params![scope.account_id,scope.origin,scope.service,scope.collection,generation,object.remote_id,object.etag,object.raw])?;
        tx.commit()?; Ok(())
    }
    /// Search only locally indexed provider mail metadata within the requested identity.
    pub fn search_provider_mail(&self, account: &str, origin: &str, query: &str, limit: usize) -> Result<Vec<(ProviderScope,ProviderObject)>> {
        let mut stmt = self.conn.prepare("SELECT p.collection,p.remote_id,p.etag,p.raw,length(p.raw) FROM provider_mail_fts f JOIN provider_objects p ON p.account_id=f.account_id AND p.origin=f.origin AND p.service='mail' AND p.collection=f.collection AND p.remote_id=f.remote_id WHERE provider_mail_fts MATCH ?1 AND f.account_id=?2 AND f.origin=?3 ORDER BY rank LIMIT ?4")?;
        let mut rows = stmt.query(params![query,account,origin,limit.min(100) as i64])?;
        let mut output = Vec::new(); let mut bytes = 0usize;
        while let Some(r) = rows.next()? {
            let size = r.get::<_,i64>(4)? as usize;
            if size > 8 * 1024 * 1024 { if output.is_empty() { return Err(StoreError::ProviderObjectTooLarge); } break; }
            if bytes + size > 8 * 1024 * 1024 { break; }
            bytes += size;
            output.push((ProviderScope { account_id:account.into(),origin:origin.into(),service:"mail".into(),collection:r.get(0)? }, ProviderObject { remote_id:r.get(1)?,etag:r.get(2)?,raw:r.get(3)? }));
        }
        Ok(output)
    }
    /// Read collection checkpoints and both outbox high-water marks consistently.
    pub fn provider_export_snapshot(&self, account: &str, origin: &str) -> Result<ProviderExportSnapshot> {
        let tx = self.conn.unchecked_transaction()?;
        let mut stmt = tx.prepare("SELECT service,collection,cursor,accessible,complete,observed_at FROM provider_collections WHERE account_id=?1 AND origin=?2 ORDER BY service,collection")?;
        let collections = stmt.query_map(params![account,origin], |r| Ok(ProviderCollectionSnapshot { scope:ProviderScope { account_id:account.into(), origin:origin.into(), service:r.get(0)?, collection:r.get(1)? }, cursor:r.get(2)?, accessible:r.get(3)?, complete:r.get(4)?, observed_at:r.get(5)? }))?.collect::<std::result::Result<Vec<_>, _>>()?;
        drop(stmt);
        let capture_highwater = tx.query_row("SELECT COALESCE(MAX(sequence),0) FROM provider_capture_outbox WHERE account_id=?1 AND origin=?2", params![account,origin], |r| r.get(0))?;
        let event_highwater = tx.query_row("SELECT COALESCE(MAX(sequence),0) FROM provider_events WHERE account_id=?1 AND origin=?2", params![account,origin], |r| r.get(0))?;
        tx.commit()?;
        Ok(ProviderExportSnapshot { collections, capture_highwater, event_highwater })
    }

    /// Apply a related set of page changes and checkpoints in one transaction.
    pub fn apply_provider_bundle(&self, pages: &[(&ProviderScope, &[ProviderChange], &str)]) -> Result<()> {
        if !self.protected_provider_storage() { return Err(StoreError::MissingStoreKey); }
        let tx = self.conn.unchecked_transaction()?;
        for (scope, changes, cursor) in pages { apply_changes(&tx,scope,changes,cursor)?; }
        tx.commit()?;
        Ok(())
    }

    /// Publish only fully enumerated related snapshots and checkpoints together.
    pub fn reconcile_provider_bundle(&self, pages: &[(&ProviderScope, &[ProviderObject], &str)]) -> Result<()> {
        if !self.protected_provider_storage() { return Err(StoreError::MissingStoreKey); }
        let tx = self.conn.unchecked_transaction()?;
        for (scope, objects, cursor) in pages { reconcile_snapshot(&tx,scope,objects,cursor)?; }
        tx.commit()?;
        Ok(())
    }
    pub fn claim_provider_mutation(&self, scope: &ProviderScope, operation: &str, kind: &str, intent: &str, digest: &str) -> Result<bool> {
        if !self.protected_provider_storage() { return Err(StoreError::MissingStoreKey); }
        Ok(self.conn.execute("INSERT OR IGNORE INTO provider_mutations(account_id,origin,service,remote_account,operation_id,kind,intent_json,intent_digest,state) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,'unknown')", params![scope.account_id,scope.origin,scope.service,scope.collection,operation,kind,intent,digest])? == 1)
    }

    pub fn provider_mutation(&self, scope: &ProviderScope, operation: &str) -> Result<Option<(String, String, Option<String>)>> {
        Ok(self.conn.query_row("SELECT intent_digest,state,result_id FROM provider_mutations WHERE account_id=?1 AND origin=?2 AND service=?3 AND remote_account=?4 AND operation_id=?5", params![scope.account_id,scope.origin,scope.service,scope.collection,operation], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?)
    }

    pub fn finish_provider_mutation(&self, scope: &ProviderScope, operation: &str, result_id: Option<&str>) -> Result<bool> {
        let state = if result_id.is_some() { "confirmed" } else { "rejected" };
        Ok(self.conn.execute("UPDATE provider_mutations SET state=?6,result_id=COALESCE(?7,result_id) WHERE account_id=?1 AND origin=?2 AND service=?3 AND remote_account=?4 AND operation_id=?5 AND state='unknown'", params![scope.account_id,scope.origin,scope.service,scope.collection,operation,state,result_id])? == 1)
    }

    /// Persist a native result identity while verification is still pending.
    pub fn checkpoint_provider_mutation(&self, scope:&ProviderScope, operation:&str, expected_digest:&str, native_id:&str) -> Result<bool> {
        if !self.protected_provider_storage() { return Err(StoreError::MissingStoreKey); }
        if native_id.is_empty() { return Err(StoreError::IllegalTransition("empty native result identity".into())); }
        Ok(self.conn.execute("UPDATE provider_mutations SET result_id=?7 WHERE account_id=?1 AND origin=?2 AND service=?3 AND remote_account=?4 AND operation_id=?5 AND intent_digest=?6 AND state='unknown' AND (result_id IS NULL OR result_id=?7)",params![scope.account_id,scope.origin,scope.service,scope.collection,operation,expected_digest,native_id])? == 1)
    }

    pub fn provider_mutation_intent(&self, scope: &ProviderScope, operation: &str) -> Result<Option<(String,String,String,String,Option<String>)>> {
        Ok(self.conn.query_row("SELECT kind,intent_json,intent_digest,state,result_id FROM provider_mutations WHERE account_id=?1 AND origin=?2 AND service=?3 AND remote_account=?4 AND operation_id=?5", params![scope.account_id,scope.origin,scope.service,scope.collection,operation], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).optional()?)
    }

    pub fn conflict_provider_mutation(&self, scope: &ProviderScope, operation: &str) -> Result<bool> {
        Ok(self.conn.execute("UPDATE provider_mutations SET state='conflict' WHERE account_id=?1 AND origin=?2 AND service=?3 AND remote_account=?4 AND operation_id=?5 AND state='unknown'", params![scope.account_id,scope.origin,scope.service,scope.collection,operation])? == 1)
    }
    /// Claims an effect before dispatch. An existing operation is never resent.
    pub fn claim_provider_submission(&self, op: &ProviderSubmission) -> Result<bool> {
        if !self.protected_provider_storage() { return Err(StoreError::MissingStoreKey); }
        Ok(self.conn.execute("INSERT OR IGNORE INTO provider_submissions(account_id,origin,remote_account,operation_id,draft_id,identity_id,email_state,snapshot_digest,creation_id,intent_json,state) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,'unknown')", params![op.scope.account_id,op.scope.origin,op.scope.collection,op.operation_id,op.draft_id,op.identity_id,op.email_state,op.snapshot_digest,op.creation_id,op.intent_json])? == 1)
    }

    pub fn provider_submission(&self, scope: &ProviderScope, operation_id: &str) -> Result<Option<ProviderSubmission>> {
        Ok(self.conn.query_row("SELECT draft_id,identity_id,email_state,snapshot_digest,creation_id,state,submission_id,intent_json,sent_update FROM provider_submissions WHERE account_id=?1 AND origin=?2 AND remote_account=?3 AND operation_id=?4", params![scope.account_id,scope.origin,scope.collection,operation_id], |r| Ok(ProviderSubmission { scope:scope.clone(), operation_id:operation_id.into(), draft_id:r.get(0)?, identity_id:r.get(1)?, email_state:r.get(2)?, snapshot_digest:r.get(3)?, creation_id:r.get(4)?, state:r.get(5)?, submission_id:r.get(6)?, intent_json:r.get(7)?, sent_update:r.get(8)? })).optional()?)
    }

    pub fn confirm_provider_submission(&self, scope: &ProviderScope, operation_id: &str, submission_id: &str, sent_update: &str) -> Result<bool> {
        Ok(self.conn.execute("UPDATE provider_submissions SET state='confirmed',submission_id=?5,sent_update=?6 WHERE account_id=?1 AND origin=?2 AND remote_account=?3 AND operation_id=?4 AND state='unknown'", params![scope.account_id,scope.origin,scope.collection,operation_id,submission_id,sent_update])? == 1)
    }

    pub fn reject_provider_submission(&self, scope: &ProviderScope, operation_id: &str) -> Result<bool> {
        Ok(self.conn.execute("UPDATE provider_submissions SET state='rejected' WHERE account_id=?1 AND origin=?2 AND remote_account=?3 AND operation_id=?4 AND state='unknown'", params![scope.account_id,scope.origin,scope.collection,operation_id])? == 1)
    }
    /// List immutable observed versions for durable export. Listing does not mark
    /// a version protected and never removes it.
    pub fn provider_captures(&self, after: i64, limit: usize) -> Result<Vec<ProviderCapture>> {
        let mut stmt = self.conn.prepare("SELECT sequence,account_id,origin,service,collection,remote_id,etag,raw,digest,version_key,length(raw) FROM provider_capture_outbox WHERE sequence>?1 ORDER BY sequence LIMIT ?2")?;
        let mut rows = stmt.query(params![after, limit.min(1000) as i64])?;
        let mut output = Vec::new(); let mut bytes = 0usize;
        while let Some(r) = rows.next()? {
            let size = r.get::<_, i64>(10)? as usize;
            if size > 64 * 1024 * 1024 {
                if output.is_empty() { return Err(StoreError::ProviderObjectTooLarge); }
                break;
            }
            if bytes + size > 64 * 1024 * 1024 { break; }
            bytes += size;
            output.push(ProviderCapture { sequence:r.get(0)?, scope: ProviderScope { account_id:r.get(1)?, origin:r.get(2)?, service:r.get(3)?, collection:r.get(4)? }, object: ProviderObject { remote_id:r.get(5)?, etag:r.get(6)?, raw:r.get(7)? }, digest:r.get(8)?, version_key:r.get(9)? });
        }
        Ok(output)
    }

    /// Account-bound capture traversal for scoped headless exports.
    pub fn provider_captures_for(&self, account: &str, origin: &str, after: i64, limit: usize) -> Result<Vec<ProviderCapture>> {
        let mut stmt = self.conn.prepare("SELECT sequence,account_id,origin,service,collection,remote_id,etag,raw,digest,version_key,length(raw) FROM provider_capture_outbox WHERE account_id=?1 AND origin=?2 AND sequence>?3 ORDER BY sequence LIMIT ?4")?;
        let mut rows = stmt.query(params![account,origin,after,limit.min(1000) as i64])?;
        let mut output = Vec::new(); let mut bytes = 0usize;
        while let Some(r) = rows.next()? {
            let size = r.get::<_, i64>(10)? as usize;
            if size > 64 * 1024 * 1024 { if output.is_empty() { return Err(StoreError::ProviderObjectTooLarge); } break; }
            if bytes + size > 64 * 1024 * 1024 { break; }
            bytes += size;
            output.push(ProviderCapture { sequence:r.get(0)?, scope:ProviderScope { account_id:r.get(1)?, origin:r.get(2)?, service:r.get(3)?, collection:r.get(4)? }, object:ProviderObject { remote_id:r.get(5)?, etag:r.get(6)?, raw:r.get(7)? }, digest:r.get(8)?, version_key:r.get(9)? });
        }
        Ok(output)
    }

    /// Ordered scoped membership changes; `None` denotes removal from scope.
    pub fn provider_events(&self, after: i64, limit: usize) -> Result<Vec<ProviderEvent>> {
        let mut stmt = self.conn.prepare("SELECT sequence,account_id,origin,service,collection,remote_id,version_key,cursor FROM provider_events WHERE sequence > ?1 ORDER BY sequence LIMIT ?2")?;
        let rows = stmt.query_map(params![after,limit.min(1000) as i64], |r| Ok(ProviderEvent {
            sequence:r.get(0)?, scope: ProviderScope { account_id:r.get(1)?,origin:r.get(2)?,service:r.get(3)?,collection:r.get(4)? }, remote_id:r.get(5)?,version_key:r.get(6)?,cursor:r.get(7)?,
        }))?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn provider_events_for(&self, account: &str, origin: &str, after: i64, limit: usize) -> Result<Vec<ProviderEvent>> {
        let mut stmt = self.conn.prepare("SELECT sequence,account_id,origin,service,collection,remote_id,version_key,cursor FROM provider_events WHERE account_id=?1 AND origin=?2 AND sequence>?3 ORDER BY sequence LIMIT ?4")?;
        let rows = stmt.query_map(params![account,origin,after,limit.min(1000) as i64], |r| Ok(ProviderEvent { sequence:r.get(0)?, scope:ProviderScope { account_id:r.get(1)?,origin:r.get(2)?,service:r.get(3)?,collection:r.get(4)? }, remote_id:r.get(5)?,version_key:r.get(6)?,cursor:r.get(7)? }))?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }
    /// Atomically persist a complete received page and its corresponding cursor.
    pub fn apply_provider_page(&self, scope: &ProviderScope, changes: &[ProviderChange], cursor: &str) -> Result<()> {
        if !self.protected_provider_storage() { return Err(StoreError::MissingStoreKey); }
        let tx = self.conn.unchecked_transaction()?;
        apply_changes(&tx, scope, changes, cursor)?;
        tx.commit()?;
        Ok(())
    }

    /// Replace only this collection after a complete, successful enumeration.
    pub fn reconcile_provider_snapshot(&self, scope: &ProviderScope, objects: &[ProviderObject], cursor: &str) -> Result<()> {
        if !self.protected_provider_storage() { return Err(StoreError::MissingStoreKey); }
        let tx = self.conn.unchecked_transaction()?;
        reconcile_snapshot(&tx,scope,objects,cursor)?;
        tx.commit()?;
        Ok(())
    }

    pub fn provider_cursor(&self, scope: &ProviderScope) -> Result<Option<String>> {
        Ok(self.conn.query_row("SELECT cursor FROM provider_collections WHERE account_id=?1 AND origin=?2 AND service=?3 AND collection=?4", params![scope.account_id,scope.origin,scope.service,scope.collection], |r| r.get::<_, Option<String>>(0)).optional()?.flatten())
    }

    /// Mark an explicitly staged initial enumeration complete only if its current
    /// checkpoint is still exactly the one the caller finished enumerating.
    pub fn mark_provider_complete(&self, scope: &ProviderScope, expected_cursor: &str, final_cursor: &str) -> Result<bool> {
        if !self.protected_provider_storage() { return Err(StoreError::MissingStoreKey); }
        Ok(self.conn.execute("UPDATE provider_collections SET complete=1,cursor=?6,observed_at=CAST(strftime('%s','now') AS INTEGER) WHERE account_id=?1 AND origin=?2 AND service=?3 AND collection=?4 AND cursor=?5 AND complete=0",params![scope.account_id,scope.origin,scope.service,scope.collection,expected_cursor,final_cursor])? == 1)
    }

    /// CAS transition to an incomplete staged reconciliation; existing objects
    /// and previously published export generations remain untouched.
    pub fn begin_provider_reconcile(&self, scope: &ProviderScope, previous_cursor: Option<&str>, marker: &str) -> Result<bool> {
        if !self.protected_provider_storage() { return Err(StoreError::MissingStoreKey); }
        if marker.is_empty() { return Err(StoreError::InvalidProgressiveSearch("empty reconcile marker".into())); }
        let tx = self.conn.unchecked_transaction()?;
        let count = if let Some(previous) = previous_cursor {
            tx.execute("UPDATE provider_collections SET cursor=?6,complete=0 WHERE account_id=?1 AND origin=?2 AND service=?3 AND collection=?4 AND cursor=?5",params![scope.account_id,scope.origin,scope.service,scope.collection,previous,marker])?
        } else {
            tx.execute("INSERT OR IGNORE INTO provider_collections(account_id,origin,service,collection,cursor,complete) VALUES (?1,?2,?3,?4,?5,0)",params![scope.account_id,scope.origin,scope.service,scope.collection,marker])?
        };
        tx.commit()?; Ok(count == 1)
    }

    /// Local UTC Unix time at which the cursor and page last committed.
    pub fn provider_observed_at(&self, scope: &ProviderScope) -> Result<Option<i64>> {
        Ok(self.conn.query_row("SELECT observed_at FROM provider_collections WHERE account_id=?1 AND origin=?2 AND service=?3 AND collection=?4", params![scope.account_id,scope.origin,scope.service,scope.collection], |r| r.get::<_, Option<i64>>(0)).optional()?.flatten())
    }

    pub fn provider_object(&self, scope: &ProviderScope, id: &str) -> Result<Option<ProviderObject>> {
        Ok(self.conn.query_row("SELECT remote_id,etag,raw FROM provider_objects WHERE account_id=?1 AND origin=?2 AND service=?3 AND collection=?4 AND remote_id=?5", params![scope.account_id,scope.origin,scope.service,scope.collection,id], |r| Ok(ProviderObject { remote_id:r.get(0)?, etag:r.get(1)?, raw:r.get(2)? })).optional()?)
    }

    pub fn provider_objects(&self, scope: &ProviderScope) -> Result<Vec<ProviderObject>> {
        let mut stmt = self.conn.prepare("SELECT remote_id,etag,raw FROM provider_objects WHERE account_id=?1 AND origin=?2 AND service=?3 AND collection=?4 ORDER BY remote_id")?;
        let rows = stmt.query_map(params![scope.account_id,scope.origin,scope.service,scope.collection], |r| Ok(ProviderObject { remote_id:r.get(0)?, etag:r.get(1)?, raw:r.get(2)? }))?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// List known scopes without opening a provider connection.
    pub fn provider_scopes(&self, account: &str, origin: &str, service: &str) -> Result<Vec<ProviderScope>> {
        let mut stmt = self.conn.prepare("SELECT collection FROM provider_collections WHERE account_id=?1 AND origin=?2 AND service=?3 ORDER BY collection")?;
        let rows = stmt.query_map(params![account,origin,service], |r| Ok(ProviderScope { account_id:account.into(), origin:origin.into(), service:service.into(), collection:r.get(0)? }))?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Keyset pagination avoids rescanning or loading an entire collection.
    pub fn provider_objects_page(&self, scope: &ProviderScope, after: &str, limit: usize) -> Result<Vec<ProviderObject>> {
        let mut stmt = self.conn.prepare("SELECT remote_id,etag,raw,length(raw) FROM provider_objects WHERE account_id=?1 AND origin=?2 AND service=?3 AND collection=?4 AND remote_id>?5 ORDER BY remote_id LIMIT ?6")?;
        let mut rows = stmt.query(params![scope.account_id,scope.origin,scope.service,scope.collection,after,limit.min(200) as i64])?;
        let mut output = Vec::new(); let mut bytes = 0usize;
        while let Some(r) = rows.next()? {
            let size = r.get::<_, i64>(3)? as usize;
            if size > 8 * 1024 * 1024 {
                if output.is_empty() { return Err(StoreError::ProviderObjectTooLarge); }
                break;
            }
            if bytes + size > 8 * 1024 * 1024 { break; }
            bytes += size;
            output.push(ProviderObject { remote_id:r.get(0)?, etag:r.get(1)?, raw:r.get(2)? });
        }
        Ok(output)
    }

    pub fn set_provider_accessible(&self, scope: &ProviderScope, accessible: bool) -> Result<()> {
        self.conn.execute("INSERT INTO provider_collections(account_id,origin,service,collection,accessible) VALUES (?1,?2,?3,?4,?5) ON CONFLICT(account_id,origin,service,collection) DO UPDATE SET accessible=excluded.accessible", params![scope.account_id,scope.origin,scope.service,scope.collection,accessible])?;
        Ok(())
    }

    pub fn provider_accessible(&self, scope: &ProviderScope) -> Result<Option<bool>> {
        Ok(self.conn.query_row("SELECT accessible FROM provider_collections WHERE account_id=?1 AND origin=?2 AND service=?3 AND collection=?4", params![scope.account_id,scope.origin,scope.service,scope.collection], |r| r.get(0)).optional()?)
    }
}

fn reconcile_snapshot(tx: &rusqlite::Transaction<'_>, scope: &ProviderScope, objects: &[ProviderObject], cursor: &str) -> Result<()> {
    let incoming: std::collections::HashSet<&str> = objects.iter().map(|o| o.remote_id.as_str()).collect();
    let mut stmt = tx.prepare("SELECT remote_id FROM provider_objects WHERE account_id=?1 AND origin=?2 AND service=?3 AND collection=?4")?;
    let existing = stmt.query_map(params![scope.account_id,scope.origin,scope.service,scope.collection], |r| r.get::<_, String>(0))?.collect::<std::result::Result<Vec<_>, _>>()?;
    drop(stmt);
    let mut changes: Vec<ProviderChange> = existing.into_iter().filter(|id| !incoming.contains(id.as_str())).map(ProviderChange::Destroyed).collect();
    changes.extend(objects.iter().cloned().map(ProviderChange::Upsert));
    apply_changes(tx, scope, &changes, cursor)?;
    tx.execute("UPDATE provider_collections SET complete=1 WHERE account_id=?1 AND origin=?2 AND service=?3 AND collection=?4",params![scope.account_id,scope.origin,scope.service,scope.collection])?;
    Ok(())
}

fn apply_changes(tx: &rusqlite::Transaction<'_>, scope: &ProviderScope, changes: &[ProviderChange], cursor: &str) -> Result<()> {
        for change in changes {
            let id = match change { ProviderChange::Upsert(object) => &object.remote_id, ProviderChange::Destroyed(id) => id };
            if scope.service == "calendar" { tx.execute("DELETE FROM provider_dav_index WHERE account_id=?1 AND origin=?2 AND service=?3 AND collection=?4 AND remote_id=?5",params![scope.account_id,scope.origin,scope.service,scope.collection,id])?; }
            let previous = tx.query_row("SELECT etag,raw FROM provider_objects WHERE account_id=?1 AND origin=?2 AND service=?3 AND collection=?4 AND remote_id=?5", params![scope.account_id,scope.origin,scope.service,scope.collection,id], |r| Ok((r.get::<_, Option<String>>(0)?,r.get::<_, Vec<u8>>(1)?))).optional()?;
            if let Some((etag, raw)) = previous {
                let digest = format!("{:x}", Sha256::digest(&raw));
                let version = capture_version(etag.as_deref(), &raw);
                tx.execute("INSERT OR IGNORE INTO provider_capture_outbox(account_id,origin,service,collection,remote_id,digest,version_key,etag,raw) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)", params![scope.account_id,scope.origin,scope.service,scope.collection,id,digest,version,etag,raw])?;
            }
            match change {
                ProviderChange::Upsert(object) => {
                    let digest = format!("{:x}", Sha256::digest(&object.raw));
                    let version = capture_version(object.etag.as_deref(), &object.raw);
                    tx.execute("INSERT OR IGNORE INTO provider_capture_outbox(account_id,origin,service,collection,remote_id,digest,version_key,etag,raw) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)", params![scope.account_id,scope.origin,scope.service,scope.collection,id,digest,version,object.etag,object.raw])?;
                    tx.execute("INSERT INTO provider_events(account_id,origin,service,collection,remote_id,kind,version_key,cursor) VALUES (?1,?2,?3,?4,?5,'upsert',?6,?7)", params![scope.account_id,scope.origin,scope.service,scope.collection,id,version,cursor])?;
                    tx.execute("INSERT INTO provider_objects VALUES (?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(account_id,origin,service,collection,remote_id) DO UPDATE SET etag=excluded.etag,raw=excluded.raw", params![scope.account_id,scope.origin,scope.service,scope.collection,object.remote_id,object.etag,object.raw])?;
                    if scope.service == "mail" {
                        tx.execute("DELETE FROM provider_mail_fts WHERE account_id=?1 AND origin=?2 AND collection=?3 AND remote_id=?4",params![scope.account_id,scope.origin,scope.collection,object.remote_id])?;
                        if let Ok(mail) = serde_json::from_slice::<serde_json::Value>(&object.raw) {
                        let mut fields = Vec::new();
                        for key in ["subject", "preview"] { if let Some(value) = mail[key].as_str() { fields.push(value.to_owned()); } }
                        for key in ["from", "to", "cc"] { if let Some(entries) = mail[key].as_array() { for entry in entries { for field in ["name", "email"] { if let Some(value) = entry[field].as_str() { fields.push(value.to_owned()); } } } } }
                        tx.execute("INSERT INTO provider_mail_fts(account_id,origin,collection,remote_id,searchable) VALUES (?1,?2,?3,?4,?5)",params![scope.account_id,scope.origin,scope.collection,object.remote_id,fields.join(" ")])?;
                        }
                    }
                }
                ProviderChange::Destroyed(id) => {
                    tx.execute("INSERT INTO provider_events(account_id,origin,service,collection,remote_id,kind,version_key,cursor) VALUES (?1,?2,?3,?4,?5,'removed',NULL,?6)", params![scope.account_id,scope.origin,scope.service,scope.collection,id,cursor])?;
                    tx.execute("DELETE FROM provider_objects WHERE account_id=?1 AND origin=?2 AND service=?3 AND collection=?4 AND remote_id=?5", params![scope.account_id,scope.origin,scope.service,scope.collection,id])?;
                    if scope.service == "mail" { tx.execute("DELETE FROM provider_mail_fts WHERE account_id=?1 AND origin=?2 AND collection=?3 AND remote_id=?4",params![scope.account_id,scope.origin,scope.collection,id])?; }
                }
            }
        }
        tx.execute("INSERT INTO provider_collections(account_id,origin,service,collection,cursor,observed_at,accessible) VALUES (?1,?2,?3,?4,?5,CAST(strftime('%s','now') AS INTEGER),1) ON CONFLICT(account_id,origin,service,collection) DO UPDATE SET cursor=excluded.cursor,observed_at=excluded.observed_at,accessible=1", params![scope.account_id,scope.origin,scope.service,scope.collection,cursor])?;
        Ok(())
}

fn dav_index_fields(index:&ProviderDavIndex) -> Result<(bool,Option<&str>,Option<&str>,Option<&str>)> {
    match index {
        ProviderDavIndex::Invalid => Ok((false,None,None,None)),
        ProviderDavIndex::Valid { uid,kind,parent_uid } => {
            if uid.is_empty() || uid.len()>4096 || !matches!(kind.as_str(),"event"|"task") || (kind == "event" && parent_uid.is_some()) || parent_uid.as_ref().is_some_and(|p| p.is_empty() || p.len()>4096) { return Err(StoreError::InvalidProgressiveSearch("invalid DAV identity index".into())); }
            Ok((true,Some(uid),Some(kind),parent_uid.as_deref()))
        }
    }
}

fn validate_session(scope:&ProviderScope, session:&str) -> Result<()> {
    if scope.account_id.is_empty() || scope.origin.is_empty() || session.is_empty() || session.len()>128 { return Err(StoreError::IllegalTransition("invalid session identity".into())); }
    Ok(())
}

fn write_dav_index(tx:&rusqlite::Transaction<'_>, scope:&ProviderScope, remote_id:&str, index:&ProviderDavIndex) -> Result<()> {
    let (valid,uid,kind,parent) = dav_index_fields(index)?;
    tx.execute("INSERT INTO provider_dav_index(account_id,origin,service,collection,remote_id,valid,uid,kind,parent_uid) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",params![scope.account_id,scope.origin,scope.service,scope.collection,remote_id,valid,uid,kind,parent])?;
    Ok(())
}

fn capture_version(etag: Option<&str>, raw: &[u8]) -> String {
    let mut hash = Sha256::new();
    match etag {
        Some(value) => { hash.update([1]); hash.update((value.len() as u64).to_be_bytes()); hash.update(value.as_bytes()); }
        None => hash.update([0]),
    }
    hash.update(raw);
    format!("{:x}", hash.finalize())
}

fn capture_observation(tx: &rusqlite::Transaction<'_>, scope: &ProviderScope, object: &ProviderObject) -> Result<()> {
    let digest = format!("{:x}", Sha256::digest(&object.raw));
    let version = capture_version(object.etag.as_deref(),&object.raw);
    tx.execute("INSERT OR IGNORE INTO provider_capture_outbox(account_id,origin,service,collection,remote_id,digest,version_key,etag,raw) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",params![scope.account_id,scope.origin,scope.service,scope.collection,object.remote_id,digest,version,object.etag,object.raw])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_cursor_and_observed_versions_survive_access_loss_and_deletion() {
        let store = Store::open_in_memory().unwrap();
        let scope = ProviderScope { account_id: "a".into(), origin: "https://one.example".into(), service: "calendar".into(), collection: "/cal/".into() };
        let other = ProviderScope { origin: "https://two.example".into(), ..scope.clone() };
        let object = ProviderObject { remote_id: "/cal/1.ics".into(), etag: Some("v1".into()), raw: b"original".to_vec() };
        store.apply_provider_page(&scope, &[ProviderChange::Upsert(object.clone())], "one").unwrap();
        store.set_provider_accessible(&scope, false).unwrap();
        assert_eq!(store.provider_accessible(&scope).unwrap(), Some(false));
        assert_eq!(store.provider_object(&scope, &object.remote_id).unwrap(), Some(object.clone()));
        assert_eq!(store.provider_cursor(&other).unwrap(), None);
        store.apply_provider_page(&scope, &[ProviderChange::Destroyed(object.remote_id.clone())], "two").unwrap();
        assert_eq!(store.provider_cursor(&scope).unwrap().as_deref(), Some("two"));
        assert_eq!(store.provider_object(&scope, &object.remote_id).unwrap(), None);
        assert_eq!(store.provider_captures(0, 10).unwrap()[0].object.raw, object.raw);
        let events = store.provider_events(0, 10).unwrap();
        assert!(events[0].version_key.is_some());
        assert!(events[1].version_key.is_none());
        assert_eq!(events[1].cursor, "two");
    }

    #[test]
    fn failed_page_rolls_back_objects_and_cursor() {
        let store = Store::open_in_memory().unwrap();
        let scope = ProviderScope { account_id: "a".into(), origin: "https://one.example".into(), service: "calendar".into(), collection: "/cal/".into() };
        store.apply_provider_page(&scope, &[], "before").unwrap();
        store.conn.execute_batch("CREATE TRIGGER fail_capture BEFORE INSERT ON provider_capture_outbox BEGIN SELECT RAISE(ABORT,'full'); END;").unwrap();
        let object = ProviderObject { remote_id: "x".into(), etag: None, raw: b"body".to_vec() };
        assert!(store.apply_provider_page(&scope, &[ProviderChange::Upsert(object)], "after").is_err());
        assert_eq!(store.provider_cursor(&scope).unwrap().as_deref(), Some("before"));
        assert!(store.provider_objects(&scope).unwrap().is_empty());
        assert!(store.provider_events(0, 10).unwrap().is_empty());
    }

    #[test]
    fn completed_snapshot_reconciles_only_the_selected_scope() {
        let store = Store::open_in_memory().unwrap();
        let scope = ProviderScope { account_id: "a".into(), origin: "https://one.example".into(), service: "contacts".into(), collection: "/a/".into() };
        let other = ProviderScope { collection: "/b/".into(), ..scope.clone() };
        let object = ProviderObject { remote_id: "one".into(), etag: None, raw: b"old".to_vec() };
        store.apply_provider_page(&scope, &[ProviderChange::Upsert(object.clone())], "old").unwrap();
        store.apply_provider_page(&other, &[ProviderChange::Upsert(object.clone())], "other").unwrap();
        store.reconcile_provider_snapshot(&scope, &[], "new").unwrap();
        assert!(store.provider_objects(&scope).unwrap().is_empty());
        assert_eq!(store.provider_objects(&other).unwrap(), vec![object]);
        assert_eq!(store.provider_cursor(&scope).unwrap().as_deref(), Some("new"));
        assert_eq!(store.provider_captures(0, 10).unwrap().len(), 2);
    }

    #[test]
    fn submission_claim_is_durable_and_never_claimed_twice() {
        let store = Store::open_in_memory().unwrap();
        let scope = ProviderScope { account_id:"a".into(), origin:"https://one.example".into(), service:"mail".into(), collection:"remote".into() };
        let op = ProviderSubmission { scope:scope.clone(), operation_id:"approval".into(), draft_id:"draft".into(), identity_id:"identity".into(), email_state:"state".into(), snapshot_digest:"digest".into(), creation_id:"creation".into(), intent_json:"{}".into(), state:"unknown".into(), submission_id:None, sent_update:None };
        assert!(store.claim_provider_submission(&op).unwrap());
        assert!(!store.claim_provider_submission(&op).unwrap());
        let mut alternate = op.clone(); alternate.operation_id = "other-approval".into();
        assert!(!store.claim_provider_submission(&alternate).unwrap());
        assert_eq!(store.provider_submission(&scope,"approval").unwrap().unwrap().state, "unknown");
        assert!(store.confirm_provider_submission(&scope,"approval","submission","applied").unwrap());
        assert!(!store.confirm_provider_submission(&scope,"approval","another","applied").unwrap());
        assert_eq!(store.provider_submission(&scope,"approval").unwrap().unwrap().submission_id.as_deref(),Some("submission"));
    }

    #[test]
    fn rejected_submission_allows_new_approval_but_unknown_or_confirmed_do_not() {
        let store = Store::open_in_memory().unwrap();
        let scope = ProviderScope { account_id:"a".into(),origin:"https://example.test".into(),service:"mail".into(),collection:"remote".into() };
        let mut op = ProviderSubmission { scope:scope.clone(),operation_id:"a".into(),draft_id:"draft".into(),identity_id:"identity".into(),email_state:"state".into(),snapshot_digest:"digest".into(),creation_id:"creation".into(),intent_json:"{}".into(),state:"unknown".into(),submission_id:None,sent_update:None };
        assert!(store.claim_provider_submission(&op).unwrap());
        op.operation_id = "b".into();
        assert!(!store.claim_provider_submission(&op).unwrap());
        assert!(store.reject_provider_submission(&scope,"a").unwrap());
        assert!(store.claim_provider_submission(&op).unwrap());
        assert!(store.confirm_provider_submission(&scope,"b","native","applied").unwrap());
        op.operation_id = "c".into();
        assert!(!store.claim_provider_submission(&op).unwrap());
        assert_eq!(store.provider_submission(&scope,"b").unwrap().unwrap().state,"confirmed");
    }

    #[test]
    fn unresolved_target_mutation_blocks_new_operation_until_rejected() {
        let store=Store::open_in_memory().unwrap();
        let scope=ProviderScope {account_id:"a".into(),origin:"https://example.test".into(),service:"mail".into(),collection:"remote".into()};
        assert!(store.claim_provider_target_mutation(&scope,"first","set_read","id","{}","digest").unwrap());
        assert!(!store.claim_provider_target_mutation(&scope,"second","set_read","id","{}","digest").unwrap());
        assert!(store.finish_provider_mutation(&scope,"first",None).unwrap());
        assert!(store.claim_provider_target_mutation(&scope,"second","set_read","id","{}","digest").unwrap());
    }

    #[test]
    fn persistent_plain_store_rejects_provider_content() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_plain(dir.path().join("plain.db")).unwrap();
        let scope = ProviderScope { account_id:"a".into(), origin:"https://one.example".into(), service:"contacts".into(), collection:"/a/".into() };
        let object = ProviderObject { remote_id:"one".into(), etag:None, raw:b"private".to_vec() };
        assert!(matches!(store.apply_provider_page(&scope, &[ProviderChange::Upsert(object)], "cursor"), Err(StoreError::MissingStoreKey)));
        assert!(store.provider_objects(&scope).unwrap().is_empty());
    }

    #[test]
    fn mail_metadata_search_is_scoped_and_tracks_updates_and_deletion() {
        let store = Store::open_in_memory().unwrap();
        let scope = ProviderScope { account_id:"a".into(), origin:"https://one.example".into(), service:"mail".into(), collection:"remote".into() };
        let item = ProviderObject { remote_id:"id".into(), etag:None, raw:br#"{"subject":"Quarterly report","from":[{"name":"Ada","email":"a@example.test"}]}"#.to_vec() };
        store.apply_provider_page(&scope,&[ProviderChange::Upsert(item)],"s1").unwrap();
        assert_eq!(store.search_provider_mail("a","https://one.example","quarterly",10).unwrap().len(),1);
        assert!(store.search_provider_mail("a","https://other.example","quarterly",10).unwrap().is_empty());
        store.apply_provider_page(&scope,&[ProviderChange::Destroyed("id".into())],"s2").unwrap();
        assert!(store.search_provider_mail("a","https://one.example","quarterly",10).unwrap().is_empty());
    }

    #[test]
    fn staged_reconcile_preserves_old_state_until_atomic_commit_and_rejects_stale_generation() {
        let store = Store::open_in_memory().unwrap();
        let scope = ProviderScope { account_id:"a".into(), origin:"https://one.example".into(), service:"contacts".into(), collection:"c".into() };
        let old = ProviderObject { remote_id:"old".into(),etag:Some("v1".into()),raw:b"old".to_vec() };
        let new = ProviderObject { remote_id:"new".into(),etag:Some("v2".into()),raw:b"new".to_vec() };
        store.apply_provider_page(&scope,&[ProviderChange::Upsert(old.clone())],"s1").unwrap();
        store.begin_provider_staging(&scope,"gen1").unwrap();
        store.stage_provider_objects(&scope,"gen1",std::slice::from_ref(&new)).unwrap();
        assert_eq!(store.provider_objects(&scope).unwrap(),vec![old.clone()]);
        assert!(store.provider_captures_for("a","https://one.example",0,10).unwrap().iter().any(|capture| capture.object == new));
        store.set_provider_accessible(&scope,false).unwrap();
        assert!(store.finish_provider_staging(&scope,"gen1","s2").is_err());
        assert_eq!(store.provider_objects(&scope).unwrap(),vec![old.clone()]);
        assert_eq!(store.provider_cursor(&scope).unwrap().as_deref(),Some("s1"));
        store.abort_provider_staging(&scope,"gen1").unwrap();
        store.set_provider_accessible(&scope,true).unwrap();
        store.begin_provider_staging(&scope,"gen2").unwrap();
        store.stage_provider_objects(&scope,"gen2",std::slice::from_ref(&new)).unwrap();
        assert!(store.stage_provider_objects(&scope,"gen2",std::slice::from_ref(&new)).is_err());
        store.finish_provider_staging(&scope,"gen2","s2").unwrap();
        assert_eq!(store.provider_objects(&scope).unwrap(),vec![new]);
        assert_eq!(store.provider_cursor(&scope).unwrap().as_deref(),Some("s2"));
        assert!(store.provider_export_snapshot("a","https://one.example").unwrap().collections[0].complete);
    }

    #[test]
    fn dav_index_is_atomic_scoped_and_missing_index_fails_closed() {
        let store = Store::open_in_memory().unwrap();
        let scope = ProviderScope {account_id:"a".into(),origin:"https://example.test".into(),service:"calendar".into(),collection:"/cal/".into()};
        let item = ProviderObject {remote_id:"/cal/task.ics".into(),etag:Some("v1".into()),raw:b"BEGIN:VCALENDAR".to_vec()};
        let index = ProviderDavIndex::Valid {uid:"task1".into(),kind:"task".into(),parent_uid:Some("parent".into())};
        store.apply_provider_dav_page(&scope,&[(ProviderChange::Upsert(item.clone()),Some(index.clone()))],"one").unwrap();
        assert_eq!(store.provider_dav_uid_lookup(&scope,"task1").unwrap().len(),1);
        assert_eq!(store.provider_dav_index_incomplete_count(&scope).unwrap(),0);
        let invalid = ProviderDavIndex::Valid {uid:"".into(),kind:"task".into(),parent_uid:None};
        assert!(store.apply_provider_dav_page(&scope,&[(ProviderChange::Upsert(item.clone()),Some(invalid))],"bad").is_err());
        assert_eq!(store.provider_cursor(&scope).unwrap().as_deref(),Some("one"));
        assert_eq!(store.provider_dav_uid_lookup(&scope,"task1").unwrap().len(),1);
        store.begin_provider_staging(&scope,"stage").unwrap();
        store.stage_provider_objects(&scope,"stage",std::slice::from_ref(&item)).unwrap();
        assert!(store.finish_provider_staging(&scope,"stage","two").is_err());
        store.stage_provider_dav_index(&scope,"stage",&item.remote_id,&index).unwrap();
        store.finish_provider_staging(&scope,"stage","two").unwrap();
        assert_eq!(store.provider_dav_uid_lookup(&scope,"task1").unwrap().len(),1);
        store.apply_provider_page(&scope,&[ProviderChange::Upsert(item)],"three").unwrap();
        assert_eq!(store.provider_dav_index_incomplete_count(&scope).unwrap(),1);
    }

    #[test]
    fn local_sessions_are_scoped_immutable_and_cas_prevents_aba() {
        let store=Store::open_in_memory().unwrap();
        let scope=ProviderScope {account_id:"a".into(),origin:"https://one.example".into(),service:"agent".into(),collection:"".into()};
        let other=ProviderScope {origin:"https://other.example".into(),..scope.clone()};
        assert!(store.local_session_create(&scope,"session",b"sealed1").unwrap());
        assert!(!store.local_session_create(&scope,"session",b"sealed1").unwrap());
        assert!(store.local_session_create(&scope,"session",b"different").is_err());
        assert!(store.local_session_manifest(&other,"session").unwrap().is_none());
        store.local_session_stage(&scope,"session","record","id",b"ciphertext").unwrap();
        store.local_session_stage(&scope,"session","record","id",b"ciphertext").unwrap();
        assert!(store.local_session_stage(&scope,"session","record","id",b"other").is_err());
        assert_eq!(store.local_session_cas(&scope,"session","1",b"sealed2").unwrap().as_deref(),Some("2"));
        assert!(store.local_session_cas(&scope,"session","1",b"sealed3").unwrap().is_none());
        assert_eq!(store.local_session_cas(&scope,"session","2",b"sealed1").unwrap().as_deref(),Some("3"));
        assert!(store.local_session_cas(&scope,"session","1",b"sealed4").unwrap().is_none());
        store.conn.execute("UPDATE provider_sessions SET object_bytes=134217728 WHERE account_id=?1 AND origin=?2 AND session_id=?3",params![scope.account_id,scope.origin,"session"]).unwrap();
        assert!(matches!(store.local_session_stage(&scope,"session","record","overflow",b"x"),Err(StoreError::ProviderObjectTooLarge)));
        assert!(store.local_session_load(&scope,"session","record","overflow").unwrap().is_none());
    }

    #[test]
    fn alarm_claims_distinguish_occurrence_and_survive_pending_state() {
        let store=Store::open_in_memory().unwrap();
        let scope=ProviderScope {account_id:"a".into(),origin:"https://example.test".into(),service:"calendar".into(),collection:"c".into()};
        let alarm=ProviderAlarmIdentity {uid:"task".into(),recurrence_id:"occurrence1".into(),alarm_id:"alarm".into(),trigger_at:"2026-10-28T10:00:00Z".into()};
        assert!(store.claim_provider_alarm(&scope,&alarm).unwrap());
        assert!(!store.claim_provider_alarm(&scope,&alarm).unwrap());
        let other=ProviderAlarmIdentity {recurrence_id:"occurrence2".into(),..alarm.clone()};
        assert!(store.claim_provider_alarm(&scope,&other).unwrap());
        assert_eq!(store.pending_provider_alarms(&scope,10).unwrap().len(),2);
        assert!(store.finish_provider_alarm(&scope,&alarm).unwrap());
        assert!(!store.finish_provider_alarm(&scope,&alarm).unwrap());
        assert_eq!(store.pending_provider_alarms(&scope,10).unwrap(),vec![other]);
    }
}
