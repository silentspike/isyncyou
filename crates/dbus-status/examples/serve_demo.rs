//! Serve the FileStatus provider on the session bus over a seeded store, for
//! manual/live verification with `gdbus`/`busctl`.
//!
//! ```text
//! cargo run -p isyncyou-dbus-status --example serve_demo -- /tmp/isy-demo
//! gdbus call --session --dest org.silentspike.iSyncYou \
//!   --object-path /org/silentspike/iSyncYou/FileStatus \
//!   --method org.silentspike.iSyncYou.FileStatus.Status /tmp/isy-demo/OneDrive/IMG.jpg
//! ```
//! It prints the seeded paths + their expected status, then blocks serving.

#[cfg(target_os = "linux")]
fn main() {
    use isyncyou_dbus_status::{serve_blocking, AccountRoot, StoreStatusProvider};
    use std::sync::Arc;

    let base = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/tmp/isy-demo".to_string());
    let base = std::path::PathBuf::from(base);
    let root = base.join("OneDrive");
    let db = base.join("Archive").join(".isyncyou-store.db");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(base.join("Archive")).unwrap();
    let _ = std::fs::remove_file(&db);

    let store = isyncyou_store::Store::open(&db).unwrap();
    for (rid, name, state) in [
        ("r1", "IMG.jpg", "clean"),
        ("r2", "Draft.txt", "localDirty"),
        ("r3", "Bericht.pdf", "conflict"),
    ] {
        let mut it = isyncyou_store::Item::new("demo", "onedrive", rid, name, "file");
        it.local_path = Some(root.join(name).to_string_lossy().into_owned());
        it.sync_state = state.into();
        store.upsert_item(&it).unwrap();
        println!("seeded {} ({state})", root.join(name).display());
    }
    drop(store);

    let provider = Arc::new(StoreStatusProvider::new(vec![AccountRoot {
        sync_root: root,
        store_db: db,
    }]));
    eprintln!("serving org.silentspike.iSyncYou on the session bus (Ctrl-C to stop)…");
    if let Err(e) = serve_blocking(provider) {
        eprintln!("serve failed: {e}");
        std::process::exit(1);
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("serve_demo is Linux-only (needs a DBus session bus).");
}
