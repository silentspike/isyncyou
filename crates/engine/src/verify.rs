//! Archive **integrity verification** — re-read every archived body, SHA-256 it,
//! and compare to the recorded baseline. The web UI's "Integrity verified" /
//! "Verified" signals are backed by this and nothing else: a record counts as
//! verified only after its on-disk body has actually been read and hashed here.
//!
//! Semantics (honest, point-in-time):
//! - first-seen body → record its hash as the baseline, count `verified`
//! - body present, hash == baseline → `verified`
//! - body present, hash != baseline → `changed` (drift since the last check); the
//!   baseline is advanced to the current hash so the next pass compares fresh
//! - body missing / unreadable → `failed`

use isyncyou_core::Config;
use isyncyou_store::Store;
use sha2::{Digest, Sha256};
use std::time::{SystemTime, UNIX_EPOCH};

/// Outcome of a verify pass over one account's archived bodies.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct VerifyReport {
    /// Items that have an archived body (the work-list size).
    pub total: usize,
    /// Bodies read + hashed OK (baseline or unchanged).
    pub verified: usize,
    /// Bodies whose hash differs from the recorded baseline.
    pub changed: usize,
    /// Bodies missing or unreadable on disk.
    pub failed: usize,
}

impl VerifyReport {
    pub fn summary(&self) -> String {
        format!(
            "{} verified, {} changed, {} failed of {}",
            self.verified, self.changed, self.failed, self.total
        )
    }
    /// Activity-log status: clean only when nothing drifted or failed.
    pub fn status(&self) -> &'static str {
        if self.failed > 0 {
            "error"
        } else if self.changed > 0 {
            "warn"
        } else {
            "ok"
        }
    }
}

fn now_unix() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
        .to_string()
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Verify every archived body for `account` and persist per-item status via
/// [`Store::set_verify`], then record a `verify` run. Opens the account's own
/// store under `archive_root`.
pub fn verify_account(cfg: &Config, account: &str) -> Result<VerifyReport, String> {
    let acc = cfg
        .accounts
        .iter()
        .find(|a| a.id == account)
        .ok_or_else(|| format!("no account '{account}' in config"))?;
    let store =
        Store::open(acc.archive_root.join(".isyncyou-store.db")).map_err(|e| e.to_string())?;
    let started = now_unix();
    let items = store.items_with_body(account).map_err(|e| e.to_string())?;
    let mut rep = VerifyReport::default();
    for it in &items {
        let rel = match &it.local_path {
            Some(p) => p,
            None => continue, // items_with_body guarantees Some; stay defensive
        };
        // An archived service (mail/calendar/…) stores its body under
        // archive_root; a OneDrive file is the materialized file under sync_root
        // (same split as the web UI's read_archived).
        let root = if it.service == "onedrive" {
            &acc.sync_root
        } else {
            &acc.archive_root
        };
        let path = root.join(rel);
        // A OneDrive item can be a cloud-only placeholder (tracked but not yet
        // downloaded) — that's not an archive-integrity item, so skip it rather
        // than count it as a failure.
        if it.service == "onedrive" && !path.exists() {
            continue;
        }
        rep.total += 1;
        let at = now_unix();
        match std::fs::read(&path) {
            Ok(bytes) => {
                let sha = sha256_hex(&bytes);
                let drifted = matches!(&it.body_sha256, Some(prev) if *prev != sha);
                let status = if drifted { "changed" } else { "verified" };
                if drifted {
                    rep.changed += 1;
                } else {
                    rep.verified += 1;
                }
                // advance the baseline to the current hash either way
                store
                    .set_verify(account, &it.service, &it.remote_id, Some(&sha), status, &at)
                    .map_err(|e| e.to_string())?;
            }
            Err(_) => {
                rep.failed += 1;
                // keep whatever baseline we had; just flag the failure
                store
                    .set_verify(
                        account,
                        &it.service,
                        &it.remote_id,
                        it.body_sha256.as_deref(),
                        "failed",
                        &at,
                    )
                    .map_err(|e| e.to_string())?;
            }
        }
    }
    let finished = now_unix();
    store
        .add_run(
            account,
            "verify",
            &started,
            &finished,
            rep.status(),
            &rep.summary(),
        )
        .map_err(|e| e.to_string())?;
    Ok(rep)
}

#[cfg(test)]
mod tests {
    use super::*;
    use isyncyou_store::{Item, Store};

    fn cfg_with(arch: &std::path::Path) -> Config {
        Config {
            accounts: vec![isyncyou_core::AccountConfig {
                id: "t".into(),
                username: "u@example.com".into(),
                sync_root: arch.join("sync"),
                archive_root: arch.to_path_buf(),
                mount_point: None,
            }],
            ..Default::default()
        }
    }

    #[test]
    fn verify_baselines_then_detects_drift_and_missing() {
        let dir = std::env::temp_dir().join(format!("isyncyou-verify-{}", std::process::id()));
        let arch = dir.join("arch");
        std::fs::create_dir_all(arch.join("contacts/aa")).unwrap();
        std::fs::write(arch.join("contacts/aa/a.json"), b"{\"name\":\"A\"}").unwrap();
        std::fs::write(arch.join("contacts/aa/b.json"), b"{\"name\":\"B\"}").unwrap();
        {
            let store = Store::open(arch.join(".isyncyou-store.db")).unwrap();
            for (id, p) in [("a", "contacts/aa/a.json"), ("b", "contacts/aa/b.json")] {
                let mut it = Item::new("t", "contacts", id, format!("{id}.json"), "contact");
                it.local_path = Some(p.into());
                store.upsert_item(&it).unwrap();
            }
        }
        let cfg = cfg_with(&arch);

        // first pass: both baselined → verified
        let r1 = verify_account(&cfg, "t").unwrap();
        assert_eq!((r1.total, r1.verified, r1.changed, r1.failed), (2, 2, 0, 0));
        {
            let store = Store::open(arch.join(".isyncyou-store.db")).unwrap();
            assert_eq!(store.verify_counts("t").unwrap(), (2, 2));
            let a = store.get_item("t", "contacts", "a").unwrap().unwrap();
            assert_eq!(a.verify_status.as_deref(), Some("verified"));
            assert!(a.body_sha256.is_some());
        }

        // tamper a, delete b → changed + failed
        std::fs::write(arch.join("contacts/aa/a.json"), b"{\"name\":\"A-EDITED\"}").unwrap();
        std::fs::remove_file(arch.join("contacts/aa/b.json")).unwrap();
        let r2 = verify_account(&cfg, "t").unwrap();
        assert_eq!((r2.total, r2.verified, r2.changed, r2.failed), (2, 0, 1, 1));
        {
            let store = Store::open(arch.join(".isyncyou-store.db")).unwrap();
            let a = store.get_item("t", "contacts", "a").unwrap().unwrap();
            assert_eq!(a.verify_status.as_deref(), Some("changed"));
            let b = store.get_item("t", "contacts", "b").unwrap().unwrap();
            assert_eq!(b.verify_status.as_deref(), Some("failed"));
            // a run was recorded with a non-clean status
            let runs = store.recent_runs("t", 1).unwrap();
            assert_eq!(runs[0].kind, "verify");
            assert_eq!(runs[0].status, "error"); // a failure present
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
