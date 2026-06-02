//! `isyncyou-connectors` — per-service connectors.
//!
//! - [`onedrive`] ingests the OneDrive delta walk (remote → local) and drives
//!   uploads/deletes (local → remote).
//! - [`mail`] ingests per-folder Outlook message deltas (Phase 2 backup).
//!
//! Calendar/Contacts/ToDo/OneNote connectors follow behind the same shape.

pub mod mail;
pub mod onedrive;

pub use mail::{incremental_sync_mail, MailReport};
pub use onedrive::{
    incremental_sync, push_delete, push_upload, RemoteWriter, SyncError, SyncReport,
};
