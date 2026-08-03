//! Closed, bounded public activity events for progressive archive search.

use crate::session_v2::{SourceRef, MAX_SOURCE_REF_BYTES};
use serde::{Deserialize, Serialize};

pub const ACTIVITY_SCHEMA_VERSION: u32 = 1;
pub const ACTIVITY_ID_BYTES: usize = 22;
pub const RESULT_KEY_BYTES: usize = 22;
pub const MAX_CURRENT_ITEM_BYTES: usize = 160;
pub const MAX_RESULT_NAME_BYTES: usize = 192;
pub const MAX_SENDER_BYTES: usize = 256;
pub const MAX_ITEM_ID_BYTES: usize = 512;
pub const MAX_DISPLAY_PATH_BYTES: usize = 768;
pub const MAX_ITEM_TYPE_BYTES: usize = 64;
pub const MAX_PARTIAL_RESULT_ITEMS: usize = 20;
pub const MAX_PUBLIC_COUNTER: u32 = 1_000_000;
pub const MAX_STAGE_PROGRESS_BYTES: usize = 4 * 1_024;
pub const MAX_PARTIAL_RESULT_BYTES: usize = 64 * 1_024;
pub const MAX_PUBLIC_TOOL_RESULT_BYTES: usize = 8 * 1_024;
pub const MAX_PUBLIC_TOOL_RESULT_SOURCES: usize = 3;

const SERVICES: &[&str] = &[
    "mail", "onedrive", "calendar", "contacts", "todo", "onenote",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityKind {
    ArchiveSearch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchStage {
    Names,
    Bodies,
    Deep,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StageStatus {
    Queued,
    Running,
    Complete,
    Failed,
    Skipped,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResultChange {
    Add,
    Enrich,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnExitKind {
    Final,
    PendingConfirmation,
    ProviderError,
    Cancelled,
    OutcomeUnknown,
    StepLimit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoverageNoteReason {
    Incomplete,
    BudgetReached,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CoverageNoteV1 {
    pub version: u32,
    pub reason: CoverageNoteReason,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProgressiveActivityExitV1 {
    pub activity_id: String,
    pub names_status: StageStatus,
    pub bodies_status: StageStatus,
    pub deep_status: StageStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProgressiveExitStateV1 {
    pub exit_version: u32,
    pub exit_kind: TurnExitKind,
    pub activities: Vec<ProgressiveActivityExitV1>,
    pub terminal_code: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProgressiveActivityFinalizationV1 {
    pub activity_id: String,
    pub deep_status: StageStatus,
    pub coverage_complete: bool,
    pub budget_reached: bool,
    pub continuation_available: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProgressiveFinalizationV1 {
    pub finalization_version: u32,
    pub activities: Vec<ProgressiveActivityFinalizationV1>,
    pub coverage_note: Option<CoverageNoteV1>,
    pub finalized_text_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ActivityError {
    #[error("invalid_activity_event")]
    InvalidEvent,
    #[error("activity_event_too_large")]
    EventTooLarge,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StageProgressV1 {
    pub schema_version: u32,
    pub activity_id: String,
    pub activity_kind: ActivityKind,
    pub stage: SearchStage,
    pub status: StageStatus,
    pub scanned: u32,
    pub total: Option<u32>,
    pub hits: u32,
    pub current_item: Option<String>,
    pub coverage_complete: Option<bool>,
    pub budget_reached: Option<bool>,
    pub continuation_available: Option<bool>,
}

impl StageProgressV1 {
    pub fn validate(&self) -> Result<(), ActivityError> {
        if self.schema_version != ACTIVITY_SCHEMA_VERSION
            || !valid_opaque_id(&self.activity_id, ACTIVITY_ID_BYTES)
            || self.scanned > MAX_PUBLIC_COUNTER
            || self.total.is_some_and(|total| total > MAX_PUBLIC_COUNTER)
            || self.hits > MAX_PUBLIC_COUNTER
            || self.current_item.as_ref().is_some_and(|value| {
                value.is_empty()
                    || value.len() > MAX_CURRENT_ITEM_BYTES
                    || has_forbidden_text(value)
            })
        {
            return Err(ActivityError::InvalidEvent);
        }
        let bytes = serde_json::to_vec(&self.public_json())
            .map_err(|_| ActivityError::InvalidEvent)?
            .len();
        if bytes > MAX_STAGE_PROGRESS_BYTES {
            return Err(ActivityError::EventTooLarge);
        }
        Ok(())
    }

    pub fn public_json(&self) -> serde_json::Value {
        serde_json::json!({
            "event": "stage_progress",
            "schema_version": self.schema_version,
            "activity_id": self.activity_id,
            "activity_kind": self.activity_kind,
            "stage": self.stage,
            "status": self.status,
            "scanned": self.scanned,
            "total": self.total,
            "hits": self.hits,
            "current_item": self.current_item,
            "coverage_complete": self.coverage_complete,
            "budget_reached": self.budget_reached,
            "continuation_available": self.continuation_available,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchResultPublicV1 {
    pub result_key: String,
    pub change: ResultChange,
    pub service: String,
    pub item_id: String,
    pub name: String,
    pub item_type: String,
    pub display_path: Option<String>,
    pub sender: Option<String>,
    pub body_available: bool,
    pub source: SourceRef,
}

impl SearchResultPublicV1 {
    pub fn validate(&self) -> Result<(), ActivityError> {
        if !valid_opaque_id(&self.result_key, RESULT_KEY_BYTES)
            || !SERVICES.contains(&self.service.as_str())
            || self.item_id.is_empty()
            || self.item_id.len() > MAX_ITEM_ID_BYTES
            || has_forbidden_text(&self.item_id)
            || self.name.is_empty()
            || self.name.len() > MAX_RESULT_NAME_BYTES
            || has_forbidden_text(&self.name)
            || self.item_type.is_empty()
            || self.item_type.len() > MAX_ITEM_TYPE_BYTES
            || has_forbidden_text(&self.item_type)
            || self
                .display_path
                .as_ref()
                .is_some_and(|path| !valid_display_path(path))
            || self.sender.as_ref().is_some_and(|sender| {
                sender.is_empty() || sender.len() > MAX_SENDER_BYTES || has_forbidden_text(sender)
            })
            || self.source.service != self.service
            || self.source.item_id != self.item_id
            || self
                .source
                .label
                .as_ref()
                .is_some_and(|label| label != &self.name)
            || !valid_source_ref(&self.source)
        {
            return Err(ActivityError::InvalidEvent);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PartialResultV1 {
    pub schema_version: u32,
    pub activity_id: String,
    pub stage: SearchStage,
    pub sequence: u16,
    pub items: Vec<SearchResultPublicV1>,
}

impl PartialResultV1 {
    pub fn validate(&self) -> Result<(), ActivityError> {
        if self.schema_version != ACTIVITY_SCHEMA_VERSION
            || !valid_opaque_id(&self.activity_id, ACTIVITY_ID_BYTES)
            || self.items.len() > MAX_PARTIAL_RESULT_ITEMS
            || self.items.iter().any(|item| item.validate().is_err())
        {
            return Err(ActivityError::InvalidEvent);
        }
        let bytes = serde_json::to_vec(&self.public_json())
            .map_err(|_| ActivityError::InvalidEvent)?
            .len();
        if bytes > MAX_PARTIAL_RESULT_BYTES {
            return Err(ActivityError::EventTooLarge);
        }
        Ok(())
    }

    pub fn public_json(&self) -> serde_json::Value {
        serde_json::json!({
            "event": "partial_result",
            "schema_version": self.schema_version,
            "activity_id": self.activity_id,
            "stage": self.stage,
            "sequence": self.sequence,
            "items": self.items,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicToolResultV1 {
    pub schema_version: u32,
    pub operation: String,
    pub activity_id: String,
    pub visible_hits: u32,
    pub coverage_complete: bool,
    pub budget_reached: bool,
    pub continuation_available: bool,
    pub sources: Vec<SourceRef>,
}

impl PublicToolResultV1 {
    pub fn validate(&self) -> Result<(), ActivityError> {
        if self.schema_version != ACTIVITY_SCHEMA_VERSION
            || !matches!(self.operation.as_str(), "search" | "deep-search")
            || !valid_opaque_id(&self.activity_id, ACTIVITY_ID_BYTES)
            || self.visible_hits > MAX_PUBLIC_COUNTER
            || self.sources.len() > MAX_PUBLIC_TOOL_RESULT_SOURCES
            || self.sources.iter().any(|source| !valid_source_ref(source))
        {
            return Err(ActivityError::InvalidEvent);
        }
        let bytes = serde_json::to_vec(self)
            .map_err(|_| ActivityError::InvalidEvent)?
            .len();
        if bytes > MAX_PUBLIC_TOOL_RESULT_BYTES {
            return Err(ActivityError::EventTooLarge);
        }
        Ok(())
    }
}

pub fn valid_source_ref(source: &SourceRef) -> bool {
    !source.service.is_empty()
        && SERVICES.contains(&source.service.as_str())
        && !source.item_id.is_empty()
        && source.item_id.len() <= MAX_ITEM_ID_BYTES
        && !has_forbidden_text(&source.item_id)
        && source.label.as_ref().is_none_or(|label| {
            !label.is_empty() && label.len() <= MAX_RESULT_NAME_BYTES && !has_forbidden_text(label)
        })
        && serde_json::to_vec(source).is_ok_and(|bytes| bytes.len() <= MAX_SOURCE_REF_BYTES)
}

pub fn valid_opaque_id(value: &str, exact_len: usize) -> bool {
    value.len() == exact_len
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

pub fn truncate_collapsed(value: &str, max_bytes: usize) -> String {
    let collapsed = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.len() <= max_bytes {
        return collapsed;
    }
    let mut end = max_bytes;
    while !collapsed.is_char_boundary(end) {
        end -= 1;
    }
    collapsed[..end].trim_end().to_string()
}

fn valid_display_path(path: &str) -> bool {
    if path.is_empty()
        || path.len() > MAX_DISPLAY_PATH_BYTES
        || path.starts_with('/')
        || path.contains('\\')
        || path.contains(':')
        || has_forbidden_text(path)
    {
        return false;
    }
    path.split('/')
        .all(|component| !component.is_empty() && !matches!(component, "." | ".."))
}

fn has_forbidden_text(value: &str) -> bool {
    value.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACTIVITY: &str = "abcdefghijklmnopqrstuv";
    const RESULT: &str = "zyxwvutsrqponmlkjihgfe";

    fn source() -> SourceRef {
        SourceRef {
            service: "mail".into(),
            item_id: "item-1".into(),
            label: Some("Quarterly report".into()),
        }
    }

    fn item() -> SearchResultPublicV1 {
        SearchResultPublicV1 {
            result_key: RESULT.into(),
            change: ResultChange::Add,
            service: "mail".into(),
            item_id: "item-1".into(),
            name: "Quarterly report".into(),
            item_type: "message".into(),
            display_path: Some("Inbox/Reports".into()),
            sender: None,
            body_available: true,
            source: source(),
        }
    }

    fn tool_result() -> PublicToolResultV1 {
        PublicToolResultV1 {
            schema_version: ACTIVITY_SCHEMA_VERSION,
            operation: "search".into(),
            activity_id: ACTIVITY.into(),
            visible_hits: 1,
            coverage_complete: false,
            budget_reached: false,
            continuation_available: true,
            sources: vec![source()],
        }
    }

    #[test]
    fn stage_progress_v1_serializes_closed_public_shape() {
        let event = StageProgressV1 {
            schema_version: ACTIVITY_SCHEMA_VERSION,
            activity_id: ACTIVITY.into(),
            activity_kind: ActivityKind::ArchiveSearch,
            stage: SearchStage::Deep,
            status: StageStatus::Running,
            scanned: 120,
            total: None,
            hits: 8,
            current_item: Some("Music payout".into()),
            coverage_complete: None,
            budget_reached: None,
            continuation_available: None,
        };
        event.validate().unwrap();
        let value = event.public_json();
        assert_eq!(value["event"], "stage_progress");
        assert_eq!(value["activity_kind"], "archive_search");
        assert_eq!(value["stage"], "deep");
        assert_eq!(value["status"], "running");
        assert!(value["total"].is_null());
    }

    #[test]
    fn stage_progress_rejects_unknown_stage_status_and_activity_kind() {
        for field in ["activity_kind", "stage", "status"] {
            let mut value = serde_json::json!({
                "schema_version": 1,
                "activity_id": ACTIVITY,
                "activity_kind": "archive_search",
                "stage": "names",
                "status": "running",
                "scanned": 0,
                "total": null,
                "hits": 0,
                "current_item": null,
                "coverage_complete": null,
                "budget_reached": null,
                "continuation_available": null
            });
            value[field] = serde_json::json!("unknown");
            assert!(serde_json::from_value::<StageProgressV1>(value).is_err());
        }
    }

    #[test]
    fn stage_progress_rejects_counter_above_public_cap() {
        let event = StageProgressV1 {
            schema_version: 1,
            activity_id: ACTIVITY.into(),
            activity_kind: ActivityKind::ArchiveSearch,
            stage: SearchStage::Names,
            status: StageStatus::Complete,
            scanned: MAX_PUBLIC_COUNTER + 1,
            total: None,
            hits: 0,
            current_item: None,
            coverage_complete: Some(false),
            budget_reached: Some(true),
            continuation_available: Some(false),
        };
        assert_eq!(event.validate(), Err(ActivityError::InvalidEvent));
    }

    #[test]
    fn partial_result_v1_enforces_item_and_byte_limits() {
        let valid = PartialResultV1 {
            schema_version: 1,
            activity_id: ACTIVITY.into(),
            stage: SearchStage::Bodies,
            sequence: 0,
            items: vec![item()],
        };
        valid.validate().unwrap();

        let mut too_many = valid.clone();
        too_many.items = vec![item(); MAX_PARTIAL_RESULT_ITEMS + 1];
        assert_eq!(too_many.validate(), Err(ActivityError::InvalidEvent));

        let mut too_large = valid;
        too_large.items[0].name = "x".repeat(MAX_RESULT_NAME_BYTES + 1);
        assert_eq!(too_large.validate(), Err(ActivityError::InvalidEvent));
    }

    #[test]
    fn progress_current_item_is_collapsed_and_bounded() {
        assert_eq!(
            truncate_collapsed("  Quarterly\n\tarchive   report  ", 160),
            "Quarterly archive report"
        );
        assert_eq!(
            truncate_collapsed(&"ä".repeat(100), MAX_CURRENT_ITEM_BYTES).len(),
            MAX_CURRENT_ITEM_BYTES
        );

        let base = StageProgressV1 {
            schema_version: ACTIVITY_SCHEMA_VERSION,
            activity_id: ACTIVITY.into(),
            activity_kind: ActivityKind::ArchiveSearch,
            stage: SearchStage::Bodies,
            status: StageStatus::Running,
            scanned: 1,
            total: None,
            hits: 1,
            current_item: Some("x".repeat(MAX_CURRENT_ITEM_BYTES)),
            coverage_complete: None,
            budget_reached: None,
            continuation_available: None,
        };
        base.validate().unwrap();

        let mut over = base;
        over.current_item = Some("x".repeat(MAX_CURRENT_ITEM_BYTES + 1));
        assert_eq!(over.validate(), Err(ActivityError::InvalidEvent));
    }

    #[test]
    fn public_tool_result_projection_is_closed_and_bounded() {
        let result = tool_result();
        result.validate().unwrap();
        let value = serde_json::to_value(&result).unwrap();
        let object = value.as_object().unwrap();
        assert_eq!(object.len(), 8);
        for private in [
            "provider_content",
            "deep_context",
            "continuation",
            "candidates",
            "snippet",
            "display_path",
        ] {
            assert!(!object.contains_key(private));
        }
        assert!(serde_json::to_vec(&result).unwrap().len() <= MAX_PUBLIC_TOOL_RESULT_BYTES);
    }

    #[test]
    fn public_tool_result_admits_at_most_three_sources_inside_8k() {
        let mut result = tool_result();
        result.sources = (0..MAX_PUBLIC_TOOL_RESULT_SOURCES)
            .map(|index| SourceRef {
                service: "mail".into(),
                item_id: format!("item-{index}"),
                label: Some("x".repeat(MAX_RESULT_NAME_BYTES)),
            })
            .collect();
        result.validate().unwrap();
        assert!(serde_json::to_vec(&result).unwrap().len() <= MAX_PUBLIC_TOOL_RESULT_BYTES);

        result.sources.push(source());
        assert_eq!(result.validate(), Err(ActivityError::InvalidEvent));
    }

    #[test]
    fn source_label_truncation_and_sourceref_worst_case_fit_existing_2k_cap() {
        let label = truncate_collapsed(
            &format!("  {}  ", "ä".repeat(MAX_RESULT_NAME_BYTES)),
            MAX_RESULT_NAME_BYTES,
        );
        assert_eq!(label.len(), MAX_RESULT_NAME_BYTES);

        let source = SourceRef {
            service: "onedrive".into(),
            item_id: "i".repeat(MAX_ITEM_ID_BYTES),
            label: Some(label),
        };
        assert!(valid_source_ref(&source));
        assert!(serde_json::to_vec(&source).unwrap().len() <= MAX_SOURCE_REF_BYTES);

        let mut oversized_id = source.clone();
        oversized_id.item_id.push('i');
        assert!(!valid_source_ref(&oversized_id));

        let mut oversized_label = source;
        oversized_label.label = Some("l".repeat(MAX_RESULT_NAME_BYTES + 1));
        assert!(!valid_source_ref(&oversized_label));
    }

    #[test]
    fn public_search_item_rejects_mismatched_service_item_id_or_source() {
        let mut service_mismatch = item();
        service_mismatch.source.service = "onedrive".into();
        assert_eq!(
            service_mismatch.validate(),
            Err(ActivityError::InvalidEvent)
        );

        let mut id_mismatch = item();
        id_mismatch.source.item_id = "other-item".into();
        assert_eq!(id_mismatch.validate(), Err(ActivityError::InvalidEvent));

        let mut label_mismatch = item();
        label_mismatch.source.label = Some("Other label".into());
        assert_eq!(label_mismatch.validate(), Err(ActivityError::InvalidEvent));
    }

    #[test]
    fn search_display_path_is_never_source_viewer_or_result_identity() {
        let mut value = item();
        value.item_id.clear();
        value.source.item_id.clear();
        value.display_path = Some("Inbox/Reports".into());
        assert_eq!(value.validate(), Err(ActivityError::InvalidEvent));

        let valid = item();
        assert_ne!(
            valid.result_key,
            valid.display_path.clone().expect("fixture display path")
        );
        assert!(!serde_json::to_value(&valid.source)
            .unwrap()
            .to_string()
            .contains("Inbox/Reports"));
    }

    #[test]
    fn search_display_path_rejects_local_absolute_uri_and_traversal_forms() {
        for path in [
            "/archive/mail/item",
            "C:/archive/item",
            "mail\\item",
            "mail/../item",
            "mail/./item",
            "file://archive/item",
            "https://example.invalid/item",
        ] {
            let mut value = item();
            value.display_path = Some(path.into());
            assert_eq!(value.validate(), Err(ActivityError::InvalidEvent));
        }
    }
}
