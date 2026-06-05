//! `isyncyou-doctor` — standalone health/recovery checker.
//!
//! Deliberately minimal-dependency so it runs even when the daemon/GUI are
//! broken. It checks the configuration, each account's store file, and free disk
//! space, prints a report, and exits non-zero if anything is red.
//!
//! Usage: `isyncyou-doctor [--config <path>]` (default `isyncyou.toml`).

use isyncyou_doctor_lib::{parse_config_arg, run_checks, Level};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let config = parse_config_arg(&args);
    let report = run_checks(&config);

    println!("iSyncYou doctor — {}", config.display());
    for c in &report.checks {
        println!("  {} {:<14} {}", c.level.mark(), c.name, c.detail);
    }
    match report.worst() {
        Level::Fail => {
            println!("status: PROBLEMS FOUND");
            std::process::exit(1);
        }
        Level::Warn => println!("status: ok with warnings"),
        Level::Ok => println!("status: healthy"),
    }
}
