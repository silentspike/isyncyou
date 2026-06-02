//! `isyncyoud` — the iSyncYou engine daemon.
//!
//! The long-running service (systemd-user / system) that serves the local web
//! UI + JSON API (via [`isyncyou_webui`]) and logs a periodic liveness
//! heartbeat. Scheduled background backup/sync layers on top once the OAuth token
//! store is wired in (so the daemon can mint per-account tokens unattended, #40);
//! for now it is the persistent UI/API host.

use clap::Parser;
use isyncyou_core::Config;
use std::path::{Path, PathBuf};

#[derive(Parser, Debug, PartialEq, Eq)]
#[command(name = "isyncyoud", version, about = "iSyncYou engine daemon")]
struct Args {
    /// Configuration file.
    #[arg(long, default_value = "isyncyou.toml")]
    config: PathBuf,
    /// Address to bind the local web UI/API (localhost only by default).
    #[arg(long, default_value = "127.0.0.1:8765")]
    bind: String,
    /// Liveness heartbeat interval in seconds (0 disables).
    #[arg(long, default_value_t = 300)]
    heartbeat_secs: u64,
}

fn main() {
    let args = Args::parse();
    if let Err(e) = run(&args.config, &args.bind, args.heartbeat_secs) {
        eprintln!("isyncyoud: error: {e}");
        std::process::exit(1);
    }
}

fn run(config: &Path, bind: &str, heartbeat_secs: u64) -> Result<(), String> {
    let cfg = load_config(config)?;
    let n = cfg.accounts.len();
    eprintln!("isyncyoud: {n} account(s) configured; serving web UI on http://{bind}/");

    if heartbeat_secs > 0 {
        let bind = bind.to_string();
        std::thread::spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_secs(heartbeat_secs));
            // Liveness only — no store access (the web UI opens stores per request
            // and holds the single-instance lock momentarily; the daemon must not
            // hold it open).
            eprintln!("isyncyoud: alive, {n} account(s), web UI on http://{bind}/");
        });
    }

    let router = isyncyou_webui::Router::new(cfg);
    isyncyou_webui::serve(bind, router).map_err(|e| format!("serve: {e}"))
}

fn load_config(path: &Path) -> Result<Config, String> {
    let cfg = Config::load(path)?;
    cfg.validate().map_err(|errs| errs.join("; "))?;
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_defaults() {
        let a = Args::try_parse_from(["isyncyoud"]).unwrap();
        assert_eq!(
            a,
            Args {
                config: "isyncyou.toml".into(),
                bind: "127.0.0.1:8765".into(),
                heartbeat_secs: 300,
            }
        );
    }

    #[test]
    fn parses_overrides() {
        let a = Args::try_parse_from([
            "isyncyoud",
            "--config",
            "/etc/isyncyou.toml",
            "--bind",
            "0.0.0.0:9000",
            "--heartbeat-secs",
            "0",
        ])
        .unwrap();
        assert_eq!(a.config, PathBuf::from("/etc/isyncyou.toml"));
        assert_eq!(a.bind, "0.0.0.0:9000");
        assert_eq!(a.heartbeat_secs, 0);
    }

    #[test]
    fn run_rejects_invalid_config() {
        let dir = std::env::temp_dir().join(format!("isyncyoud-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("bad.toml");
        // archive_root == sync_root is invalid -> load_config errors before serving
        std::fs::write(
            &p,
            "[[accounts]]\nid=\"a\"\nusername=\"a@o\"\nsync_root=\"/x\"\narchive_root=\"/x\"\n",
        )
        .unwrap();
        assert!(load_config(&p).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
