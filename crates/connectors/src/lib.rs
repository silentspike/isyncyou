//! `isyncyou-connectors` — per-service connectors.
//!
//! - [`onedrive`] ingests the OneDrive delta walk (remote → local) and drives
//!   uploads/deletes (local → remote).
//! - [`mail`] ingests per-folder Outlook message deltas (Phase 2 backup).
//! - [`calendar`] ingests per-calendar `calendarView` deltas (Phase 2 backup).
//! - [`contacts`] ingests default + per-folder contact deltas (Phase 2 backup).
//! - [`todo`] ingests per-list Microsoft To Do task deltas (Phase 2 backup).
//!
//! The OneNote connector follows behind the same shape.

pub mod calendar;
mod common;
pub mod contacts;
pub mod mail;
pub mod onedrive;
pub mod todo;

pub use calendar::{incremental_sync_calendar, CalendarReport};
pub use contacts::{incremental_sync_contacts, ContactsReport};
pub use mail::{incremental_sync_mail, MailReport};
pub use onedrive::{
    incremental_sync, push_delete, push_upload, RemoteWriter, SyncError, SyncReport,
};
pub use todo::{incremental_sync_todo, TodoReport};
