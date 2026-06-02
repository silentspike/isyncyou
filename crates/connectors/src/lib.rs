//! `isyncyou-connectors` — per-service connectors.
//!
//! - [`onedrive`] ingests the OneDrive delta walk (remote → local) and drives
//!   uploads/deletes (local → remote).
//! - [`mail`] ingests per-folder Outlook message deltas (Phase 2 backup).
//! - [`calendar`] ingests per-calendar `calendarView` deltas (Phase 2 backup).
//! - [`contacts`] ingests default + per-folder contact deltas (Phase 2 backup).
//! - [`todo`] ingests per-list Microsoft To Do task deltas (Phase 2 backup).
//! - [`onenote`] reconciles the OneNote page list (no delta) (Phase 2 backup).
//!
//! This completes the Phase-2 backup connector set.

pub mod archive;
pub mod calendar;
mod common;
pub mod contacts;
pub mod export;
pub mod mail;
pub mod mime;
pub mod onedrive;
pub mod onenote;
pub mod restore;
pub mod shared;
pub mod todo;

pub use archive::{
    backup_byte_bodies, backup_calendar_bodies, backup_contacts_bodies, backup_json_bodies,
    backup_onenote_bodies, backup_todo_bodies, ArchiveReport, BytesFetcher, JsonFetcher,
};
pub use calendar::{incremental_sync_calendar, CalendarReport};
pub use contacts::{backup_contact_photos, incremental_sync_contacts, ContactsReport, PhotoReport};
pub use export::{contact_to_vcard, event_to_ics};
pub use mail::{
    backup_message_bodies, incremental_sync_mail, index_mail_bodies, BodyReport, MailReport,
    MimeFetcher,
};
pub use mime::{extract_html, extract_text};
pub use onedrive::{
    apply_local_deletes, apply_local_modifies, incremental_sync, materialize_downloads,
    pending_local_deletes, push_delete, push_local_creates, push_upload, scan_local_creates,
    scan_local_modifies, ContentReplacer, Downloader, MaterializeReport, ModifyReport,
    PendingLocalDelete, RemoteWriter, SyncError, SyncReport,
};
pub use onenote::{incremental_sync_onenote, OneNoteReport};
pub use restore::{
    restore_contact, restore_event, restore_message, restore_task, sanitize_contact,
    sanitize_event, sanitize_task, MessageCreator, Restorer,
};
pub use shared::{sync_shared_with_me, SharedReport};
pub use todo::{incremental_sync_todo, TodoReport};

/// Serializes the `live_*` integration tests. They all exercise one shared,
/// rate-limited throwaway account, so running them concurrently self-throttles
/// (a `429` storm that can exhaust a single walk's retry budget). Each live test
/// acquires this process-wide gate first, so they run one at a time regardless
/// of the test harness's thread count (plan §16: the real test account is
/// exercised serialized). A panicking test poisons the lock; we recover the
/// guard so one failure doesn't cascade into "poisoned" failures for the rest.
#[cfg(test)]
pub(crate) fn live_test_gate() -> std::sync::MutexGuard<'static, ()> {
    static GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());
    GATE.lock().unwrap_or_else(|poison| poison.into_inner())
}
