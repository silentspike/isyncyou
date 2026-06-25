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

    // --- richer demo dataset for the showcase landing page + a fuller CI smoke.
    //     Every added mail carries a body, so the newest-first first row always renders
    //     the sandboxed reader; all data is obviously synthetic (example.com). OneNote is
    //     left as the single body-less page on purpose (ci-ui-smoke asserts the last leaf
    //     renders the "not archived" card, never a 404 iframe). ---
    let more_mail = [
        ("fx-mail-2", "Your June backup finished — 1,284 items archived", "iSyncYou <bot@example.com>", "2026-06-25T14:32:00Z",
         "<h2>Backup complete</h2><p>All five Microsoft 365 services were indexed and archived: 1,284 items, 312 MB of bodies on disk.</p>", "June backup finished 1284 items archived bodies"),
        ("fx-mail-3", "Q3 roadmap review — agenda", "Grace Hopper <grace@example.com>", "2026-06-25T13:05:00Z",
         "<p>Hi team,</p><p>Agenda for Thursday: the unified live client, the standalone APK, and the new staged release pipeline.</p>", "Q3 roadmap review agenda live client APK release pipeline"),
        ("fx-mail-4", "Invoice #2041 — June", "Billing <billing@example.com>", "2026-06-25T11:48:00Z",
         "<p>Invoice #2041 is attached. Amount due: 49.00 EUR. Thank you for your business.</p>", "Invoice 2041 June amount due 49 EUR"),
        ("fx-mail-5", "Re: Team offsite logistics", "Ada Lovelace <ada@example.com>", "2026-06-24T18:20:00Z",
         "<p>Booked the room and lunch — see the calendar invite for the details.</p>", "Team offsite logistics room lunch calendar invite"),
        ("fx-mail-6", "Security digest: 3 new sign-ins", "Microsoft 365 <security@example.com>", "2026-06-24T08:00:00Z",
         "<p>We noticed sign-ins from 3 new devices this week. If this was you, no action is needed.</p>", "Security digest 3 new sign-ins devices week"),
        ("fx-mail-7", "Photos from the weekend", "Alan Turing <alan@example.com>", "2026-06-23T20:11:00Z",
         "<p>Shared 24 photos with you — they are in your OneDrive under Photos/Weekend.</p>", "Photos weekend OneDrive shared 24 Photos Weekend"),
        ("fx-mail-8", "Welcome aboard — getting started", "iSyncYou <bot@example.com>", "2026-06-22T09:00:00Z",
         "<p>Thanks for trying iSyncYou. Run <code>isyncyou backup</code> to archive everything, then open the web UI.</p>", "Welcome getting started isyncyou backup web UI"),
    ];
    for (id, subj, sender, mtime, body, idx) in more_mail {
        let mut m = Item::new(ACCOUNT, "mail", id, subj, "message");
        m.sender = Some(sender.into());
        m.remote_mtime = Some(mtime.into());
        store.upsert_item(&m).expect("upsert mail");
        let eml = format!(
            "MIME-Version: 1.0\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<html><body>{body}</body></html>\r\n"
        );
        let rel = write("mail", id, "eml", eml.as_bytes());
        store
            .set_local_path(ACCOUNT, "mail", id, Some(&rel))
            .expect("set mail body path");
        store
            .index_body(ACCOUNT, "mail", id, idx)
            .expect("index mail body");
    }

    let more_events = [
        (
            "fx-ev-2",
            "Q3 roadmap review",
            "2026-06-26T11:00:00",
            "2026-06-26T12:00:00",
            "Meeting Room A",
        ),
        (
            "fx-ev-3",
            "1:1 with Grace",
            "2026-06-26T14:00:00",
            "2026-06-26T14:30:00",
            "Online",
        ),
        (
            "fx-ev-4",
            "Design review — web UI",
            "2026-06-27T10:00:00",
            "2026-06-27T11:00:00",
            "Online",
        ),
        (
            "fx-ev-5",
            "Release retro",
            "2026-06-27T15:30:00",
            "2026-06-27T16:15:00",
            "Meeting Room B",
        ),
        (
            "fx-ev-6",
            "Lunch & learn: Rust",
            "2026-06-28T12:00:00",
            "2026-06-28T13:00:00",
            "Cafeteria",
        ),
        (
            "fx-ev-7",
            "Sprint planning",
            "2026-06-29T09:30:00",
            "2026-06-29T10:30:00",
            "Online",
        ),
    ];
    for (id, subj, start, end, loc) in more_events {
        let mut e = Item::new(ACCOUNT, "calendar", id, subj, "event");
        e.remote_mtime = Some(format!("{start}Z"));
        store.upsert_item(&e).expect("upsert event");
        let j = json!({
            "subject": subj,
            "start": {"dateTime": format!("{start}.0000000"), "timeZone": "UTC"},
            "end": {"dateTime": format!("{end}.0000000"), "timeZone": "UTC"},
            "location": {"displayName": loc},
            "isAllDay": false,
        });
        let rel = write("calendar", id, "json", &serde_json::to_vec(&j).unwrap());
        store
            .set_local_path(ACCOUNT, "calendar", id, Some(&rel))
            .expect("set event sidecar path");
    }

    let more_contacts = [
        (
            "fx-con-2",
            "Grace Hopper",
            "grace@example.com",
            "Compiler Co",
        ),
        (
            "fx-con-3",
            "Alan Turing",
            "alan@example.com",
            "Bletchley Ltd",
        ),
        (
            "fx-con-4",
            "Katherine Johnson",
            "katherine@example.com",
            "Orbital Systems",
        ),
        (
            "fx-con-5",
            "Linus Fixture",
            "linus@example.com",
            "Kernel Inc",
        ),
        (
            "fx-con-6",
            "Margaret Hamilton",
            "margaret@example.com",
            "Apollo Software",
        ),
        (
            "fx-con-7",
            "Tim Berners-Lee",
            "tim@example.com",
            "Web Foundation",
        ),
        (
            "fx-con-8",
            "Barbara Liskov",
            "barbara@example.com",
            "Abstraction Labs",
        ),
    ];
    for (id, name, email, company) in more_contacts {
        store
            .upsert_item(&Item::new(ACCOUNT, "contacts", id, name, "contact"))
            .expect("upsert contact");
        let j = json!({
            "displayName": name,
            "emailAddresses": [{"address": email, "name": name}],
            "companyName": company,
        });
        let rel = write("contacts", id, "json", &serde_json::to_vec(&j).unwrap());
        store
            .set_local_path(ACCOUNT, "contacts", id, Some(&rel))
            .expect("set contact sidecar path");
    }

    let more_tasks = [
        (
            "fx-task-2",
            "Review pull request #579",
            "notStarted",
            "high",
        ),
        (
            "fx-task-3",
            "Renew the TLS certificate",
            "notStarted",
            "normal",
        ),
        (
            "fx-task-4",
            "Back up the photo library",
            "completed",
            "normal",
        ),
        ("fx-task-5", "Plan the Q4 OKRs", "inProgress", "high"),
        ("fx-task-6", "Update dependencies", "notStarted", "low"),
        (
            "fx-task-7",
            "Write the release notes",
            "notStarted",
            "normal",
        ),
    ];
    for (id, title, status, importance) in more_tasks {
        let mut t = Item::new(ACCOUNT, "todo", id, title, "task");
        t.parent_remote_id = Some("fx-list-1".into());
        store.upsert_item(&t).expect("upsert task");
        let j = json!({
            "title": title,
            "status": status,
            "importance": importance,
            "body": {"content": "", "contentType": "text"},
        });
        let rel = write("todo", id, "json", &serde_json::to_vec(&j).unwrap());
        store
            .set_local_path(ACCOUNT, "todo", id, Some(&rel))
            .expect("set task sidecar path");
    }

    // --- onedrive: a small file/folder tree so the overview breakdown + file browser
    //     have content (id-based, with sizes; bodies are not archived for files). ---
    let drive = [
        (
            "fx-od-1",
            "Documents",
            "folder",
            0i64,
            "2026-06-25T10:00:00Z",
        ),
        ("fx-od-2", "Photos", "folder", 0, "2026-06-24T19:00:00Z"),
        (
            "fx-od-3",
            "Q3-roadmap.pptx",
            "file",
            2_456_120,
            "2026-06-25T13:10:00Z",
        ),
        (
            "fx-od-4",
            "invoice-2041.pdf",
            "file",
            184_320,
            "2026-06-25T11:50:00Z",
        ),
        (
            "fx-od-5",
            "IMG_2024.jpg",
            "file",
            3_201_044,
            "2026-06-23T20:05:00Z",
        ),
        (
            "fx-od-6",
            "budget.xlsx",
            "file",
            96_512,
            "2026-06-22T16:40:00Z",
        ),
        (
            "fx-od-7",
            "notes.md",
            "file",
            12_004,
            "2026-06-21T09:15:00Z",
        ),
        (
            "fx-od-8",
            "backup-archive.zip",
            "file",
            21_884_002,
            "2026-06-20T02:00:00Z",
        ),
    ];
    for (id, name, ty, size, mtime) in drive {
        let mut f = Item::new(ACCOUNT, "onedrive", id, name, ty);
        f.size = Some(size);
        f.remote_mtime = Some(mtime.into());
        store.upsert_item(&f).expect("upsert drive item");
    }

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
