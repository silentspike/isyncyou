//! Render a sample status bar to a PNG (manual visual preview).
//!
//! `cargo run -p isyncyou-statusbar --example preview -- [STATE] [out.png]`
//! STATE = synced | syncing | throttled | paused | error  (default: throttled)

use isyncyou_statusbar::{render_png, StatusView, SyncState, Transfer};

fn main() {
    let mut args = std::env::args().skip(1);
    let state_name = args.next().unwrap_or_else(|| "throttled".into());
    let out = args
        .next()
        .unwrap_or_else(|| "statusbar-preview.png".into());
    let state = match state_name.as_str() {
        "synced" => SyncState::Synced,
        "syncing" => SyncState::Syncing,
        "paused" => SyncState::Paused,
        "error" => SyncState::Error {
            reason: "Sign-in expired \u{2014} reconnect in the browser".into(),
        },
        _ => SyncState::Throttled { wait_secs: 14 },
    };
    let view = StatusView {
        account: "you@example.com".into(),
        state,
        transfers: vec![
            Transfer {
                name: "IMG_2024.jpg".into(),
                up: false,
                percent: 71,
            },
            Transfer {
                name: "invoice.pdf".into(),
                up: true,
                percent: 88,
            },
        ],
        down_mbps: 12.2,
        up_mbps: 3.2,
        queue: 14,
    };
    std::fs::write(&out, render_png(&view)).expect("write png");
    eprintln!("wrote {out} (state: {state_name})");
}
