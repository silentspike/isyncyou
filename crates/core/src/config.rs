//! TOML configuration — the single source of truth shared by the daemon and GUI
//! (plan §13). Loaded once, validated, and written back atomically.

use crate::recovery::atomic_write;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Which change-detection backend to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ChangeSource {
    /// inotify accelerator + periodic reconciler (desktop default).
    #[default]
    Inotify,
    /// periodic reconciler only (no event watcher).
    ReconcileOnly,
    /// eBPF/fanotify (privileged server mode).
    Ebpf,
}

/// Mass-delete guard thresholds (mirrors [`crate::guard::DeleteGuard`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeleteGuardConfig {
    pub max_absolute: usize,
    pub max_fraction: f64,
    pub fraction_min_total: usize,
}

impl Default for DeleteGuardConfig {
    fn default() -> Self {
        DeleteGuardConfig {
            max_absolute: 1000,
            max_fraction: 0.5,
            fraction_min_total: 10,
        }
    }
}

/// One configured Microsoft account.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccountConfig {
    pub id: String,
    pub username: String,
    /// Bidirectionally-synced OneDrive folder.
    pub sync_root: PathBuf,
    /// Archive/backup directory for the other services.
    pub archive_root: PathBuf,
}

/// Engine-wide sync settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SyncConfig {
    pub trash_retention_days: u32,
    pub delete_guard: DeleteGuardConfig,
    pub change_source: ChangeSource,
    /// Index mail/file bodies in FTS (off = metadata only; privacy/space).
    pub body_index: bool,
}

impl Default for SyncConfig {
    fn default() -> Self {
        SyncConfig {
            trash_retention_days: 30,
            delete_guard: DeleteGuardConfig::default(),
            change_source: ChangeSource::default(),
            body_index: false,
        }
    }
}

/// Optional Proxmox Backup Server target (plan §9.2/§12). No secret lives here —
/// `password_file` points at a file holding the PBS password / API-token secret,
/// so the config can be shared/committed without leaking credentials.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PbsConfig {
    /// PBS repository, e.g. `user@realm@host:datastore` (or `…!token@…`).
    pub repository: String,
    /// Path to a file containing the PBS password / API-token secret.
    pub password_file: PathBuf,
    /// TLS fingerprint for a self-signed PBS certificate (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    /// PBS namespace isolating iSyncYou snapshots from other backups (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
}

/// The full configuration document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    pub accounts: Vec<AccountConfig>,
    pub sync: SyncConfig,
    /// Optional PBS backup target (snapshot/restore of the store).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pbs: Option<PbsConfig>,
}

impl Config {
    /// Parse a TOML string.
    pub fn from_toml(s: &str) -> Result<Config, String> {
        toml::from_str(s).map_err(|e| e.to_string())
    }

    /// Serialize to a TOML string.
    pub fn to_toml(&self) -> Result<String, String> {
        toml::to_string_pretty(self).map_err(|e| e.to_string())
    }

    /// Load from a file (does not validate; call [`Config::validate`] after).
    pub fn load(path: impl AsRef<Path>) -> Result<Config, String> {
        let s = std::fs::read_to_string(path.as_ref()).map_err(|e| e.to_string())?;
        Config::from_toml(&s)
    }

    /// Write to a file atomically (temp + rename).
    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), String> {
        let s = self.to_toml()?;
        atomic_write(path.as_ref(), s.as_bytes()).map_err(|e| e.to_string())
    }

    /// Validate the configuration, collecting every problem found.
    pub fn validate(&self) -> Result<(), Vec<String>> {
        let mut errs = Vec::new();
        let mut seen_ids = std::collections::HashSet::new();

        for (i, a) in self.accounts.iter().enumerate() {
            let who = if a.id.is_empty() {
                format!("account[{i}]")
            } else {
                a.id.clone()
            };
            if a.id.trim().is_empty() {
                errs.push(format!("{who}: empty account id"));
            } else if !seen_ids.insert(a.id.clone()) {
                errs.push(format!("duplicate account id: {}", a.id));
            }
            if a.sync_root.as_os_str().is_empty() {
                errs.push(format!("{who}: empty sync_root"));
            }
            if a.archive_root.as_os_str().is_empty() {
                errs.push(format!("{who}: empty archive_root"));
            }
            if !a.sync_root.as_os_str().is_empty() && a.sync_root == a.archive_root {
                errs.push(format!("{who}: sync_root and archive_root must differ"));
            }
        }

        let g = &self.sync.delete_guard;
        if !(g.max_fraction > 0.0 && g.max_fraction <= 1.0) {
            errs.push(format!(
                "sync.delete_guard.max_fraction must be in (0, 1], got {}",
                g.max_fraction
            ));
        }
        if g.max_absolute == 0 {
            errs.push("sync.delete_guard.max_absolute must be > 0".to_string());
        }

        if errs.is_empty() {
            Ok(())
        } else {
            Err(errs)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(id: &str) -> AccountConfig {
        AccountConfig {
            id: id.into(),
            username: format!("{id}@outlook.com"),
            sync_root: PathBuf::from(format!("/home/u/{id}/OneDrive")),
            archive_root: PathBuf::from(format!("/home/u/{id}/Archive")),
        }
    }

    #[test]
    fn defaults_are_sane() {
        let c = Config::default();
        assert_eq!(c.sync.trash_retention_days, 30);
        assert_eq!(c.sync.change_source, ChangeSource::Inotify);
        assert!(!c.sync.body_index);
        assert!(c.validate().is_ok());
    }

    #[test]
    fn partial_toml_fills_defaults() {
        // only one account, no [sync] table -> defaults apply
        let toml = r#"
            [[accounts]]
            id = "primary"
            username = "primary@outlook.com"
            sync_root = "/data/od"
            archive_root = "/data/archive"
        "#;
        let c = Config::from_toml(toml).unwrap();
        assert_eq!(c.accounts.len(), 1);
        assert_eq!(c.sync.trash_retention_days, 30); // default
        assert_eq!(c.sync.delete_guard.max_fraction, 0.5); // default
        c.validate().unwrap();
    }

    #[test]
    fn toml_roundtrips_via_save_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut c = Config::default();
        c.accounts.push(account("a"));
        c.sync.body_index = true;
        c.sync.change_source = ChangeSource::Ebpf;
        c.save(&path).unwrap();
        let back = Config::load(&path).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn detects_duplicate_account_ids() {
        let mut c = Config::default();
        c.accounts.push(account("dup"));
        c.accounts.push(account("dup"));
        let errs = c.validate().unwrap_err();
        assert!(errs.iter().any(|e| e.contains("duplicate account id")));
    }

    #[test]
    fn detects_same_sync_and_archive_root() {
        let mut a = account("x");
        a.archive_root = a.sync_root.clone();
        let c = Config {
            accounts: vec![a],
            ..Default::default()
        };
        let errs = c.validate().unwrap_err();
        assert!(errs.iter().any(|e| e.contains("must differ")));
    }

    #[test]
    fn detects_bad_guard_fraction() {
        let mut c = Config::default();
        c.sync.delete_guard.max_fraction = 1.5;
        let errs = c.validate().unwrap_err();
        assert!(errs.iter().any(|e| e.contains("max_fraction")));
    }

    #[test]
    fn collects_multiple_errors_at_once() {
        let mut c = Config::default();
        c.accounts.push(account("")); // empty id
        c.sync.delete_guard.max_absolute = 0; // bad
        let errs = c.validate().unwrap_err();
        assert!(errs.len() >= 2);
    }
}
