use crate::AgentError;
use base64::Engine as _;
use ring::{digest, hmac};
use serde::{Deserialize, Serialize};
use std::fmt;

pub const PROGRESSIVE_SEARCH_WIRE_VERSION: u8 = 1;
pub const MAX_CONTINUATION_ASCII_BYTES: usize = 1_024;
pub const MAX_CONTINUATION_PAYLOAD_BYTES: usize = 640;
pub const MAX_CANDIDATES_PER_PAGE: usize = 64;
pub const MAX_SELECTED_CANDIDATES: usize = 12;
pub const MAX_METADATA_SCANNED_PER_CANDIDATE_PAGE: u32 = 500;
pub const MAX_METADATA_SCANNED_PER_CALL: u32 = 1_000;
pub const MAX_METADATA_SCANNED_PER_ACTIVITY: u32 = 16_000;

const SERVICES: [&str; 6] = [
    "mail", "calendar", "contacts", "todo", "onenote", "onedrive",
];

fn append_u32_len(bytes: &mut Vec<u8>, value: &[u8]) -> Result<(), AgentError> {
    let length = u32::try_from(value.len())
        .map_err(|_| AgentError::Provider("progressive_encoding_overflow".into()))?;
    bytes.extend_from_slice(&length.to_be_bytes());
    bytes.extend_from_slice(value);
    Ok(())
}

fn append_u16_len(bytes: &mut Vec<u8>, value: &[u8]) -> Result<(), AgentError> {
    let length = u16::try_from(value.len())
        .map_err(|_| AgentError::Provider("progressive_encoding_overflow".into()))?;
    bytes.extend_from_slice(&length.to_be_bytes());
    bytes.extend_from_slice(value);
    Ok(())
}

fn service_ordinal(service: &str) -> Option<u8> {
    SERVICES
        .iter()
        .position(|candidate| *candidate == service)
        .and_then(|index| u8::try_from(index).ok())
}

fn encode_32(value: &[u8; 32]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(value)
}

fn decode_32(value: &str) -> Result<[u8; 32], AgentError> {
    if value.len() != 43
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err(AgentError::ToolArgs(
            "invalid progressive digest encoding".into(),
        ));
    }
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|_| AgentError::ToolArgs("invalid progressive digest encoding".into()))?;
    decoded
        .try_into()
        .map_err(|_| AgentError::ToolArgs("invalid progressive digest encoding".into()))
}

pub fn admission_account_digest(resolved_account_key: &str) -> Result<[u8; 32], AgentError> {
    if resolved_account_key.is_empty() || resolved_account_key.len() > 128 {
        return Err(AgentError::ToolArgs("invalid resolved account".into()));
    }
    let mut bytes = Vec::with_capacity(resolved_account_key.len() + 2);
    append_u16_len(&mut bytes, resolved_account_key.as_bytes())?;
    let mut context = digest::Context::new(&digest::SHA256);
    context.update(b"isyncyou-agent-turn-account/v1");
    context.update(&bytes);
    Ok(context.finish().as_ref().try_into().unwrap())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalSearchScopeV1 {
    account: String,
    query: String,
    services: Vec<String>,
    effective_keyword_limit: u32,
    encoded: Vec<u8>,
    digest: [u8; 32],
}

impl CanonicalSearchScopeV1 {
    pub fn new(
        account: impl Into<String>,
        query: impl Into<String>,
        requested_services: Vec<String>,
        limit: Option<u32>,
    ) -> Result<Self, AgentError> {
        let account = account.into();
        let query = query.into();
        if account.is_empty() || account.len() > 128 {
            return Err(AgentError::ToolArgs("invalid resolved account".into()));
        }
        if query.is_empty()
            || query.len() > 2_048
            || query.trim_matches(char::is_whitespace) != query
        {
            return Err(AgentError::ToolArgs("invalid progressive query".into()));
        }
        let mut services = Vec::new();
        if requested_services.is_empty() {
            services.extend(SERVICES.iter().map(ToString::to_string));
        } else {
            for service in SERVICES {
                if requested_services
                    .iter()
                    .any(|candidate| candidate == service)
                {
                    services.push(service.to_string());
                }
            }
            let unique = requested_services
                .iter()
                .collect::<std::collections::HashSet<_>>();
            if unique.len() != services.len()
                || requested_services
                    .iter()
                    .any(|service| service_ordinal(service).is_none())
            {
                return Err(AgentError::ToolArgs(
                    "invalid progressive service scope".into(),
                ));
            }
        }
        let effective_keyword_limit = limit.unwrap_or(20);
        if effective_keyword_limit == 0 || effective_keyword_limit > 160 {
            return Err(AgentError::ToolArgs(
                "invalid progressive result limit".into(),
            ));
        }
        let mut encoded = Vec::with_capacity(account.len() + query.len() + services.len() + 10);
        encoded.push(1);
        append_u16_len(&mut encoded, account.as_bytes())?;
        append_u16_len(&mut encoded, query.as_bytes())?;
        encoded.push(services.len() as u8);
        for service in &services {
            encoded.push(service_ordinal(service).expect("validated service"));
        }
        encoded.extend_from_slice(&effective_keyword_limit.to_be_bytes());
        let mut context = digest::Context::new(&digest::SHA256);
        context.update(b"isyncyou-progressive-search-scope/v1");
        context.update(&encoded);
        let digest: [u8; 32] = context.finish().as_ref().try_into().unwrap();
        Ok(Self {
            account,
            query,
            services,
            effective_keyword_limit,
            encoded,
            digest,
        })
    }

    pub fn account(&self) -> &str {
        &self.account
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn services(&self) -> &[String] {
        &self.services
    }

    pub fn effective_keyword_limit(&self) -> u32 {
        self.effective_keyword_limit
    }

    pub fn encoded(&self) -> &[u8] {
        &self.encoded
    }

    pub fn digest(&self) -> [u8; 32] {
        self.digest
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadExecutionBindingV2 {
    pub session_id: String,
    pub request_id: String,
    pub tool_use_id: String,
    pub resolved_account_key: String,
    pub admission_account_digest: [u8; 32],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchActivityBindingV1 {
    pub session_id: String,
    pub request_id: String,
    pub activity_id: String,
    pub originating_search_tool_use_id: String,
    pub canonical_scope_digest: [u8; 32],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeepContinuationStateV1 {
    pub version: u32,
    pub activity_id: String,
    pub canonical_scope_digest: [u8; 32],
    pub service_index: u8,
    pub service_offset: u32,
    pub page: u16,
    pub metadata_scanned: u32,
    pub body_reads_used: u16,
    pub candidate_page_digest: [u8; 32],
    pub issued_at_provider_step: u8,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DeepContinuationWireV1 {
    v: u8,
    a: String,
    sc: String,
    si: u8,
    so: u32,
    p: u16,
    ms: u32,
    br: u16,
    cd: String,
    ps: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SearchCandidateMetadataV1 {
    pub candidate_key: String,
    pub service: String,
    pub name: String,
    pub sender: Option<String>,
    pub item_type: String,
    pub remote_mtime: Option<String>,
    pub size: Option<u64>,
}

#[derive(Clone, PartialEq, Eq)]
pub struct IssuedCandidateV1 {
    pub candidate_key: String,
    pub service: String,
    pub item_id: String,
    pub provider_metadata_digest: [u8; 32],
}

impl fmt::Debug for IssuedCandidateV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("IssuedCandidateV1")
            .field("candidate_key", &"[redacted]")
            .field("service", &self.service)
            .field("item_id", &"[redacted]")
            .finish()
    }
}

pub trait ProgressiveSearchAuthority: Send + Sync {
    fn activity_id(&self, binding: &ReadExecutionBindingV2) -> Result<String, AgentError>;
    fn seal_continuation(
        &self,
        activity: &SearchActivityBindingV1,
        state: &DeepContinuationStateV1,
    ) -> Result<String, AgentError>;
    fn open_continuation(
        &self,
        activity: &SearchActivityBindingV1,
        encoded: &str,
    ) -> Result<DeepContinuationStateV1, AgentError>;
    fn candidate_key(
        &self,
        continuation: &DeepContinuationStateV1,
        service: &str,
        item_id: &str,
    ) -> Result<String, AgentError>;
}

pub struct HmacProgressiveSearchAuthority {
    root: [u8; 32],
}

impl HmacProgressiveSearchAuthority {
    pub fn new(root: [u8; 32]) -> Self {
        Self { root }
    }

    fn hmac(&self, domain: &[u8], message: &[u8]) -> [u8; 32] {
        let key = hmac::Key::new(hmac::HMAC_SHA256, &self.root);
        let mut bytes = Vec::with_capacity(domain.len() + message.len());
        bytes.extend_from_slice(domain);
        bytes.extend_from_slice(message);
        hmac::sign(&key, &bytes).as_ref().try_into().unwrap()
    }

    fn continuation_message(
        activity: &SearchActivityBindingV1,
        payload: &[u8],
    ) -> Result<Vec<u8>, AgentError> {
        let mut message = Vec::new();
        append_u16_len(&mut message, activity.session_id.as_bytes())?;
        append_u16_len(&mut message, activity.request_id.as_bytes())?;
        append_u16_len(&mut message, activity.activity_id.as_bytes())?;
        append_u16_len(
            &mut message,
            activity.originating_search_tool_use_id.as_bytes(),
        )?;
        message.extend_from_slice(&activity.canonical_scope_digest);
        append_u16_len(&mut message, payload)?;
        Ok(message)
    }
}

impl Drop for HmacProgressiveSearchAuthority {
    fn drop(&mut self) {
        self.root.fill(0);
    }
}

impl fmt::Debug for HmacProgressiveSearchAuthority {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("HmacProgressiveSearchAuthority([redacted])")
    }
}

impl ProgressiveSearchAuthority for HmacProgressiveSearchAuthority {
    fn activity_id(&self, binding: &ReadExecutionBindingV2) -> Result<String, AgentError> {
        if binding.tool_use_id.is_empty() || binding.tool_use_id.len() > 128 {
            return Err(AgentError::ToolArgs("invalid tool-use binding".into()));
        }
        let mut message = Vec::new();
        append_u32_len(&mut message, binding.tool_use_id.as_bytes())?;
        let mac = self.hmac(b"isyncyou-progressive-search-activity/v1", &message);
        Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&mac[..16]))
    }

    fn seal_continuation(
        &self,
        activity: &SearchActivityBindingV1,
        state: &DeepContinuationStateV1,
    ) -> Result<String, AgentError> {
        validate_activity_state(activity, state)?;
        let wire = DeepContinuationWireV1 {
            v: PROGRESSIVE_SEARCH_WIRE_VERSION,
            a: state.activity_id.clone(),
            sc: encode_32(&state.canonical_scope_digest),
            si: state.service_index,
            so: state.service_offset,
            p: state.page,
            ms: state.metadata_scanned,
            br: state.body_reads_used,
            cd: encode_32(&state.candidate_page_digest),
            ps: state.issued_at_provider_step,
        };
        let payload = serde_json::to_vec(&wire)
            .map_err(|_| AgentError::Provider("continuation_encode_failed".into()))?;
        if payload.len() > MAX_CONTINUATION_PAYLOAD_BYTES {
            return Err(AgentError::Provider("continuation_too_large".into()));
        }
        let mac = self.hmac(
            b"isyncyou-progressive-search-continuation/v1",
            &Self::continuation_message(activity, &payload)?,
        );
        let encoded = format!(
            "{}.{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload),
            encode_32(&mac)
        );
        if encoded.len() > MAX_CONTINUATION_ASCII_BYTES {
            return Err(AgentError::Provider("continuation_too_large".into()));
        }
        Ok(encoded)
    }

    fn open_continuation(
        &self,
        activity: &SearchActivityBindingV1,
        encoded: &str,
    ) -> Result<DeepContinuationStateV1, AgentError> {
        if encoded.is_empty() || encoded.len() > MAX_CONTINUATION_ASCII_BYTES || !encoded.is_ascii()
        {
            return Err(AgentError::ToolArgs("invalid continuation".into()));
        }
        let mut parts = encoded.split('.');
        let payload_text = parts
            .next()
            .ok_or_else(|| AgentError::ToolArgs("invalid continuation".into()))?;
        let mac_text = parts
            .next()
            .ok_or_else(|| AgentError::ToolArgs("invalid continuation".into()))?;
        if parts.next().is_some() {
            return Err(AgentError::ToolArgs("invalid continuation".into()));
        }
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(payload_text)
            .map_err(|_| AgentError::ToolArgs("invalid continuation".into()))?;
        if payload.len() > MAX_CONTINUATION_PAYLOAD_BYTES {
            return Err(AgentError::ToolArgs("invalid continuation".into()));
        }
        let supplied_mac = decode_32(mac_text)?;
        let message = Self::continuation_message(activity, &payload)?;
        let expected = self.hmac(b"isyncyou-progressive-search-continuation/v1", &message);
        if hmac::verify(
            &hmac::Key::new(hmac::HMAC_SHA256, &self.root),
            &[
                b"isyncyou-progressive-search-continuation/v1".as_slice(),
                message.as_slice(),
            ]
            .concat(),
            &supplied_mac,
        )
        .is_err()
            || supplied_mac != expected
        {
            return Err(AgentError::ToolArgs("invalid continuation".into()));
        }
        let wire: DeepContinuationWireV1 = serde_json::from_slice(&payload)
            .map_err(|_| AgentError::ToolArgs("invalid continuation".into()))?;
        let canonical = serde_json::to_vec(&wire)
            .map_err(|_| AgentError::ToolArgs("invalid continuation".into()))?;
        if canonical != payload {
            return Err(AgentError::ToolArgs("invalid continuation".into()));
        }
        let state = DeepContinuationStateV1 {
            version: u32::from(wire.v),
            activity_id: wire.a,
            canonical_scope_digest: decode_32(&wire.sc)?,
            service_index: wire.si,
            service_offset: wire.so,
            page: wire.p,
            metadata_scanned: wire.ms,
            body_reads_used: wire.br,
            candidate_page_digest: decode_32(&wire.cd)?,
            issued_at_provider_step: wire.ps,
        };
        validate_activity_state(activity, &state)?;
        Ok(state)
    }

    fn candidate_key(
        &self,
        continuation: &DeepContinuationStateV1,
        service: &str,
        item_id: &str,
    ) -> Result<String, AgentError> {
        if service_ordinal(service).is_none() || item_id.is_empty() || item_id.len() > 512 {
            return Err(AgentError::Provider("candidate_binding_invalid".into()));
        }
        let mut message = Vec::new();
        append_u32_len(&mut message, continuation.activity_id.as_bytes())?;
        append_u32_len(&mut message, &continuation.canonical_scope_digest)?;
        message.extend_from_slice(&continuation.page.to_be_bytes());
        append_u32_len(&mut message, &continuation.candidate_page_digest)?;
        append_u32_len(&mut message, service.as_bytes())?;
        append_u32_len(&mut message, item_id.as_bytes())?;
        let mac = self.hmac(b"isyncyou-progressive-search-candidate/v1", &message);
        Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&mac[..16]))
    }
}

fn validate_activity_state(
    activity: &SearchActivityBindingV1,
    state: &DeepContinuationStateV1,
) -> Result<(), AgentError> {
    let page_metadata_scanned = state
        .metadata_scanned
        .checked_sub(state.service_offset)
        .filter(|scanned| *scanned <= MAX_METADATA_SCANNED_PER_CANDIDATE_PAGE);
    if state.version != 1
        || state.activity_id != activity.activity_id
        || state.canonical_scope_digest != activity.canonical_scope_digest
        || state.activity_id.len() != 22
        || !state
            .activity_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        || activity.originating_search_tool_use_id.is_empty()
        || activity.originating_search_tool_use_id.len() > 128
        || state.service_index as usize >= SERVICES.len()
        || state.metadata_scanned > MAX_METADATA_SCANNED_PER_ACTIVITY
        || page_metadata_scanned.is_none()
        || state.body_reads_used > 40
        || state.issued_at_provider_step > 15
    {
        return Err(AgentError::ToolArgs("continuation binding mismatch".into()));
    }
    Ok(())
}

pub fn candidate_page_digest(
    candidates: &[SearchCandidateMetadataV1],
    authoritative_ids: &[String],
) -> Result<[u8; 32], AgentError> {
    if candidates.len() != authoritative_ids.len() || candidates.len() > MAX_CANDIDATES_PER_PAGE {
        return Err(AgentError::Provider("candidate_page_invalid".into()));
    }
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"isyncyou-progressive-search-candidate-page/v1");
    bytes.extend_from_slice(&(candidates.len() as u32).to_be_bytes());
    for (candidate, item_id) in candidates.iter().zip(authoritative_ids) {
        bytes.push(
            service_ordinal(&candidate.service)
                .ok_or_else(|| AgentError::Provider("candidate_page_invalid".into()))?,
        );
        append_u32_len(&mut bytes, item_id.as_bytes())?;
        append_u32_len(&mut bytes, candidate.name.as_bytes())?;
        match &candidate.sender {
            Some(sender) => {
                bytes.push(1);
                append_u32_len(&mut bytes, sender.as_bytes())?;
            }
            None => bytes.push(0),
        }
        append_u32_len(&mut bytes, candidate.item_type.as_bytes())?;
        match &candidate.remote_mtime {
            Some(time) => {
                bytes.push(1);
                append_u32_len(&mut bytes, time.as_bytes())?;
            }
            None => bytes.push(0),
        }
        match candidate.size {
            Some(size) => {
                bytes.push(1);
                bytes.extend_from_slice(&size.to_be_bytes());
            }
            None => bytes.push(0),
        }
    }
    Ok(digest::digest(&digest::SHA256, &bytes)
        .as_ref()
        .try_into()
        .unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding(tool_use_id: &str) -> ReadExecutionBindingV2 {
        ReadExecutionBindingV2 {
            session_id: "session".into(),
            request_id: "request".into(),
            tool_use_id: tool_use_id.into(),
            resolved_account_key: "account".into(),
            admission_account_digest: [9; 32],
        }
    }

    fn activity(authority: &HmacProgressiveSearchAuthority) -> SearchActivityBindingV1 {
        let id = authority.activity_id(&binding("search-A")).unwrap();
        SearchActivityBindingV1 {
            session_id: "session".into(),
            request_id: "request".into(),
            activity_id: id,
            originating_search_tool_use_id: "search-A".into(),
            canonical_scope_digest: [3; 32],
        }
    }

    fn state(activity: &SearchActivityBindingV1) -> DeepContinuationStateV1 {
        DeepContinuationStateV1 {
            version: 1,
            activity_id: activity.activity_id.clone(),
            canonical_scope_digest: activity.canonical_scope_digest,
            service_index: 2,
            service_offset: 20,
            page: 1,
            metadata_scanned: 20,
            body_reads_used: 0,
            candidate_page_digest: [4; 32],
            issued_at_provider_step: 3,
        }
    }

    #[test]
    fn canonical_search_scope_is_byte_exact_across_restart_and_platform() {
        let first = CanonicalSearchScopeV1::new(
            "Account",
            "Ä  Query", // lang-allow: frozen Unicode canonicalization fixture
            vec!["mail".into()],
            None,
        )
        .unwrap();
        let second = CanonicalSearchScopeV1::new(
            "Account",
            "Ä  Query", // lang-allow: frozen Unicode canonicalization fixture
            vec!["mail".into()],
            Some(20),
        )
        .unwrap();
        assert_eq!(first.encoded(), second.encoded());
        assert_eq!(first.digest(), second.digest());
        assert_eq!(
            first
                .encoded()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>(),
            "0100074163636f756e740009c38420205175657279010000000014"
        );
    }

    #[test]
    fn canonical_search_scope_preserves_query_case_unicode_and_interior_whitespace() {
        let upper = CanonicalSearchScopeV1::new(
            "a",
            "Ä  Query", // lang-allow: Unicode preservation fixture
            vec![],
            None,
        )
        .unwrap();
        let lower = CanonicalSearchScopeV1::new(
            "a",
            "ä query", // lang-allow: Unicode preservation fixture
            vec![],
            None,
        )
        .unwrap();
        assert_ne!(upper.encoded(), lower.encoded());
        assert!(upper
            .encoded()
            .windows(9)
            .any(|part| part == "Ä  Query".as_bytes())); // lang-allow: Unicode preservation fixture
    }

    #[test]
    fn canonical_search_scope_defaults_dedupes_and_orders_services_once() {
        let defaults = CanonicalSearchScopeV1::new("a", "q", vec![], None).unwrap();
        assert_eq!(defaults.services(), SERVICES);
        let sorted =
            CanonicalSearchScopeV1::new("a", "q", vec!["onedrive".into(), "mail".into()], None)
                .unwrap();
        assert_eq!(sorted.services(), ["mail", "onedrive"]);
        let deduped =
            CanonicalSearchScopeV1::new("a", "q", vec!["mail".into(), "mail".into()], None)
                .unwrap();
        assert_eq!(deduped.services(), ["mail"]);
    }

    #[test]
    fn progressive_search_authority_is_zeroized_and_never_serialized_or_logged() {
        let authority = HmacProgressiveSearchAuthority::new([0x5a; 32]);
        let debug = format!("{authority:?}");

        assert_eq!(debug, "HmacProgressiveSearchAuthority([redacted])");
        assert!(!debug.contains("5a"));
        assert!(!debug.contains(&"90".repeat(32)));
    }

    #[test]
    fn search_tool_id_a_continuation_is_accepted_by_deep_search_tool_id_b() {
        let authority = HmacProgressiveSearchAuthority::new([7; 32]);
        let activity = activity(&authority);
        let encoded = authority
            .seal_continuation(&activity, &state(&activity))
            .unwrap();
        assert_eq!(
            authority
                .open_continuation(&activity, &encoded)
                .unwrap()
                .activity_id,
            authority.activity_id(&binding("search-A")).unwrap()
        );
        assert_ne!(
            activity.originating_search_tool_use_id,
            binding("deep-B").tool_use_id
        );
    }

    #[test]
    fn deep_search_continuation_rejects_wrong_originating_search_binding() {
        let authority = HmacProgressiveSearchAuthority::new([7; 32]);
        let activity = activity(&authority);
        let encoded = authority
            .seal_continuation(&activity, &state(&activity))
            .unwrap();
        let mut wrong = activity.clone();
        wrong.originating_search_tool_use_id = "search-other".into();
        assert!(authority.open_continuation(&wrong, &encoded).is_err());
    }

    #[test]
    fn deep_search_continuation_accepts_activity_budget_beyond_one_call_and_rejects_overflow() {
        let authority = HmacProgressiveSearchAuthority::new([7; 32]);
        let activity = activity(&authority);
        let mut continuation = state(&activity);
        continuation.service_offset =
            MAX_METADATA_SCANNED_PER_ACTIVITY - MAX_METADATA_SCANNED_PER_CANDIDATE_PAGE;
        continuation.metadata_scanned = MAX_METADATA_SCANNED_PER_ACTIVITY;
        let encoded = authority
            .seal_continuation(&activity, &continuation)
            .unwrap();
        assert_eq!(
            authority
                .open_continuation(&activity, &encoded)
                .unwrap()
                .metadata_scanned,
            MAX_METADATA_SCANNED_PER_ACTIVITY
        );

        continuation.metadata_scanned = MAX_METADATA_SCANNED_PER_ACTIVITY + 1;
        assert!(authority
            .seal_continuation(&activity, &continuation)
            .is_err());
    }

    #[test]
    fn deep_search_continuation_rejects_session_request_scope_and_mac_tampering() {
        let authority = HmacProgressiveSearchAuthority::new([7; 32]);
        let activity = activity(&authority);
        let encoded = authority
            .seal_continuation(&activity, &state(&activity))
            .unwrap();
        for wrong in [
            SearchActivityBindingV1 {
                session_id: "other-session".into(),
                ..activity.clone()
            },
            SearchActivityBindingV1 {
                request_id: "other-request".into(),
                ..activity.clone()
            },
            SearchActivityBindingV1 {
                canonical_scope_digest: [5; 32],
                ..activity.clone()
            },
        ] {
            assert!(authority.open_continuation(&wrong, &encoded).is_err());
        }

        let mut tampered = encoded.into_bytes();
        let last = tampered.last_mut().unwrap();
        *last = if *last == b'A' { b'B' } else { b'A' };
        assert!(authority
            .open_continuation(&activity, std::str::from_utf8(&tampered).unwrap())
            .is_err());
    }

    #[test]
    fn deep_search_candidate_page_digest_is_byte_exact_and_order_sensitive() {
        let candidate = |name: &str| SearchCandidateMetadataV1 {
            candidate_key: String::new(),
            service: "mail".into(),
            name: name.into(),
            sender: None,
            item_type: "message".into(),
            remote_mtime: None,
            size: None,
        };
        let ids = vec!["a".to_string(), "b".to_string()];
        let forward = candidate_page_digest(&[candidate("A"), candidate("B")], &ids).unwrap();
        let reverse = candidate_page_digest(&[candidate("B"), candidate("A")], &ids).unwrap();
        assert_ne!(forward, reverse);
    }

    #[test]
    fn deep_search_maximal_valid_wire_has_pinned_length_below_caps() {
        let authority = HmacProgressiveSearchAuthority::new([7; 32]);
        let activity = activity(&authority);
        let encoded = authority
            .seal_continuation(&activity, &state(&activity))
            .unwrap();
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(encoded.split('.').next().unwrap())
            .unwrap();
        assert!(payload.len() <= MAX_CONTINUATION_PAYLOAD_BYTES);
        assert!(encoded.len() <= MAX_CONTINUATION_ASCII_BYTES);
    }

    #[test]
    fn deep_search_continuation_encoded_1024_reaches_decode_and_1025_rejects_predecode() {
        let authority = HmacProgressiveSearchAuthority::new([7; 32]);
        let activity = activity(&authority);
        assert!(authority
            .open_continuation(&activity, &"a".repeat(1_025))
            .is_err());
    }

    #[test]
    fn deep_search_continuation_decoded_640_reaches_authenticated_parse_and_641_rejects() {
        let authority = HmacProgressiveSearchAuthority::new([7; 32]);
        let activity = activity(&authority);
        for payload_len in [
            MAX_CONTINUATION_PAYLOAD_BYTES,
            MAX_CONTINUATION_PAYLOAD_BYTES + 1,
        ] {
            let payload = vec![b'x'; payload_len];
            let mac = authority.hmac(
                b"isyncyou-progressive-search-continuation/v1",
                &HmacProgressiveSearchAuthority::continuation_message(&activity, &payload).unwrap(),
            );
            let encoded = format!(
                "{}.{}",
                base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload),
                encode_32(&mac)
            );
            assert!(authority.open_continuation(&activity, &encoded).is_err());
        }
    }

    #[test]
    fn candidate_key_rejects_unknown_service_empty_id_and_oversized_id() {
        let authority = HmacProgressiveSearchAuthority::new([7; 32]);
        let activity = activity(&authority);
        let continuation = state(&activity);
        assert!(authority
            .candidate_key(&continuation, "unknown", "id")
            .is_err());
        assert!(authority.candidate_key(&continuation, "mail", "").is_err());
        assert!(authority
            .candidate_key(&continuation, "mail", &"x".repeat(513))
            .is_err());
    }
}
