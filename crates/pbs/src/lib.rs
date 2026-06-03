//! `isyncyou-pbs` — trigger Proxmox Backup Server (PBS) snapshot + restore of the
//! store. The user never touches Proxmox directly: the tool shells out to
//! `proxmox-backup-client`. Restores always land in a **temporary** target (never
//! the live store) — the caller imports/previews from there.
//!
//! Secrets stay out of this crate: the caller reads the password/token from its
//! configured `password_file` and passes it in; it reaches the client only via the
//! `PBS_PASSWORD` env of the spawned process (never a command-line argument).

use std::path::Path;
use std::process::Command;

/// A PBS repository the tool can push to / restore from. One archive — `data.pxar`
/// — holds the snapshot directory (the store DB + a manifest).
pub struct Pbs {
    /// `user@realm@host:datastore` (or `…!token@…`).
    pub repository: String,
    /// TLS fingerprint for a self-signed PBS cert (optional).
    pub fingerprint: Option<String>,
    /// PBS namespace isolating these snapshots (optional).
    pub namespace: Option<String>,
    /// PBS password / API-token secret (read by the caller from its password file).
    pub password: String,
    /// `proxmox-backup-client` binary (override for tests / non-standard installs).
    pub client: String,
}

/// The single pxar archive name inside each snapshot.
const ARCHIVE: &str = "data.pxar";

impl Pbs {
    /// A `Pbs` with the default client binary on `$PATH`.
    pub fn new(repository: impl Into<String>, password: impl Into<String>) -> Self {
        Pbs {
            repository: repository.into(),
            fingerprint: None,
            namespace: None,
            password: password.into(),
            client: "proxmox-backup-client".to_string(),
        }
    }

    /// `proxmox-backup-client` with only the env set (no args). The subcommand +
    /// positionals come first; repository/namespace are appended via [`add_repo`].
    fn base(&self) -> Command {
        let mut c = Command::new(&self.client);
        // password via env, never argv (argv is world-readable in /proc)
        c.env("PBS_PASSWORD", &self.password);
        if let Some(fp) = &self.fingerprint {
            c.env("PBS_FINGERPRINT", fp);
        }
        c
    }

    /// Append `--repository`/`--ns` — these are options, so they must follow the
    /// subcommand + positional arguments, not precede them.
    fn add_repo(&self, c: &mut Command) {
        c.arg("--repository").arg(&self.repository);
        if let Some(ns) = &self.namespace {
            c.arg("--ns").arg(ns);
        }
    }

    /// Back up `dir` as a `data.pxar` snapshot under group `host/<backup_id>`.
    /// Returns the created snapshot path (`host/<backup_id>/<timestamp>`), parsed
    /// from the client's "Starting backup" line so the caller can restore it.
    pub fn backup(&self, backup_id: &str, dir: &Path) -> Result<String, String> {
        let mut c = self.base();
        c.arg("backup")
            .arg(format!("{ARCHIVE}:{}", dir.display()))
            .arg("--backup-type")
            .arg("host")
            .arg("--backup-id")
            .arg(backup_id);
        self.add_repo(&mut c);
        let out = run(c, "pbs backup")?;
        parse_snapshot(&out)
            .ok_or_else(|| format!("pbs backup: could not find the created snapshot in:\n{out}"))
    }

    /// Restore the `data.pxar` of `snapshot` into `target` (a temporary directory,
    /// never the live store). `target` must be an existing empty directory.
    pub fn restore(&self, snapshot: &str, target: &Path) -> Result<(), String> {
        let mut c = self.base();
        c.arg("restore").arg(snapshot).arg(ARCHIVE).arg(target);
        self.add_repo(&mut c);
        run(c, "pbs restore").map(|_| ())
    }

    /// Raw `snapshot list` output (text) for the repository — for the UI/CLI to show.
    pub fn list(&self) -> Result<String, String> {
        let mut c = self.base();
        c.arg("snapshot").arg("list");
        self.add_repo(&mut c);
        c.arg("--output-format").arg("text");
        run(c, "pbs snapshot list")
    }

    /// Forget (delete) a snapshot — used to clean up test/old snapshots.
    pub fn forget(&self, snapshot: &str) -> Result<(), String> {
        let mut c = self.base();
        c.arg("snapshot").arg("forget").arg(snapshot);
        self.add_repo(&mut c);
        run(c, "pbs snapshot forget").map(|_| ())
    }
}

/// Run the client, capturing combined output; map a non-zero exit to an error.
fn run(mut c: Command, what: &str) -> Result<String, String> {
    let out = c
        .output()
        .map_err(|e| format!("{what}: cannot run proxmox-backup-client: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    if out.status.success() {
        Ok(format!("{stdout}{stderr}"))
    } else {
        Err(format!("{what} failed: {}", stderr.trim()))
    }
}

/// Extract the `host/<id>/<timestamp>` snapshot path from a backup run's output
/// (the client logs `Starting backup: host/<id>/<ts>`).
fn parse_snapshot(output: &str) -> Option<String> {
    output
        .lines()
        .find_map(|l| {
            l.split_once("Starting backup:")
                .map(|(_, s)| s.trim().to_string())
        })
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_created_snapshot_from_backup_output() {
        let out = "Starting backup: host/isyncyou-acct/2026-06-02T16:30:00Z\n\
                   Client name: dev\nUploaded 1 chunks\nDuration: 0.42s";
        assert_eq!(
            parse_snapshot(out).as_deref(),
            Some("host/isyncyou-acct/2026-06-02T16:30:00Z")
        );
        assert_eq!(parse_snapshot("no snapshot line here"), None);
    }

    #[test]
    fn errors_never_leak_the_password_and_use_env_not_argv() {
        let mut pbs = Pbs::new("user@pbs@pbs.example:datastore", "secret");
        pbs.namespace = Some("isyncyou".into());
        pbs.client = "/nonexistent/proxmox-backup-client".into();
        let err = pbs.backup("acct", Path::new("/tmp")).unwrap_err();
        assert!(
            err.contains("cannot run proxmox-backup-client"),
            "got: {err}"
        );
        assert!(
            !err.contains("secret"),
            "password must never appear in errors"
        );
    }
}
