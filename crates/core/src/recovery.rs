//! Crash-recovery primitives (plan §5): atomic file writes, an operation journal,
//! and a periodic self-check.
//!
//! - [`atomic_write`] — write-to-temp + fsync + rename, so a crash never leaves a
//!   half-written file at the target path.
//! - [`Journal`] — an append/remove record of in-flight operations, persisted
//!   atomically; after a crash, [`Journal::pending`] lists the operations that
//!   were started but never completed, so they can be re-driven.
//! - [`SelfCheck`] — folds health signals (token / DB / disk / freshness) into a
//!   green/yellow/red [`HealthStatus`].

use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static TMP_CTR: AtomicU64 = AtomicU64::new(0);

/// Atomically replace `path`'s contents with `data`: write a sibling temp file,
/// `fsync` it, then `rename` over the target (atomic on POSIX), and best-effort
/// `fsync` the directory so the rename is durable.
pub fn atomic_write(path: &Path, data: &[u8]) -> io::Result<()> {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let fname = path.file_name().and_then(|s| s.to_str()).unwrap_or("file");
    let ctr = TMP_CTR.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!(
        ".{fname}.isyncyou-tmp.{}.{ctr}",
        std::process::id()
    ));

    let res = (|| {
        let mut f = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if res.is_err() {
        let _ = fs::remove_file(&tmp);
        return res;
    }
    if let Ok(d) = File::open(dir) {
        let _ = d.sync_all(); // best-effort durability of the rename
    }
    Ok(())
}

/// One in-flight operation recorded in the [`Journal`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalEntry {
    pub seq: u64,
    pub op: String,
}

/// An atomically-persisted journal of in-flight operations.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Journal {
    next_seq: u64,
    entries: Vec<JournalEntry>,
    #[serde(skip)]
    path: PathBuf,
}

impl Journal {
    /// Open (or create) a journal at `path`, loading any operations that were
    /// pending from a previous (possibly crashed) run.
    pub fn open(path: impl Into<PathBuf>) -> io::Result<Self> {
        let path = path.into();
        let mut j: Journal = if path.exists() {
            serde_json::from_slice(&fs::read(&path)?).map_err(io::Error::other)?
        } else {
            Journal::default()
        };
        j.path = path;
        Ok(j)
    }

    /// Record the start of an operation; returns its sequence number.
    pub fn begin(&mut self, op: impl Into<String>) -> io::Result<u64> {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.entries.push(JournalEntry { seq, op: op.into() });
        self.persist()?;
        Ok(seq)
    }

    /// Mark an operation complete (removes it from the journal).
    pub fn commit(&mut self, seq: u64) -> io::Result<()> {
        self.entries.retain(|e| e.seq != seq);
        self.persist()
    }

    /// Operations that were started but not yet committed.
    pub fn pending(&self) -> &[JournalEntry] {
        &self.entries
    }

    fn persist(&self) -> io::Result<()> {
        let bytes = serde_json::to_vec(self).map_err(io::Error::other)?;
        atomic_write(&self.path, &bytes)
    }
}

/// Aggregated health signals for the periodic self-check.
#[derive(Debug, Clone)]
pub struct SelfCheck {
    pub token_valid: bool,
    pub db_ok: bool,
    pub free_bytes: u64,
    pub min_free_bytes: u64,
    /// Seconds since the last successful sync (`None` = never).
    pub last_sync_age_secs: Option<i64>,
    pub max_sync_age_secs: i64,
}

/// Result of [`SelfCheck::evaluate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HealthStatus {
    Green,
    Yellow(Vec<String>),
    Red(Vec<String>),
}

impl SelfCheck {
    /// Fold the signals into a status. Any hard problem (auth/DB/disk) is red;
    /// staleness is yellow; otherwise green.
    pub fn evaluate(&self) -> HealthStatus {
        let mut red = Vec::new();
        let mut yellow = Vec::new();

        if !self.token_valid {
            red.push("auth token invalid or expired".to_string());
        }
        if !self.db_ok {
            red.push("store database not healthy".to_string());
        }
        if self.free_bytes < self.min_free_bytes {
            red.push(format!(
                "low disk space: {} bytes free, need {}",
                self.free_bytes, self.min_free_bytes
            ));
        }
        match self.last_sync_age_secs {
            Some(age) if age > self.max_sync_age_secs => {
                yellow.push(format!(
                    "last sync {age}s ago exceeds {}s",
                    self.max_sync_age_secs
                ));
            }
            None => yellow.push("no successful sync yet".to_string()),
            _ => {}
        }

        if !red.is_empty() {
            HealthStatus::Red(red)
        } else if !yellow.is_empty() {
            HealthStatus::Yellow(yellow)
        } else {
            HealthStatus::Green
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_creates_and_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("data.bin");
        atomic_write(&p, b"hello").unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"hello");
        atomic_write(&p, b"world!!").unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"world!!");
        // no leftover temp files in the directory
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains("isyncyou-tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );
    }

    #[test]
    fn journal_records_and_recovers_pending() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.json");
        let (s1, s2);
        {
            let mut j = Journal::open(&path).unwrap();
            s1 = j.begin("upload r1").unwrap();
            s2 = j.begin("delete r2").unwrap();
            j.commit(s1).unwrap();
        } // simulate crash: drop without committing s2
        let j2 = Journal::open(&path).unwrap();
        let pending = j2.pending();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].seq, s2);
        assert_eq!(pending[0].op, "delete r2");
    }

    #[test]
    fn journal_seq_is_monotonic_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("j.json");
        let first;
        {
            let mut j = Journal::open(&path).unwrap();
            first = j.begin("a").unwrap();
        }
        let mut j2 = Journal::open(&path).unwrap();
        let second = j2.begin("b").unwrap();
        assert!(
            second > first,
            "seq should keep increasing: {first} -> {second}"
        );
    }

    fn healthy() -> SelfCheck {
        SelfCheck {
            token_valid: true,
            db_ok: true,
            free_bytes: 100_000,
            min_free_bytes: 10_000,
            last_sync_age_secs: Some(30),
            max_sync_age_secs: 3600,
        }
    }

    #[test]
    fn self_check_green_when_all_ok() {
        assert_eq!(healthy().evaluate(), HealthStatus::Green);
    }

    #[test]
    fn self_check_red_on_hard_problems() {
        let mut c = healthy();
        c.token_valid = false;
        c.free_bytes = 0;
        match c.evaluate() {
            HealthStatus::Red(rs) => {
                assert!(rs.iter().any(|r| r.contains("token")));
                assert!(rs.iter().any(|r| r.contains("disk space")));
            }
            other => panic!("expected Red, got {other:?}"),
        }
    }

    #[test]
    fn self_check_yellow_on_staleness() {
        let mut c = healthy();
        c.last_sync_age_secs = Some(7200);
        assert!(matches!(c.evaluate(), HealthStatus::Yellow(_)));
        c.last_sync_age_secs = None;
        assert!(matches!(c.evaluate(), HealthStatus::Yellow(_)));
    }

    #[test]
    fn red_takes_precedence_over_yellow() {
        let mut c = healthy();
        c.db_ok = false; // red
        c.last_sync_age_secs = None; // would be yellow
        assert!(matches!(c.evaluate(), HealthStatus::Red(_)));
    }
}
