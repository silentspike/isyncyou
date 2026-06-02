//! `isyncyou-connectors` — per-service connectors.
//!
//! Currently the OneDrive connector ([`onedrive`]): it ingests a Graph delta walk
//! into the store (remote → local). Mail/Calendar/Contacts/ToDo/OneNote connectors
//! (Phase 2) follow behind the same shape.

pub mod onedrive;

pub use onedrive::{incremental_sync, SyncError, SyncReport};
