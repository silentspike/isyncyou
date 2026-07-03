//! TOML configuration — the single source of truth shared by the daemon and GUI.
//! Loaded once, validated, and written back atomically.

use crate::onedrive_mode::OneDriveModes;
use crate::recovery::atomic_write;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
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
    /// Privileged mount-wide change source, fanotify-backed (server mode). Needs
    /// `CAP_SYS_ADMIN`; falls back to inotify when unprivileged or unsupported.
    /// Accepts `ebpf` or `fanotify` in TOML (both map here; serializes as `ebpf`).
    #[serde(alias = "fanotify")]
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

/// True for an unset (empty) path — keeps a defaulted `cache_root` out of serialized TOML.
fn path_is_empty(p: &Path) -> bool {
    p.as_os_str().is_empty()
}

/// One configured Microsoft account.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccountConfig {
    pub id: String,
    pub username: String,
    /// Bidirectionally-synced OneDrive folder — the **offline** working copy
    /// (Mode 3). Only this root is scanned for local→cloud writeback.
    pub sync_root: PathBuf,
    /// Archive/backup directory for the other services.
    pub archive_root: PathBuf,
    /// Lazy-preview cache root for OneDrive **online/sync** modes (1/2): on-demand
    /// downloads land here, kept apart from the editable offline copy in `sync_root`
    /// so preview cache and working copy never mix (the writeback scanner ignores it).
    /// Empty in older configs → [`AccountConfig::effective_cache_root`] derives a
    /// sibling `cache` dir. (#onedrive-mobile 0C.)
    #[serde(default, skip_serializing_if = "path_is_empty")]
    pub cache_root: PathBuf,
    /// Optional FUSE placeholder mount point (Files-on-Demand). When set, the
    /// daemon mounts a read-only view of the whole OneDrive tree here; files
    /// materialize on first read. Independent of `sync_root` (which stays the
    /// bidirectional full sync). Unset = no placeholder mount.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mount_point: Option<PathBuf>,
}

impl AccountConfig {
    /// The effective OneDrive lazy-preview cache root: the configured `cache_root`,
    /// or — for older configs that predate it — a `cache` sibling of `archive_root`.
    /// Always distinct from `sync_root`/`archive_root` (asserted by config validation).
    pub fn effective_cache_root(&self) -> PathBuf {
        if self.cache_root.as_os_str().is_empty() {
            self.archive_root
                .parent()
                .map(|p| p.join("cache"))
                .unwrap_or_else(|| self.archive_root.join("_cache"))
        } else {
            self.cache_root.clone()
        }
    }
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
    /// Cloud-poll cadence (seconds) when the UI is active — the user-tunable
    /// interval slider (1 s … 3600 s). `429`/`Retry-After` backoff always overrides.
    pub poll_interval_secs: u64,
    /// When the UI is idle, the active interval is stretched by this factor to
    /// save battery/network (e.g. 5 s active → 30 s idle at factor 6).
    pub poll_idle_factor: u32,
    /// Calendar sync model (#565): `"events"` (default) pages `/me/events` —
    /// recurring series stored as one master + its rule, no date window, no
    /// occurrence explosion. `"calendar_view"` keeps the legacy windowed
    /// `calendarView/delta` (incremental but window-bound) as a rollback.
    pub calendar_sync_mode: String,
    /// Mobile transfer policy (#onedrive-mobile 0.8): only run downloads/materialization
    /// on an unmetered network (Wi-Fi). Off = any network. Enforced by [`crate::policy`].
    pub wifi_only: bool,
    /// Mobile transfer policy (#onedrive-mobile 0.8): only run downloads/materialization
    /// while the device is charging. Off = any power state. Enforced by [`crate::policy`].
    pub charging_only: bool,
    /// Device-protection storage floor in bytes (#onedrive-mobile 0.8): below this much
    /// free space, NEW downloads/materialization stop — existing files are kept. This is
    /// an OS safety floor to protect the device, NOT a user storage quota. Default 256 MiB.
    pub min_free_bytes: u64,
}

impl Default for SyncConfig {
    fn default() -> Self {
        SyncConfig {
            trash_retention_days: 30,
            delete_guard: DeleteGuardConfig::default(),
            change_source: ChangeSource::default(),
            body_index: false,
            poll_interval_secs: 5,
            poll_idle_factor: 6,
            calendar_sync_mode: "events".into(),
            wifi_only: false,
            charging_only: false,
            min_free_bytes: 268_435_456, // 256 MiB device-protection floor
        }
    }
}

/// Optional Proxmox Backup Server target. No secret lives here —
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

/// Cloud-restore safety gate.
///
/// Re-creating an item in the cloud is a Graph write followed by a local record,
/// and the two are not atomic — a crash in between can make a naive retry create a
/// duplicate in the user's real mailbox. The crash-safe operation-ledger path that
/// makes those retries idempotent is complete and live-confirmed (see
/// `docs/adr/001-restore-semantics.md`); cloud-mutating restore is still **off by
/// default** as a deliberate opt-in — it writes to a real mailbox — and must be
/// explicitly enabled. Even when enabled, only **mail** is ledger-backed today;
/// other services' cloud restore is refused until they are migrated.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct RestoreConfig {
    /// Allow restore operations that create items in the cloud via Graph. Defaults
    /// to `false` (the `bool` default) until the operation ledger and its crash
    /// matrix are complete. Restoring an archived body *to a local file* is always
    /// allowed and is not gated by this flag.
    pub cloud_restore_enabled: bool,
}

/// The full configuration document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    pub accounts: Vec<AccountConfig>,
    pub sync: SyncConfig,
    /// Cloud-restore safety gate (off by default).
    pub restore: RestoreConfig,
    /// Optional PBS backup target (snapshot/restore of the store).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pbs: Option<PbsConfig>,
    /// Per-account OneDrive folder-mode policy (#650), keyed by account id. Kept here (not
    /// on the literal-constructed `AccountConfig`) so it is purely additive: every
    /// `Config { .. }` uses `..Default::default()`, so no other construction site changes.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub onedrive_modes: BTreeMap<String, OneDriveModes>,
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
            // The three OneDrive roots must be distinct (#onedrive-mobile 0C): the offline
            // working copy (sync_root), the other-services archive (archive_root), and the
            // lazy-preview cache (cache_root) must never overlap, or writeback/cleanup and
            // conflict handling become ambiguous. Checked against the *effective* cache_root.
            let cache = a.effective_cache_root();
            if !a.sync_root.as_os_str().is_empty() && cache == a.sync_root {
                errs.push(format!("{who}: cache_root and sync_root must differ"));
            }
            if !a.archive_root.as_os_str().is_empty() && cache == a.archive_root {
                errs.push(format!("{who}: cache_root and archive_root must differ"));
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

        // Per-account OneDrive mode policy (#650): each `onedrive_modes` entry must name a
        // configured account, and its folder-mode keys must be non-empty.
        let known: std::collections::HashSet<&str> =
            self.accounts.iter().map(|a| a.id.as_str()).collect();
        for (acct, modes) in &self.onedrive_modes {
            if !known.contains(acct.as_str()) {
                errs.push(format!("onedrive_modes: unknown account id '{acct}'"));
            }
            errs.extend(modes.validation_errors(&format!("onedrive_modes[{acct}]")));
        }

        if errs.is_empty() {
            Ok(())
        } else {
            Err(errs)
        }
    }

    /// Effective OneDrive mode for a folder within an account. Falls back to `Online` when the
    /// account has no mode policy. `ancestry` is deepest-first — see
    /// [`crate::onedrive_mode::OneDriveModes::effective_mode`].
    pub fn effective_mode(
        &self,
        account_id: &str,
        folder_id: &str,
        ancestry: &[&str],
    ) -> crate::onedrive_mode::OneDriveMode {
        self.onedrive_modes
            .get(account_id)
            .map(|m| m.effective_mode(folder_id, ancestry))
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::onedrive_mode::OneDriveMode;

    fn account(id: &str) -> AccountConfig {
        AccountConfig {
            id: id.into(),
            username: format!("{id}@outlook.com"),
            sync_root: PathBuf::from(format!("/home/u/{id}/OneDrive")),
            archive_root: PathBuf::from(format!("/home/u/{id}/Archive")),
            cache_root: PathBuf::from(format!("/home/u/{id}/Cache")),
            mount_point: None,
        }
    }

    #[test]
    fn cache_root_defaults_distinct_and_validation_rejects_overlap() {
        // Absent in older TOML -> empty field, effective_cache_root derives a distinct
        // `cache` sibling of archive_root; validation passes.
        let toml = "[[accounts]]\nid=\"a\"\nusername=\"a@x.com\"\n\
                    sync_root=\"/home/u/a/OneDrive\"\narchive_root=\"/home/u/a/Archive\"\n";
        let c: Config = toml::from_str(toml).unwrap();
        assert!(c.accounts[0].cache_root.as_os_str().is_empty());
        assert_eq!(
            c.accounts[0].effective_cache_root(),
            PathBuf::from("/home/u/a/cache")
        );
        c.validate().unwrap();
        // An explicit cache_root that collides with sync_root must be rejected.
        let mut bad = account("b");
        bad.cache_root = bad.sync_root.clone();
        let cfg = Config {
            accounts: vec![bad],
            ..Default::default()
        };
        let errs = cfg.validate().unwrap_err();
        assert!(
            errs.iter()
                .any(|e| e.contains("cache_root and sync_root must differ")),
            "expected cache/sync overlap error, got {errs:?}"
        );
    }

    #[test]
    fn mount_point_is_optional_and_round_trips() {
        // absent in TOML -> None, and a config without it still parses
        let toml = "[[accounts]]\nid=\"a\"\nusername=\"a@x.com\"\n\
                    sync_root=\"/s\"\narchive_root=\"/ar\"\n";
        let c: Config = toml::from_str(toml).unwrap();
        assert_eq!(c.accounts[0].mount_point, None);
        // None is skipped on serialize (no noise in written configs)
        assert!(!toml::to_string(&c).unwrap().contains("mount_point"));
        // a set value round-trips
        let mut c2 = c.clone();
        c2.accounts[0].mount_point = Some(PathBuf::from("/home/u/OneDrive-cloud"));
        let s = toml::to_string(&c2).unwrap();
        assert!(s.contains("mount_point"));
        let back: Config = toml::from_str(&s).unwrap();
        assert_eq!(
            back.accounts[0].mount_point,
            Some(PathBuf::from("/home/u/OneDrive-cloud"))
        );
    }

    #[test]
    fn defaults_are_sane() {
        let c = Config::default();
        assert_eq!(c.sync.trash_retention_days, 30);
        assert_eq!(c.sync.change_source, ChangeSource::Inotify);
        assert!(!c.sync.body_index);
        assert_eq!(c.sync.poll_interval_secs, 5);
        assert_eq!(c.sync.poll_idle_factor, 6);
        // Cloud-mutating restore is OFF until the operation ledger is complete.
        assert!(!c.restore.cloud_restore_enabled);
        assert!(c.validate().is_ok());
    }

    #[test]
    fn poll_interval_round_trips_and_defaults_when_omitted() {
        // AC1: an explicit interval round-trips through toml
        let mut c = Config::default();
        c.sync.poll_interval_secs = 1;
        c.sync.poll_idle_factor = 10;
        let s = toml::to_string(&c).unwrap();
        let back: Config = toml::from_str(&s).unwrap();
        assert_eq!(back.sync.poll_interval_secs, 1);
        assert_eq!(back.sync.poll_idle_factor, 10);
        // AC2: a [sync] table that omits the field falls back to the default (5/6)
        let toml = r#"
            [[accounts]]
            id = "p"
            username = "p@outlook.com"
            sync_root = "/d/od"
            archive_root = "/d/a"
            [sync]
            trash_retention_days = 14
        "#;
        let c2 = Config::from_toml(toml).unwrap();
        assert_eq!(c2.sync.poll_interval_secs, 5);
        assert_eq!(c2.sync.poll_idle_factor, 6);
        assert_eq!(c2.sync.trash_retention_days, 14);
        // the mobile transfer-policy fields default when omitted
        assert!(!c2.sync.wifi_only);
        assert!(!c2.sync.charging_only);
        assert_eq!(c2.sync.min_free_bytes, 268_435_456);
    }

    #[test]
    fn transfer_policy_fields_round_trip() {
        // #onedrive-mobile 0.8: wifi_only/charging_only/min_free_bytes survive toml
        let mut c = Config::default();
        c.sync.wifi_only = true;
        c.sync.charging_only = true;
        c.sync.min_free_bytes = 512 * 1024 * 1024;
        let back: Config = toml::from_str(&toml::to_string(&c).unwrap()).unwrap();
        assert!(back.sync.wifi_only);
        assert!(back.sync.charging_only);
        assert_eq!(back.sync.min_free_bytes, 512 * 1024 * 1024);
    }

    #[test]
    fn cloud_restore_stays_off_when_omitted_from_toml() {
        // A config that never mentions [restore] must not silently enable it.
        let toml = r#"
            [[accounts]]
            id = "primary"
            username = "primary@example.com"
            sync_root = "/data/od"
            archive_root = "/data/archive"
        "#;
        let c = Config::from_toml(toml).unwrap();
        assert!(!c.restore.cloud_restore_enabled);
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
    fn change_source_accepts_fanotify_alias() {
        // The truthful name `fanotify` deserializes to the `Ebpf` variant (#331),
        // and so does the canonical `ebpf`.
        let from_alias: SyncConfig = toml::from_str("change_source = \"fanotify\"").unwrap();
        assert_eq!(from_alias.change_source, ChangeSource::Ebpf);
        let from_canonical: SyncConfig = toml::from_str("change_source = \"ebpf\"").unwrap();
        assert_eq!(from_canonical.change_source, ChangeSource::Ebpf);
        // Serialization stays `ebpf` (alias affects deserialization only), so
        // existing configs/tests are unaffected.
        let dumped = toml::to_string(&from_alias).unwrap();
        assert!(
            dumped.contains("change_source = \"ebpf\""),
            "expected canonical ebpf in output, got: {dumped}"
        );
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

    // ---- #650 AC1: onedrive_modes round-trips through TOML + defaults when omitted ----
    #[test]
    fn onedrive_modes_round_trip_and_default_when_omitted() {
        // An explicit per-account policy round-trips, including a realistic OneDrive folder
        // id that contains '!' (TOML must quote it as a key and reparse it identically).
        let mut c = Config::default();
        c.accounts.push(account("me"));
        let mut modes = OneDriveModes {
            default_mode: OneDriveMode::Sync,
            ..Default::default()
        };
        modes
            .folder_modes
            .insert("01ABCDEF!123".to_string(), OneDriveMode::Offline);
        modes
            .folder_modes
            .insert("01ABCDEF!456".to_string(), OneDriveMode::Sync);
        c.onedrive_modes.insert("me".to_string(), modes);
        c.validate().unwrap();

        let s = c.to_toml().unwrap();
        let back = Config::from_toml(&s).unwrap();
        assert_eq!(back, c, "onedrive_modes round-trips through TOML");
        assert_eq!(
            back.onedrive_modes["me"].folder_modes["01ABCDEF!123"],
            OneDriveMode::Offline,
            "the '!'-bearing folder id survived TOML key quoting + reparse"
        );

        // Default-when-omitted: a config that never mentions [onedrive_modes] parses to an
        // empty map (Config's derived Default carries the new field), and every folder then
        // resolves to the Online default.
        let toml = r#"
            [[accounts]]
            id = "me"
            username = "me@outlook.com"
            sync_root = "/d/od"
            archive_root = "/d/a"
        "#;
        let c2 = Config::from_toml(toml).unwrap();
        assert!(c2.onedrive_modes.is_empty());
        assert_eq!(
            c2.effective_mode("me", "any-folder", &[]),
            OneDriveMode::Online
        );
        // An empty map is skipped on serialize (no noise in written configs).
        assert!(!Config::default()
            .to_toml()
            .unwrap()
            .contains("onedrive_modes"));

        // A default (Online) default_mode is skipped on serialize but still round-trips.
        let mut c3 = Config::default();
        c3.accounts.push(account("me"));
        let mut m3 = OneDriveModes::default(); // default_mode = Online
        m3.folder_modes
            .insert("f1".to_string(), OneDriveMode::Offline);
        c3.onedrive_modes.insert("me".to_string(), m3);
        let s3 = c3.to_toml().unwrap();
        assert!(
            !s3.contains("default_mode"),
            "default Online is skipped: {s3}"
        );
        let back3 = Config::from_toml(&s3).unwrap();
        assert_eq!(
            back3.onedrive_modes["me"].default_mode,
            OneDriveMode::Online
        );
        assert_eq!(back3, c3);
    }

    // ---- #650 AC3: validation rejects invalid entries ----
    #[test]
    fn onedrive_modes_validation_rejects_invalid_entries() {
        // (1) an empty / whitespace-only folder-id key is rejected
        let mut c = Config::default();
        c.accounts.push(account("me"));
        let mut modes = OneDriveModes::default();
        modes
            .folder_modes
            .insert("  ".to_string(), OneDriveMode::Sync);
        c.onedrive_modes.insert("me".to_string(), modes);
        let errs = c.validate().unwrap_err();
        assert!(
            errs.iter().any(|e| e.contains("empty folder_modes key")),
            "expected empty-key error, got {errs:?}"
        );

        // (2) a modes entry for an account that isn't configured is rejected
        let mut c2 = Config::default();
        c2.accounts.push(account("me"));
        c2.onedrive_modes
            .insert("ghost".to_string(), OneDriveModes::default());
        let errs2 = c2.validate().unwrap_err();
        assert!(
            errs2
                .iter()
                .any(|e| e.contains("unknown account id 'ghost'")),
            "expected unknown-account error, got {errs2:?}"
        );
    }

    // ---- #650 AC3: validation rejects duplicate entries (TOML parser) ----
    #[test]
    fn onedrive_modes_duplicate_key_rejected_by_parser() {
        // Two identical folder-id keys under the same account is a TOML duplicate-key error.
        let toml = r#"
            [[accounts]]
            id = "me"
            username = "me@outlook.com"
            sync_root = "/d/od"
            archive_root = "/d/a"

            [onedrive_modes.me.folder_modes]
            "01ABC!1" = "sync"
            "01ABC!1" = "offline"
        "#;
        assert!(
            Config::from_toml(toml).is_err(),
            "duplicate folder_modes key must be rejected by the TOML parser"
        );
    }
}
