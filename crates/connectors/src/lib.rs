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

pub mod calendar;
mod common;
pub mod contacts;
pub mod mail;
pub mod onedrive;
pub mod onenote;
pub mod restore;
pub mod todo;

pub use calendar::{incremental_sync_calendar, CalendarReport};
pub use contacts::{incremental_sync_contacts, ContactsReport};
pub use mail::{backup_message_bodies, incremental_sync_mail, BodyReport, MailReport, MimeFetcher};
pub use onedrive::{
    incremental_sync, push_delete, push_upload, RemoteWriter, SyncError, SyncReport,
};
pub use onenote::{incremental_sync_onenote, OneNoteReport};
pub use restore::{
    restore_contact, restore_event, restore_message, restore_task, sanitize_contact,
    sanitize_event, sanitize_task, MessageCreator, Restorer,
};
pub use todo::{incremental_sync_todo, TodoReport};
