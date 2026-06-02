//! Render a sample status bar to a PNG (manual visual preview).
//!
//! `cargo run -p isyncyou-statusbar --example preview -- out.png`

use isyncyou_statusbar::{render_png, StatusView, SyncState, Transfer};

fn main() {
    let view = StatusView {
        account: "jan@outlook.com".into(),
        state: SyncState::Throttled { wait_secs: 14 },
        transfers: vec![
            Transfer {
                name: "IMG_2024.jpg".into(),
                up: false,
                percent: 71,
            },
            Transfer {
                name: "Beleg.pdf".into(),
                up: true,
                percent: 88,
            },
        ],
        down_mbps: 12.2,
        up_mbps: 3.2,
        queue: 14,
    };
    let out = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "statusbar-preview.png".into());
    std::fs::write(&out, render_png(&view)).expect("write png");
    eprintln!("wrote {out}");
}
