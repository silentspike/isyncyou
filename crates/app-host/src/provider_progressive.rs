//! Account- and turn-bound private state for self-hosted progressive search.
use isyncyou_agent::{
    AgentError, DeepContinuationStateV1, ProgressiveSearchAuthority, ReadExecutionBindingV2,
    SearchActivityBindingV1, SearchCandidateMetadataV1,
};
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

const MAX_ACTIVITIES: usize = 4;
const MAX_SELECTED: usize = 12;
const MAX_CANDIDATES: usize = 64;
const TTL: Duration = Duration::from_secs(300);

#[derive(Clone)]
pub(super) struct Candidate<T> {
    pub service: String,
    pub id: String,
    pub name: String,
    pub item_type: String,
    pub native: T,
}

struct Activity<T> {
    binding: SearchActivityBindingV1,
    account: String,
    account_digest: [u8; 32],
    grant: String,
    origin: String,
    expires: Instant,
    state: DeepContinuationStateV1,
    candidates: BTreeMap<String, Candidate<T>>,
    consumed: bool,
}

pub(super) struct ProviderProgressiveState<T> {
    activities: BTreeMap<String, Activity<T>>,
}

impl<T> Default for ProviderProgressiveState<T> {
    fn default() -> Self {
        Self {
            activities: BTreeMap::new(),
        }
    }
}

impl<T: Clone> ProviderProgressiveState<T> {
    pub fn prune_expired(&mut self) {
        let now = Instant::now();
        self.activities.retain(|_, value| value.expires > now);
    }
    pub fn contains(&self, id: &str) -> bool {
        self.activities.contains_key(id)
    }
    pub fn remove(&mut self, id: &str) {
        self.activities.remove(id);
    }
    pub fn issue(
        &mut self,
        authority: &dyn ProgressiveSearchAuthority,
        execution: &ReadExecutionBindingV2,
        origin: &str,
        grant: &str,
        scope_digest: [u8; 32],
        items: Vec<Candidate<T>>,
        step: u8,
    ) -> Result<(String, String, Vec<SearchCandidateMetadataV1>), AgentError> {
        self.prune_expired();
        if self.activities.len() >= MAX_ACTIVITIES
            || items.len() > MAX_CANDIDATES
            || grant.is_empty()
            || origin.is_empty()
            || execution.admission_account_digest
                != isyncyou_agent::admission_account_digest(&execution.resolved_account_key)?
        {
            return Err(AgentError::Provider(
                "provider_search_budget_exhausted".into(),
            ));
        }
        let activity_id = authority.activity_id(execution)?;
        if self.activities.contains_key(&activity_id) {
            return Err(AgentError::Provider(
                "search_activity_binding_conflict".into(),
            ));
        }
        let binding = SearchActivityBindingV1 {
            session_id: execution.session_id.clone(),
            request_id: execution.request_id.clone(),
            activity_id: activity_id.clone(),
            originating_search_tool_use_id: execution.tool_use_id.clone(),
            canonical_scope_digest: scope_digest,
        };
        let mut metadata: Vec<_> = items
            .iter()
            .map(|item| SearchCandidateMetadataV1 {
                candidate_key: String::new(),
                service: item.service.clone(),
                name: item.name.clone(),
                sender: None,
                item_type: item.item_type.clone(),
                remote_mtime: None,
                size: None,
            })
            .collect();
        let ids: Vec<_> = items.iter().map(|item| item.id.clone()).collect();
        let digest = isyncyou_agent::candidate_page_digest(&metadata, &ids)?;
        let state = DeepContinuationStateV1 {
            version: 1,
            activity_id: activity_id.clone(),
            canonical_scope_digest: scope_digest,
            service_index: 0,
            service_offset: 0,
            page: 0,
            metadata_scanned: u32::try_from(metadata.len())
                .map_err(|_| AgentError::Provider("candidate_page_invalid".into()))?,
            body_reads_used: 0,
            candidate_page_digest: digest,
            issued_at_provider_step: step,
        };
        let mut candidates = BTreeMap::new();
        for (item, meta) in items.into_iter().zip(metadata.iter_mut()) {
            let key = authority.candidate_key(&state, &item.service, &item.id)?;
            if candidates.insert(key.clone(), item).is_some() {
                return Err(AgentError::Provider("candidate_binding_invalid".into()));
            }
            meta.candidate_key = key;
        }
        let continuation = authority.seal_continuation(&binding, &state)?;
        self.activities.insert(
            activity_id.clone(),
            Activity {
                binding,
                account: execution.resolved_account_key.clone(),
                account_digest: execution.admission_account_digest,
                grant: grant.into(),
                origin: origin.into(),
                expires: Instant::now() + TTL,
                state,
                candidates,
                consumed: false,
            },
        );
        Ok((activity_id, continuation, metadata))
    }

    pub fn consume(
        &mut self,
        authority: &dyn ProgressiveSearchAuthority,
        execution: &ReadExecutionBindingV2,
        origin: &str,
        grant: &str,
        activity_id: &str,
        continuation: &str,
        selected: &[String],
        provider_step: u8,
    ) -> Result<Vec<Candidate<T>>, AgentError> {
        if selected.len() > MAX_SELECTED
            || selected.iter().collect::<BTreeSet<_>>().len() != selected.len()
        {
            return Err(AgentError::ToolArgs(
                "invalid deep-search candidates".into(),
            ));
        }
        let activity = self
            .activities
            .get_mut(activity_id)
            .ok_or_else(|| AgentError::ToolArgs("unknown search activity".into()))?;
        if activity.binding.session_id != execution.session_id
            || activity.binding.request_id != execution.request_id
            || activity.account != execution.resolved_account_key
            || activity.account_digest != execution.admission_account_digest
            || activity.origin != origin
            || activity.grant != grant
            || Instant::now() >= activity.expires
            || activity.consumed
            || provider_step <= activity.state.issued_at_provider_step
        {
            return Err(AgentError::ToolArgs(
                "search activity binding mismatch".into(),
            ));
        }
        if authority.open_continuation(&activity.binding, continuation)? != activity.state {
            return Err(AgentError::ToolArgs("continuation binding mismatch".into()));
        }
        let result =
            selected
                .iter()
                .map(|key| {
                    activity.candidates.get(key).cloned().ok_or_else(|| {
                        AgentError::ToolArgs("invalid deep-search candidates".into())
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
        activity.consumed = true;
        activity.state.body_reads_used = activity
            .state
            .body_reads_used
            .saturating_add(result.len() as u16);
        Ok(result)
    }

    pub fn next_page(
        &mut self,
        authority: &dyn ProgressiveSearchAuthority,
        execution: &ReadExecutionBindingV2,
        origin: &str,
        grant: &str,
        activity_id: &str,
        items: Vec<Candidate<T>>,
        step: u8,
    ) -> Result<(String, Vec<SearchCandidateMetadataV1>), AgentError> {
        let activity = self
            .activities
            .get_mut(activity_id)
            .ok_or_else(|| AgentError::ToolArgs("unknown search activity".into()))?;
        if !activity.consumed
            || activity.binding.session_id != execution.session_id
            || activity.binding.request_id != execution.request_id
            || activity.account != execution.resolved_account_key
            || activity.account_digest != execution.admission_account_digest
            || activity.origin != origin
            || activity.grant != grant
            || Instant::now() >= activity.expires
            || step <= activity.state.issued_at_provider_step
            || activity.state.body_reads_used >= 40
            || items.len() > MAX_CANDIDATES
        {
            return Err(AgentError::ToolArgs(
                "search activity binding mismatch".into(),
            ));
        }
        let mut metadata: Vec<_> = items
            .iter()
            .map(|item| SearchCandidateMetadataV1 {
                candidate_key: String::new(),
                service: item.service.clone(),
                name: item.name.clone(),
                sender: None,
                item_type: item.item_type.clone(),
                remote_mtime: None,
                size: None,
            })
            .collect();
        let ids: Vec<_> = items.iter().map(|item| item.id.clone()).collect();
        let mut next = activity.state.clone();
        next.page = next
            .page
            .checked_add(1)
            .ok_or_else(|| AgentError::Provider("provider_search_budget_exhausted".into()))?;
        next.service_offset = next.metadata_scanned;
        next.metadata_scanned = next
            .metadata_scanned
            .checked_add(metadata.len() as u32)
            .ok_or_else(|| AgentError::Provider("provider_search_budget_exhausted".into()))?;
        next.candidate_page_digest = isyncyou_agent::candidate_page_digest(&metadata, &ids)?;
        next.issued_at_provider_step = step;
        let mut candidates = BTreeMap::new();
        for (item, meta) in items.into_iter().zip(metadata.iter_mut()) {
            let key = authority.candidate_key(&next, &item.service, &item.id)?;
            if candidates.insert(key.clone(), item).is_some() {
                return Err(AgentError::Provider("candidate_binding_invalid".into()));
            }
            meta.candidate_key = key;
        }
        let continuation = authority.seal_continuation(&activity.binding, &next)?;
        activity.state = next;
        activity.candidates = candidates;
        activity.consumed = false;
        Ok((continuation, metadata))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn binding() -> ReadExecutionBindingV2 {
        ReadExecutionBindingV2 {
            session_id: "session".into(),
            request_id: "request".into(),
            tool_use_id: "search".into(),
            resolved_account_key: "account".into(),
            admission_account_digest: isyncyou_agent::admission_account_digest("account").unwrap(),
        }
    }
    fn candidate() -> Candidate<()> {
        Candidate {
            service: "mail".into(),
            id: "id".into(),
            name: "name".into(),
            item_type: "message".into(),
            native: (),
        }
    }
    #[test]
    fn continuation_rejects_changed_grant_turn_account_and_tampering() {
        let authority = isyncyou_agent::HmacProgressiveSearchAuthority::new([7; 32]);
        let mut state = ProviderProgressiveState::default();
        let original = binding();
        let (id, token, rows) = state
            .issue(
                &authority,
                &original,
                "https://example.test",
                "grant",
                [9; 32],
                vec![candidate()],
                0,
            )
            .unwrap();
        let chosen = vec![rows[0].candidate_key.clone()];
        assert!(state
            .consume(
                &authority,
                &original,
                "https://example.test",
                "other",
                &id,
                &token,
                &chosen,
                1,
            )
            .is_err());
        assert!(state
            .consume(
                &authority,
                &original,
                "https://other.test",
                "grant",
                &id,
                &token,
                &chosen,
                1
            )
            .is_err());
        let other_authority = isyncyou_agent::HmacProgressiveSearchAuthority::new([6; 32]);
        assert!(state
            .consume(
                &other_authority,
                &original,
                "https://example.test",
                "grant",
                &id,
                &token,
                &chosen,
                1
            )
            .is_err());
        let mut wrong = original.clone();
        wrong.session_id = "other".into();
        assert!(state
            .consume(
                &authority,
                &wrong,
                "https://example.test",
                "grant",
                &id,
                &token,
                &chosen,
                1
            )
            .is_err());
        wrong = original.clone();
        wrong.request_id = "other".into();
        assert!(state
            .consume(
                &authority,
                &wrong,
                "https://example.test",
                "grant",
                &id,
                &token,
                &chosen,
                1,
            )
            .is_err());
        wrong = original.clone();
        wrong.resolved_account_key = "other".into();
        assert!(state
            .consume(
                &authority,
                &wrong,
                "https://example.test",
                "grant",
                &id,
                &token,
                &chosen,
                1,
            )
            .is_err());
        assert!(state
            .consume(
                &authority,
                &original,
                "https://example.test",
                "grant",
                &id,
                &format!("{token}x"),
                &chosen,
                1,
            )
            .is_err());
        assert!(state
            .consume(
                &authority,
                &original,
                "https://example.test",
                "grant",
                &id,
                &token,
                &["unknown".into()],
                1,
            )
            .is_err());
        state.activities.get_mut(&id).unwrap().expires = Instant::now() - Duration::from_secs(1);
        assert!(state
            .consume(
                &authority,
                &original,
                "https://example.test",
                "grant",
                &id,
                &token,
                &chosen,
                1,
            )
            .is_err());
        state.activities.get_mut(&id).unwrap().expires = Instant::now() + Duration::from_secs(1);
        assert_eq!(
            state
                .consume(
                    &authority,
                    &original,
                    "https://example.test",
                    "grant",
                    &id,
                    &token,
                    &chosen,
                    1,
                )
                .unwrap()
                .len(),
            1
        );
        assert!(state
            .consume(
                &authority,
                &original,
                "https://example.test",
                "grant",
                &id,
                &token,
                &chosen,
                1,
            )
            .is_err());
    }

    #[test]
    fn next_page_uses_new_signed_candidate_and_cannot_replay_prior_page() {
        let authority = isyncyou_agent::HmacProgressiveSearchAuthority::new([8; 32]);
        let mut state = ProviderProgressiveState::default();
        let execution = binding();
        let (id, old, first) = state
            .issue(
                &authority,
                &execution,
                "https://example.test",
                "grant",
                [4; 32],
                vec![candidate()],
                0,
            )
            .unwrap();
        state
            .consume(
                &authority,
                &execution,
                "https://example.test",
                "grant",
                &id,
                &old,
                &[first[0].candidate_key.clone()],
                1,
            )
            .unwrap();
        let (next, second) = state
            .next_page(
                &authority,
                &execution,
                "https://example.test",
                "grant",
                &id,
                vec![Candidate {
                    id: "second".into(),
                    ..candidate()
                }],
                1,
            )
            .unwrap();
        assert!(state
            .consume(
                &authority,
                &execution,
                "https://example.test",
                "grant",
                &id,
                &old,
                &[first[0].candidate_key.clone()],
                2
            )
            .is_err());
        assert_eq!(
            state
                .consume(
                    &authority,
                    &execution,
                    "https://example.test",
                    "grant",
                    &id,
                    &next,
                    &[second[0].candidate_key.clone()],
                    2
                )
                .unwrap()
                .len(),
            1
        );
    }
    #[test]
    fn expired_and_terminal_activities_free_capacity_without_removing_live() {
        let authority = isyncyou_agent::HmacProgressiveSearchAuthority::new([6; 32]);
        let mut state = ProviderProgressiveState::default();
        let mut ids = Vec::new();
        for n in 0..4 {
            let mut b = binding();
            b.tool_use_id = format!("search-{n}");
            ids.push(
                state
                    .issue(
                        &authority,
                        &b,
                        "https://example.test",
                        "grant",
                        [1; 32],
                        vec![candidate()],
                        0,
                    )
                    .unwrap()
                    .0,
            );
        }
        state.activities.get_mut(&ids[0]).unwrap().expires =
            Instant::now() - Duration::from_secs(1);
        state.remove(&ids[1]);
        state.prune_expired();
        assert!(!state.contains(&ids[0]) && !state.contains(&ids[1]));
        assert!(state.contains(&ids[2]) && state.contains(&ids[3]));
        let mut fifth = binding();
        fifth.tool_use_id = "search-fifth".into();
        assert!(state
            .issue(
                &authority,
                &fifth,
                "https://example.test",
                "grant",
                [1; 32],
                vec![candidate()],
                0
            )
            .is_ok());
    }
}
