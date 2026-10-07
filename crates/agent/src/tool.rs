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
    /// keyword passes missed. All authority comes from the preceding Search result.
    DeepSearch {
        activity_id: String,
        continuation: String,
        candidates: Vec<String>,
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
        /// Preferred shape: `link` creates a sharing link; `invite` emails named recipients.
        /// Omitted mode keeps old callers working: `recipient`/`recipients` => invite, else link.
        #[serde(default)]
        mode: Option<String>,
        /// Link mode: `view`, `edit`, or `embed`.
        #[serde(default)]
        link_type: Option<String>,
        /// Link mode: `anonymous`, `organization`, or `users`.
        #[serde(default)]
        scope: Option<String>,
        /// Invite mode: one or more recipient emails. Public output only shows a count.
        #[serde(default)]
        recipients: Vec<String>,
        /// Invite mode: `read` or `write`.
        #[serde(default)]
        role: Option<String>,
        /// Backwards-compatible single-recipient invite input.
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

/// Crash-recovery behavior is deliberately separate from confirmation class. In
/// particular, `RestoreLocal` is Read-Class but has a local filesystem effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryPolicy {
    RepeatableReadAndCompare,
    IdempotentLocalMaterialize,
    NeverRepeat,
}

/// The single authorization/recovery policy for every tool action.
///
/// Keeping both decisions in one value prevents a confirmed cloud effect from
/// accidentally becoming replayable, and keeps `RestoreLocal` immediate while
/// acknowledging its idempotent local filesystem effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolPolicy {
    ImmediateRepeatableRead,
    ImmediateIdempotentLocalMaterialize,
    ConfirmedEffectNeverRepeat,
}

impl ToolPolicy {
    pub const fn class(self) -> ToolClass {
        match self {
            Self::ImmediateRepeatableRead | Self::ImmediateIdempotentLocalMaterialize => {
                ToolClass::Read
            }
            Self::ConfirmedEffectNeverRepeat => ToolClass::Destructive,
        }
    }

    pub const fn recovery(self) -> RecoveryPolicy {
        match self {
            Self::ImmediateRepeatableRead => RecoveryPolicy::RepeatableReadAndCompare,
            Self::ImmediateIdempotentLocalMaterialize => RecoveryPolicy::IdempotentLocalMaterialize,
            Self::ConfirmedEffectNeverRepeat => RecoveryPolicy::NeverRepeat,
        }
    }
}

impl ToolAction {
    pub const fn policy(&self) -> ToolPolicy {
        match self {
            ToolAction::Search { .. }
            | ToolAction::DeepSearch { .. }
            | ToolAction::Read { .. }
            | ToolAction::List { .. }
            | ToolAction::Export { .. } => ToolPolicy::ImmediateRepeatableRead,
            ToolAction::RestoreLocal { .. } => ToolPolicy::ImmediateIdempotentLocalMaterialize,
            ToolAction::Backup { .. }
            | ToolAction::RestoreCloud { .. }
            | ToolAction::LiveWrite { .. }
            | ToolAction::Share { .. } => ToolPolicy::ConfirmedEffectNeverRepeat,
        }
    }

    pub fn recovery_policy(&self) -> RecoveryPolicy {
        self.policy().recovery()
    }

    /// Classify the action (REQ-AGENT-002).
    pub fn class(&self) -> ToolClass {
        self.policy().class()
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
            | ToolAction::Read { account, .. }
            | ToolAction::List { account, .. }
            | ToolAction::Export { account, .. }
            | ToolAction::RestoreLocal { account, .. }
            | ToolAction::Backup { account, .. }
            | ToolAction::RestoreCloud { account, .. }
            | ToolAction::LiveWrite { account, .. }
            | ToolAction::Share { account, .. } => account,
            ToolAction::DeepSearch { .. } => "",
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
     deep-search {activity_id, continuation, candidates} · \
     read {account, service, id, max_bytes?} · \
     list {account, service, parent?, limit?, offset?} · \
     export {account, service, id} · \
     restore-local {account, service, id} · \
     backup {account, services?} [confirm] · \
     restore-cloud {account, service, id} [confirm] · \
     live-write {account, service, target?, change} [confirm] · \
     share {account, service, id, mode?, link_type?, scope?, recipients?, role?, recipient?} [confirm]. \
     There is no shell/filesystem/OS/network op."
        .to_string()
}

pub const REJECTED_TOOL_HELP_SCHEMA_VERSION: u32 = 2;
pub const INVALID_TOOL_ARGUMENTS_CODE: &str = "invalid_tool_arguments";
const REJECTED_TOOL_HELP_V1: &str = "invalid isyncyou tool call\n\nisyncyou tool — ops (M365 domain only): search {account, services?, query, limit?} · deep-search {account, services?, query, cursor?, max_reads?} · read {account, service, id, max_bytes?} · list {account, service, parent?, limit?, offset?} · export {account, service, id} · restore-local {account, service, id} · backup {account, services?} [confirm] · restore-cloud {account, service, id} [confirm] · live-write {account, service, target?, change} [confirm] · share {account, service, id, mode?, link_type?, scope?, recipients?, role?, recipient?} [confirm]. There is no shell/filesystem/OS/network op.";

/// Render the stable model-visible correction for a rejected tool call.
///
/// Retaining renderers by version makes crash recovery independent from Serde's
/// diagnostic wording and from future schema changes.
pub fn render_rejected_tool_help(version: u32, code: &str) -> Option<String> {
    if code != INVALID_TOOL_ARGUMENTS_CODE {
        return None;
    }
    match version {
        1 => Some(REJECTED_TOOL_HELP_V1.to_string()),
        REJECTED_TOOL_HELP_SCHEMA_VERSION => {
            Some(format!("invalid isyncyou tool call\n\n{}", help_text()))
        }
        _ => None,
    }
}

/// Parse a model tool input into a typed [`ToolAction`]. On failure the error carries
/// the [`help_text`] so the model can correct itself rather than crash the turn.
pub fn parse_action(input: &serde_json::Value) -> Result<ToolAction, String> {
    serde_json::from_value::<ToolAction>(input.clone()).map_err(|_| {
        render_rejected_tool_help(
            REJECTED_TOOL_HELP_SCHEMA_VERSION,
            INVALID_TOOL_ARGUMENTS_CODE,
        )
        .expect("the compiled rejected-tool renderer must exist")
    })
}

/// Public stream representation for a parsed tool call.
///
/// Search inputs contain account, query, and continuation authority and are therefore
/// reduced to a closed operation summary. Other read-class behavior remains compatible
/// until its action-specific projection moves into the shared read-output contract.
pub fn public_tool_call_input(
    action: &ToolAction,
    _raw_input: &serde_json::Value,
) -> serde_json::Value {
    fn public_service(service: &str) -> Option<&str> {
        match service {
            "mail" | "calendar" | "contacts" | "todo" | "onenote" | "onedrive" => Some(service),
            _ => None,
        }
    }

    fn closed_value<'a>(value: Option<&'a String>, allowed: &[&str]) -> Option<&'a str> {
        let value = value?.as_str();
        allowed.contains(&value).then_some(value)
    }

    match action {
        ToolAction::Search { services, .. } => {
            return serde_json::json!({
                "op": action.op(),
                "service_count": services.len(),
                "redacted": true
            });
        }
        ToolAction::DeepSearch { candidates, .. } => {
            return serde_json::json!({
                "op": action.op(),
                "selected_candidate_count": candidates.len(),
                "redacted": true
            });
        }
        _ => {}
    }
    let mut out = serde_json::Map::new();
    out.insert("op".to_string(), serde_json::json!(action.op()));
    out.insert("redacted".to_string(), serde_json::json!(true));
    if let Some(service) = action.service().and_then(public_service) {
        out.insert("service".to_string(), serde_json::json!(service));
    }
    match action {
        ToolAction::Backup { services, .. } => {
            out.insert(
                "service_count".to_string(),
                serde_json::json!(services.len()),
            );
        }
        ToolAction::LiveWrite { change, .. } => {
            if let Some(verb) = change.get("verb").and_then(serde_json::Value::as_str) {
                if [
                    "set_read",
                    "set_flag",
                    "set_categories",
                    "move",
                    "create_draft",
                    "send_draft",
                    "create",
                    "update",
                    "delete",
                    "respond",
                    "complete",
                    "checklist_add",
                    "checklist_toggle",
                    "checklist_delete",
                    "list_create",
                    "list_delete",
                    "append",
                ]
                .contains(&verb)
                {
                    out.insert("verb".to_string(), serde_json::json!(verb));
                }
            }
        }
        ToolAction::Share {
            recipient,
            recipients,
            mode,
            link_type,
            scope,
            role,
            ..
        } => {
            if let Some(mode) = closed_value(mode.as_ref(), &["link", "invite"]) {
                out.insert("mode".to_string(), serde_json::json!(mode));
            }
            if let Some(link_type) = closed_value(link_type.as_ref(), &["view", "edit", "embed"]) {
                out.insert("link_type".to_string(), serde_json::json!(link_type));
            }
            if let Some(scope) =
                closed_value(scope.as_ref(), &["anonymous", "organization", "users"])
            {
                out.insert("scope".to_string(), serde_json::json!(scope));
            }
            if let Some(role) = closed_value(role.as_ref(), &["read", "write"]) {
                out.insert("role".to_string(), serde_json::json!(role));
            }
            out.insert(
                "recipient_count".to_string(),
                serde_json::json!(recipients.len() + usize::from(recipient.is_some())),
            );
        }
        ToolAction::RestoreCloud { .. } => {}
        ToolAction::Search { .. }
        | ToolAction::DeepSearch { .. }
        | ToolAction::Read { .. }
        | ToolAction::List { .. }
        | ToolAction::Export { .. }
        | ToolAction::RestoreLocal { .. } => {}
    }
    serde_json::Value::Object(out)
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
                "activity_id": { "type": "string" },
                "continuation": { "type": "string" },
                "candidates": {
                    "type": "array",
                    "maxItems": 12,
                    "items": { "type": "string" }
                },
                "max_bytes": { "type": "integer" },
                "parent": { "type": "string" },
                "target": { "type": "string" },
                "mode": { "type": "string", "enum": ["link", "invite"] },
                "link_type": { "type": "string", "enum": ["view", "edit", "embed"] },
                "scope": { "type": "string", "enum": ["anonymous", "organization", "users"] },
                "recipients": { "type": "array", "items": { "type": "string" } },
                "role": { "type": "string", "enum": ["read", "write"] },
                "recipient": { "type": "string" },
                "change": {
                    "type": "object",
                    "description": "A live-write change. Every change requires verb. For mail read state use exactly verb=set_read with the boolean is_read field.",
                    "properties": {
                        "verb": {
                            "type": "string",
                            "enum": [
                                "set_read", "set_flag", "set_categories", "move",
                                "create_draft", "send_draft", "create", "update",
                                "delete", "respond", "complete", "checklist_add",
                                "checklist_toggle", "checklist_delete", "list_create",
                                "list_delete", "append"
                            ]
                        },
                        "is_read": { "type": "boolean" }
                    },
                    "required": ["verb"]
                }
            },
            "required": ["op"]
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
    fn tool_schema_requires_canonical_live_write_verb_and_read_field() {
        let schema = tool_schema();
        let change = &schema["input_schema"]["properties"]["change"];
        assert_eq!(change["type"], "object");
        assert_eq!(change["required"], serde_json::json!(["verb"]));
        assert_eq!(change["properties"]["is_read"]["type"], "boolean");
        assert!(change["properties"].get("value").is_none());
        assert!(change["properties"]["verb"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .any(|verb| verb == "set_read"));
    }

    #[test]
    fn public_search_tool_call_omits_query_account_continuation_and_candidates() {
        let search = ToolAction::Search {
            account: "private-account".into(),
            services: vec!["mail".into(), "onedrive".into()],
            query: "private query".into(),
            limit: Some(12),
        };
        let deep = ToolAction::DeepSearch {
            activity_id: "abcdefghijklmnopqrstuv".into(),
            continuation: "private-continuation".into(),
            candidates: vec!["private-candidate".into()],
        };
        for action in [&search, &deep] {
            let public = public_tool_call_input(
                action,
                &json!({
                    "account": "private-account",
                    "query": "private query",
                    "continuation": "private-continuation",
                    "candidates": ["private-candidate"]
                }),
            );
            let encoded = public.to_string();
            assert_eq!(public["redacted"], true);
            assert!(!encoded.contains("private-account"));
            assert!(!encoded.contains("private query"));
            assert!(!encoded.contains("private-continuation"));
            assert!(!encoded.contains("private-candidate"));
        }
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
    fn parse_share_accepts_explicit_link_and_invite_shapes() {
        let link = parse_action(&json!({
            "op": "share",
            "account": "me",
            "service": "onedrive",
            "id": "item-1",
            "mode": "link",
            "link_type": "edit",
            "scope": "organization"
        }))
        .unwrap();
        assert_eq!(
            link,
            ToolAction::Share {
                account: "me".into(),
                service: "onedrive".into(),
                id: "item-1".into(),
                mode: Some("link".into()),
                link_type: Some("edit".into()),
                scope: Some("organization".into()),
                recipients: vec![],
                role: None,
                recipient: None,
            }
        );

        let invite = parse_action(&json!({
            "op": "share",
            "account": "me",
            "service": "onedrive",
            "id": "item-1",
            "mode": "invite",
            "recipients": ["alpha@example.com", "beta@example.com"],
            "role": "write"
        }))
        .unwrap();
        assert_eq!(invite.class(), ToolClass::Destructive);
        let public = public_tool_call_input(&invite, &json!({}));
        assert_eq!(public["recipient_count"], 2);
        assert!(!public.to_string().contains("alpha@example.com"));
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

    #[test]
    fn recovery_policy_is_exhaustive_for_every_tool_action() {
        let actions = [
            (
                json!({"op":"search","account":"me","query":"q"}),
                RecoveryPolicy::RepeatableReadAndCompare,
            ),
            (
                json!({
                    "op":"deep-search",
                    "activity_id":"abcdefghijklmnopqrstuv",
                    "continuation":"opaque",
                    "candidates":[]
                }),
                RecoveryPolicy::RepeatableReadAndCompare,
            ),
            (
                json!({"op":"read","account":"me","service":"mail","id":"x"}),
                RecoveryPolicy::RepeatableReadAndCompare,
            ),
            (
                json!({"op":"list","account":"me","service":"mail"}),
                RecoveryPolicy::RepeatableReadAndCompare,
            ),
            (
                json!({"op":"export","account":"me","service":"mail","id":"x"}),
                RecoveryPolicy::RepeatableReadAndCompare,
            ),
            (
                json!({"op":"restore-local","account":"me","service":"mail","id":"x"}),
                RecoveryPolicy::IdempotentLocalMaterialize,
            ),
            (
                json!({"op":"backup","account":"me","services":["mail"]}),
                RecoveryPolicy::NeverRepeat,
            ),
            (
                json!({"op":"restore-cloud","account":"me","service":"mail","id":"x"}),
                RecoveryPolicy::NeverRepeat,
            ),
            (
                json!({"op":"live-write","account":"me","service":"mail","change":{}}),
                RecoveryPolicy::NeverRepeat,
            ),
            (
                json!({"op":"share","account":"me","service":"mail","id":"x","recipient":"a@example.invalid"}),
                RecoveryPolicy::NeverRepeat,
            ),
        ];
        for (input, expected) in actions {
            assert_eq!(parse_action(&input).unwrap().recovery_policy(), expected);
        }
    }

    fn all_policy_actions() -> Vec<ToolAction> {
        vec![
            parse_action(&json!({"op":"search","account":"private-account","query":"private-query"})).unwrap(),
            parse_action(&json!({"op":"deep-search","activity_id":"abcdefghijklmnopqrstuv","continuation":"private-continuation","candidates":["private-candidate"]})).unwrap(),
            parse_action(&json!({"op":"read","account":"private-account","service":"mail","id":"private-item"})).unwrap(),
            parse_action(&json!({"op":"list","account":"private-account","service":"mail","parent":"private-parent","limit":10,"offset":20})).unwrap(),
            parse_action(&json!({"op":"export","account":"private-account","service":"mail","id":"private-item"})).unwrap(),
            parse_action(&json!({"op":"restore-local","account":"private-account","service":"onedrive","id":"private-item"})).unwrap(),
            parse_action(&json!({"op":"backup","account":"private-account","services":["mail"]})).unwrap(),
            parse_action(&json!({"op":"restore-cloud","account":"private-account","service":"mail","id":"private-item"})).unwrap(),
            parse_action(&json!({"op":"live-write","account":"private-account","service":"mail","target":"private-item","change":{"verb":"set_read","secret":"private-change"}})).unwrap(),
            parse_action(&json!({"op":"share","account":"private-account","service":"onedrive","id":"private-item","mode":"invite","recipients":["private@example.invalid"],"role":"read"})).unwrap(),
        ]
    }

    #[test]
    fn tool_authorization_and_recovery_policy_is_one_exhaustive_matrix() {
        let expected = [
            ToolPolicy::ImmediateRepeatableRead,
            ToolPolicy::ImmediateRepeatableRead,
            ToolPolicy::ImmediateRepeatableRead,
            ToolPolicy::ImmediateRepeatableRead,
            ToolPolicy::ImmediateRepeatableRead,
            ToolPolicy::ImmediateIdempotentLocalMaterialize,
            ToolPolicy::ConfirmedEffectNeverRepeat,
            ToolPolicy::ConfirmedEffectNeverRepeat,
            ToolPolicy::ConfirmedEffectNeverRepeat,
            ToolPolicy::ConfirmedEffectNeverRepeat,
        ];
        for (action, expected) in all_policy_actions().iter().zip(expected) {
            assert_eq!(action.policy(), expected, "{} policy", action.op());
            assert_eq!(action.class(), expected.class(), "{} class", action.op());
            assert_eq!(
                action.recovery_policy(),
                expected.recovery(),
                "{} recovery",
                action.op()
            );
        }
    }

    #[test]
    fn read_policy_is_exactly_search_deep_read_list_export_restore_local() {
        let read_ops = all_policy_actions()
            .into_iter()
            .filter(|action| action.class() == ToolClass::Read)
            .map(|action| action.op())
            .collect::<Vec<_>>();
        assert_eq!(
            read_ops,
            [
                "search",
                "deep-search",
                "read",
                "list",
                "export",
                "restore-local"
            ]
        );
    }

    #[test]
    fn confirmed_effect_policy_is_exactly_backup_restore_cloud_live_write_share() {
        let confirmed_ops = all_policy_actions()
            .into_iter()
            .filter(|action| action.policy() == ToolPolicy::ConfirmedEffectNeverRepeat)
            .map(|action| action.op())
            .collect::<Vec<_>>();
        assert_eq!(
            confirmed_ops,
            ["backup", "restore-cloud", "live-write", "share"]
        );
    }

    #[test]
    fn tool_policy_rejects_invalid_authorization_recovery_pairs() {
        let valid_pairs = [
            (
                ToolPolicy::ImmediateRepeatableRead.class(),
                ToolPolicy::ImmediateRepeatableRead.recovery(),
            ),
            (
                ToolPolicy::ImmediateIdempotentLocalMaterialize.class(),
                ToolPolicy::ImmediateIdempotentLocalMaterialize.recovery(),
            ),
            (
                ToolPolicy::ConfirmedEffectNeverRepeat.class(),
                ToolPolicy::ConfirmedEffectNeverRepeat.recovery(),
            ),
        ];
        assert!(!valid_pairs.contains(&(ToolClass::Read, RecoveryPolicy::NeverRepeat)));
        assert!(!valid_pairs.contains(&(
            ToolClass::Destructive,
            RecoveryPolicy::RepeatableReadAndCompare,
        )));
        assert!(!valid_pairs.contains(&(
            ToolClass::Destructive,
            RecoveryPolicy::IdempotentLocalMaterialize,
        )));
    }

    #[test]
    fn public_tool_call_projection_is_exhaustive_for_every_action() {
        for action in all_policy_actions() {
            let public = public_tool_call_input(&action, &json!({"raw":"private-raw"}));
            assert_eq!(public["op"], action.op());
            assert_eq!(public["redacted"], true);
            assert!(public.as_object().is_some());
        }
    }

    #[test]
    fn public_tool_call_projection_omits_account_item_recipient_change_and_authority_material() {
        for action in all_policy_actions() {
            let encoded = public_tool_call_input(
                &action,
                &json!({"token":"private-token","action_hash":"private-hash"}),
            )
            .to_string();
            for forbidden in [
                "private-account",
                "private-item",
                "private-parent",
                "private-query",
                "private-continuation",
                "private-candidate",
                "private-change",
                "private@example.invalid",
                "private-token",
                "private-hash",
            ] {
                assert!(
                    !encoded.contains(forbidden),
                    "{} leaked {forbidden}",
                    action.op()
                );
            }
        }
    }

    #[test]
    fn pairing_intent_is_not_a_tool_action_or_provider_schema_entry() {
        let schema = tool_schema().to_string();
        assert!(!schema.contains("pairing"));
        assert!(!schema.contains("session_archive"));
        assert!(parse_action(&json!({
            "op": "session_pairing_reveal",
            "account": "me"
        }))
        .is_err());
    }
}
