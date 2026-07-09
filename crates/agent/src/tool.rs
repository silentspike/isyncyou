//! The single tool exposed to the model and its typed, internal representation.
//!
//! App-scope invariant (REQ-AGENT-001): there is exactly one tool, [`TOOL_NAME`]
//! (`isyncyou`), and every [`ToolAction`] variant acts only on the user's M365 domain.
//! No shell / filesystem / OS / device / free-form-HTTP action exists.

use serde::{Deserialize, Serialize};

/// The one and only tool name advertised to the model.
pub const TOOL_NAME: &str = "isyncyou";

/// The typed, internal form of a tool call. The model speaks a single `isyncyou` tool
/// whose `op` field selects the subcommand; this enum is the safe parse of it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case")]
pub enum ToolAction {
    /// Full-text search across the archived M365 services.
    Search {
        account: String,
        #[serde(default)]
        services: Vec<String>,
        query: String,
        #[serde(default)]
        limit: Option<u32>,
    },
    /// Agentic deep read (S-AG.18/#643): scan metadata and read candidate bodies the
    /// keyword passes (search) missed, budgeted + resumable via `cursor`, so a match whose
    /// wording never contains the query can still be found. Read-class.
    DeepSearch {
        account: String,
        #[serde(default)]
        services: Vec<String>,
        query: String,
        /// Resume offset into the unmatched-candidate list (from a prior call's `next_cursor`).
        #[serde(default)]
        cursor: Option<u32>,
        /// Max candidate bodies to read this call (budget); server-capped.
        #[serde(default)]
        max_reads: Option<u32>,
    },
    /// Read one archived item's content (byte-budgeted).
    Read {
        account: String,
        service: String,
        id: String,
        #[serde(default)]
        max_bytes: Option<u64>,
    },
    /// List items by service / container.
    List {
        account: String,
        service: String,
        #[serde(default)]
        parent: Option<String>,
        #[serde(default)]
        limit: Option<u32>,
        #[serde(default)]
        offset: Option<u32>,
    },
    /// Export an item to a portable file (ics/vcard/raw).
    Export {
        account: String,
        service: String,
        id: String,
    },
    /// Restore an archived item to a local file (no cloud mutation).
    RestoreLocal {
        account: String,
        service: String,
        id: String,
    },
    /// Pull a fresh backup of an account/services (heavy; confirmation-gated).
    Backup {
        account: String,
        #[serde(default)]
        services: Vec<String>,
    },
    /// Re-create an archived item in the cloud (destructive; confirmation-gated).
    RestoreCloud {
        account: String,
        service: String,
        id: String,
    },
    /// Mutate a live cloud item (mark read, flag, move, …; destructive; gated).
    LiveWrite {
        account: String,
        service: String,
        #[serde(default)]
        target: Option<String>,
        change: serde_json::Value,
    },
    /// Share an item outward (link / invite / permissions). Destructive/external; gated.
    Share {
        account: String,
        service: String,
        id: String,
        #[serde(default)]
        recipient: Option<String>,
    },
}

/// Read-class actions run immediately; destructive-class actions require confirmation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolClass {
    Read,
    Destructive,
}

impl ToolAction {
    /// Classify the action (REQ-AGENT-002).
    pub fn class(&self) -> ToolClass {
        match self {
            ToolAction::Search { .. }
            | ToolAction::DeepSearch { .. }
            | ToolAction::Read { .. }
            | ToolAction::List { .. }
            | ToolAction::Export { .. }
            | ToolAction::RestoreLocal { .. } => ToolClass::Read,
            ToolAction::Backup { .. }
            | ToolAction::RestoreCloud { .. }
            | ToolAction::LiveWrite { .. }
            | ToolAction::Share { .. } => ToolClass::Destructive,
        }
    }

    /// The subcommand name (matches the wire `op`).
    pub fn op(&self) -> &'static str {
        match self {
            ToolAction::Search { .. } => "search",
            ToolAction::DeepSearch { .. } => "deep-search",
            ToolAction::Read { .. } => "read",
            ToolAction::List { .. } => "list",
            ToolAction::Export { .. } => "export",
            ToolAction::RestoreLocal { .. } => "restore-local",
            ToolAction::Backup { .. } => "backup",
            ToolAction::RestoreCloud { .. } => "restore-cloud",
            ToolAction::LiveWrite { .. } => "live-write",
            ToolAction::Share { .. } => "share",
        }
    }

    pub fn account(&self) -> &str {
        match self {
            ToolAction::Search { account, .. }
            | ToolAction::DeepSearch { account, .. }
            | ToolAction::Read { account, .. }
            | ToolAction::List { account, .. }
            | ToolAction::Export { account, .. }
            | ToolAction::RestoreLocal { account, .. }
            | ToolAction::Backup { account, .. }
            | ToolAction::RestoreCloud { account, .. }
            | ToolAction::LiveWrite { account, .. }
            | ToolAction::Share { account, .. } => account,
        }
    }

    pub fn service(&self) -> Option<&str> {
        match self {
            ToolAction::Read { service, .. }
            | ToolAction::List { service, .. }
            | ToolAction::Export { service, .. }
            | ToolAction::RestoreLocal { service, .. }
            | ToolAction::RestoreCloud { service, .. }
            | ToolAction::LiveWrite { service, .. }
            | ToolAction::Share { service, .. } => Some(service),
            ToolAction::Search { .. }
            | ToolAction::DeepSearch { .. }
            | ToolAction::Backup { .. } => None,
        }
    }

    pub fn item_or_target(&self) -> Option<&str> {
        match self {
            ToolAction::Read { id, .. }
            | ToolAction::Export { id, .. }
            | ToolAction::RestoreLocal { id, .. }
            | ToolAction::RestoreCloud { id, .. }
            | ToolAction::Share { id, .. } => Some(id),
            ToolAction::LiveWrite { target, .. } => target.as_deref(),
            ToolAction::Search { .. }
            | ToolAction::DeepSearch { .. }
            | ToolAction::List { .. }
            | ToolAction::Backup { .. } => None,
        }
    }
}

/// Human/model-readable help, appended to a parse error (`--help`-on-error).
pub fn help_text() -> String {
    "isyncyou tool — ops (M365 domain only): \
     search {account, services?, query, limit?} · \
     deep-search {account, services?, query, cursor?, max_reads?} · \
     read {account, service, id, max_bytes?} · \
     list {account, service, parent?, limit?, offset?} · \
     export {account, service, id} · \
     restore-local {account, service, id} · \
     backup {account, services?} [confirm] · \
     restore-cloud {account, service, id} [confirm] · \
     live-write {account, service, target?, change} [confirm] · \
     share {account, service, id, recipient?} [confirm]. \
     There is no shell/filesystem/OS/network op."
        .to_string()
}

/// Parse a model tool input into a typed [`ToolAction`]. On failure the error carries
/// the [`help_text`] so the model can correct itself rather than crash the turn.
pub fn parse_action(input: &serde_json::Value) -> Result<ToolAction, String> {
    serde_json::from_value::<ToolAction>(input.clone())
        .map_err(|e| format!("invalid isyncyou tool call: {e}\n\n{}", help_text()))
}

/// The complete tool registry advertised to any provider. **Exactly one** tool — the
/// app-scope invariant (REQ-AGENT-001), asserted by a snapshot test.
pub fn registry_tool_names() -> Vec<&'static str> {
    vec![TOOL_NAME]
}

/// The JSON tool schema sent to the model (a single tool with an `op` selector).
pub fn tool_schema() -> serde_json::Value {
    serde_json::json!({
        "name": TOOL_NAME,
        "description": "Operate on the user's own Microsoft 365 archive and account: \
                        search, read, list, export, restore-local, backup, \
                        restore-cloud, live-write. This is the only tool; it cannot run \
                        shell commands, touch the filesystem, the OS, devices, or \
                        arbitrary network endpoints.",
        "input_schema": {
            "type": "object",
            "properties": {
                "op": {
                    "type": "string",
                    "enum": [
                        "search", "deep-search", "read", "list", "export",
                        "restore-local", "backup", "restore-cloud", "live-write", "share"
                    ]
                },
                "account": { "type": "string" },
                "service": { "type": "string" },
                "id": { "type": "string" },
                "query": { "type": "string" },
                "services": { "type": "array", "items": { "type": "string" } },
                "limit": { "type": "integer" },
                "offset": { "type": "integer" },
                "cursor": { "type": "integer" },
                "max_reads": { "type": "integer" },
                "max_bytes": { "type": "integer" },
                "parent": { "type": "string" },
                "target": { "type": "string" },
                "recipient": { "type": "string" },
                "change": {}
            },
            "required": ["op", "account"]
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_good_search_yields_typed_action() {
        let v = json!({"op": "search", "account": "me", "query": "spotify invoice", "limit": 5});
        let action = parse_action(&v).expect("should parse");
        assert_eq!(
            action,
            ToolAction::Search {
                account: "me".into(),
                services: vec![],
                query: "spotify invoice".into(),
                limit: Some(5),
            }
        );
        assert_eq!(action.class(), ToolClass::Read);
        assert_eq!(action.op(), "search");
    }

    #[test]
    fn parse_bad_input_returns_help_not_panic() {
        let bad = json!({"op": "rm", "path": "/etc/passwd"});
        let err = parse_action(&bad).expect_err("unknown op must error");
        assert!(
            err.contains("isyncyou tool"),
            "error should carry help text: {err}"
        );

        let missing = json!({"op": "read", "account": "me"}); // missing service/id
        assert!(parse_action(&missing).is_err());
    }

    #[test]
    fn parse_list_accepts_pagination_args() {
        let v = json!({
            "op": "list",
            "account": "me",
            "service": "onedrive",
            "parent": "root",
            "limit": 25,
            "offset": 50
        });
        let action = parse_action(&v).expect("should parse paged list");
        assert_eq!(
            action,
            ToolAction::List {
                account: "me".into(),
                service: "onedrive".into(),
                parent: Some("root".into()),
                limit: Some(25),
                offset: Some(50),
            }
        );
        assert_eq!(action.class(), ToolClass::Read);
    }

    #[test]
    fn classify_separates_read_from_destructive() {
        let read =
            json!({"op": "restore-local", "account": "me", "service": "onedrive", "id": "x"});
        assert_eq!(parse_action(&read).unwrap().class(), ToolClass::Read);
        for op in ["backup", "restore-cloud", "live-write", "share"] {
            let v = json!({"op": op, "account": "me", "service": "mail", "id": "x", "change": {}, "recipient": "a@b.c"});
            assert_eq!(
                parse_action(&v).unwrap().class(),
                ToolClass::Destructive,
                "{op} must be destructive"
            );
        }
    }

    #[test]
    fn registry_snapshot_exposes_only_isyncyou_tool() {
        // App-scope invariant (REQ-AGENT-001): exactly one tool, no shell/FS/OS/HTTP.
        let names = registry_tool_names();
        assert_eq!(
            names,
            vec!["isyncyou"],
            "the registry must expose exactly one tool"
        );
        assert_eq!(names.len(), 1);
        for forbidden in [
            "shell", "bash", "exec", "fs", "file", "http", "fetch", "os", "device",
        ] {
            assert!(
                !names.contains(&forbidden),
                "a forbidden tool '{forbidden}' must never be in the registry"
            );
        }
        assert_eq!(tool_schema()["name"], "isyncyou");
    }
}
