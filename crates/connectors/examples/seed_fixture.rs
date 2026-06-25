//! Token-free fixture seeder for the CI `staging-e2e` job.
//!
//! Writes a store + archive + `isyncyou.toml` that the real daemon router can serve
//! with NO Microsoft-365 tokens, so the staging deploy/E2E smoke has data to drive
//! (a mail with a body, a body-less OneNote page, a calendar event, a contact, a
//! task). The on-disk layout matches what the connectors write in production: the
//! store lives at `<archive_root>/.isyncyou-store.db`, and each item's body/sidecar
//! is a file under `<archive_root>` at the connectors' `shard_rel` path, recorded in
//! the item's `local_path` (the router derives `has_body` from `local_path.is_some()`
//! — see gui/webui/src/lib.rs).
//!
//! Build/run with the plain (unencrypted) store profile so the fixture is a plaintext
//! SQLite that any daemon opens without a key:
//!   cargo run -p isyncyou-connectors --example seed_fixture \
//!     --no-default-features --features plain-store -- <dest-dir>

use isyncyou_connectors::shard_rel;
use isyncyou_store::{Item, Store};
use serde_json::json;
use std::path::Path;

const ACCOUNT: &str = "fixture";

fn main() {
    let dest = std::env::args()
        .nth(1)
        .expect("usage: seed_fixture <dest-dir>");
    let dest = Path::new(&dest);
    let archive = dest.join("archive");
    std::fs::create_dir_all(&archive).expect("create archive dir");
    std::fs::create_dir_all(dest.join("sync")).expect("create sync dir");

    let store = Store::open(archive.join(".isyncyou-store.db")).expect("open store");

    // Write a body/sidecar at the connectors' sharded path and return its archive-
    // relative path (what `local_path` must hold).
    let write = |service: &str, id: &str, ext: &str, bytes: &[u8]| -> String {
        let rel = shard_rel(service, id, ext);
        let abs = archive.join(&rel);
        std::fs::create_dir_all(abs.parent().unwrap()).expect("create shard dir");
        std::fs::write(&abs, bytes).expect("write sidecar");
        rel
    };

    // --- mail: one message WITH a body (.eml) + indexed sender -> the reader renders
    //     the sandboxed iframe (has_body = local_path is set). ---
    let mut mail = Item::new(
        ACCOUNT,
        "mail",
        "fx-mail-1",
        "Welcome to iSyncYou (fixture)",
        "message",
    );
    mail.sender = Some("iSyncYou Bot <bot@example.com>".into());
    mail.remote_mtime = Some("2026-06-25T10:00:00Z".into());
    store.upsert_item(&mail).expect("upsert mail");
    let eml = b"MIME-Version: 1.0\r\nContent-Type: text/html; charset=utf-8\r\n\r\n\
<html><body><h1>Fixture mail</h1><p>Seeded body for the CI staging-e2e smoke.</p></body></html>\r\n";
    let rel = write("mail", "fx-mail-1", "eml", eml);
    store
        .set_local_path(ACCOUNT, "mail", "fx-mail-1", Some(&rel))
        .expect("set mail body path");
    store
        .index_body(ACCOUNT, "mail", "fx-mail-1", "Fixture mail body for CI")
        .expect("index mail body");

    // --- onenote: notebook -> section -> page, the page WITHOUT a body so the reader
    //     renders the "not archived" card instead of a raw 404 (CC-3). ---
    store
        .upsert_item(&Item::new(
            ACCOUNT,
            "onenote",
            "fx-nb-1",
            "Fixture Notebook",
            "notebook",
        ))
        .expect("upsert notebook");
    let mut section = Item::new(ACCOUNT, "onenote", "fx-sec-1", "Fixture Section", "section");
    section.parent_remote_id = Some("fx-nb-1".into());
    store.upsert_item(&section).expect("upsert section");
    let mut page = Item::new(
        ACCOUNT,
        "onenote",
        "fx-page-1",
        "Fixture Page (no body)",
        "page",
    );
    page.parent_remote_id = Some("fx-sec-1".into());
    store.upsert_item(&page).expect("upsert page"); // local_path = None -> has_body = false

    // --- calendar: one event + its archived Graph JSON sidecar (the agenda preview
    //     reads start/end from local_path). ---
    let mut event = Item::new(ACCOUNT, "calendar", "fx-ev-1", "Fixture standup", "event");
    event.remote_mtime = Some("2026-06-26T09:00:00Z".into());
    store.upsert_item(&event).expect("upsert event");
    let cal_json = json!({
        "subject": "Fixture standup",
        "start": {"dateTime": "2026-06-26T09:00:00.0000000", "timeZone": "UTC"},
        "end": {"dateTime": "2026-06-26T09:30:00.0000000", "timeZone": "UTC"},
        "location": {"displayName": "Online"},
        "isAllDay": false,
    });
    let rel = write(
        "calendar",
        "fx-ev-1",
        "json",
        &serde_json::to_vec(&cal_json).unwrap(),
    );
    store
        .set_local_path(ACCOUNT, "calendar", "fx-ev-1", Some(&rel))
        .expect("set event sidecar path");

    // --- contacts: one contact + JSON sidecar. ---
    store
        .upsert_item(&Item::new(
            ACCOUNT,
            "contacts",
            "fx-con-1",
            "Ada Fixture",
            "contact",
        ))
        .expect("upsert contact");
    let con_json = json!({
        "displayName": "Ada Fixture",
        "emailAddresses": [{"address": "ada@example.com", "name": "Ada Fixture"}],
        "companyName": "Fixture Inc",
    });
    let rel = write(
        "contacts",
        "fx-con-1",
        "json",
        &serde_json::to_vec(&con_json).unwrap(),
    );
    store
        .set_local_path(ACCOUNT, "contacts", "fx-con-1", Some(&rel))
        .expect("set contact sidecar path");

    // --- todo: one list + one task + JSON sidecar. ---
    store
        .upsert_item(&Item::new(
            ACCOUNT,
            "todo",
            "fx-list-1",
            "Fixture List",
            "list",
        ))
        .expect("upsert list");
    let mut task = Item::new(ACCOUNT, "todo", "fx-task-1", "Fixture task", "task");
    task.parent_remote_id = Some("fx-list-1".into());
    store.upsert_item(&task).expect("upsert task");
    let todo_json = json!({
        "title": "Fixture task",
        "status": "notStarted",
        "importance": "normal",
        "body": {"content": "", "contentType": "text"},
    });
    let rel = write(
        "todo",
        "fx-task-1",
        "json",
        &serde_json::to_vec(&todo_json).unwrap(),
    );
    store
        .set_local_path(ACCOUNT, "todo", "fx-task-1", Some(&rel))
        .expect("set task sidecar path");

    // --- config the daemon serves the fixture from ---
    let toml = format!(
        "[[accounts]]\nid = \"{ACCOUNT}\"\nusername = \"fixture@example.com\"\n\
sync_root = \"{sync}\"\narchive_root = \"{arch}\"\n\n[sync]\npoll_interval_secs = 5\n",
        sync = dest.join("sync").display(),
        arch = archive.display(),
    );
    std::fs::write(dest.join("isyncyou.toml"), toml).expect("write config");

    println!("seeded fixture at {} (account={ACCOUNT})", dest.display());
}
