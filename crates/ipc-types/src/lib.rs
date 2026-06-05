//! `isyncyou-ipc-types` — Shared API/IPC types (serde) between engine, GUI and CLI.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Service {
    #[serde(rename = "onedrive")]
    OneDrive,
    Mail,
    Calendar,
    Contacts,
    Todo,
    #[serde(rename = "onenote")]
    OneNote,
    Shared,
}

impl Service {
    pub fn as_str(self) -> &'static str {
        match self {
            Service::OneDrive => "onedrive",
            Service::Mail => "mail",
            Service::Calendar => "calendar",
            Service::Contacts => "contacts",
            Service::Todo => "todo",
            Service::OneNote => "onenote",
            Service::Shared => "shared",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    Sync,
    Backup,
    Restore,
    Verify,
    Migration,
    Doctor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Queued,
    Running,
    Paused,
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthLevel {
    Green,
    Yellow,
    Red,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthSnapshot {
    pub account: String,
    pub level: HealthLevel,
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobSnapshot {
    pub id: String,
    pub account: String,
    pub kind: JobKind,
    pub status: JobStatus,
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferSnapshot {
    pub account: String,
    pub service: Service,
    pub name: String,
    pub bytes_done: u64,
    pub bytes_total: Option<u64>,
    pub status: JobStatus,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipc_status_wire_roundtrip() {
        let transfer = TransferSnapshot {
            account: "primary".into(),
            service: Service::OneDrive,
            name: "Reports/Q4.xlsx".into(),
            bytes_done: 320,
            bytes_total: Some(640),
            status: JobStatus::Running,
        };
        let json = serde_json::to_string(&transfer).unwrap();
        assert!(json.contains(r#""service":"onedrive""#));
        assert!(json.contains(r#""status":"running""#));
        let back: TransferSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back, transfer);
        assert_eq!(Service::OneNote.as_str(), "onenote");
    }
}
