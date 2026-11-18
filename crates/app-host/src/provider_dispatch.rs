//! Private, grant-bound state for self-hosted progressive search.
use super::provider_progressive::{Candidate, ProviderProgressiveState};
use isyncyou_agent::{
    AgentError, ReadExecutionContext, ReadExecutionOutputV2, SeparatedSearchOutputV2, ToolAction,
};
use isyncyou_engine::provider_search::{
    self as search, SearchBudget, SearchCandidate, SearchCursor, SearchMode, SearchScope,
};
use std::{
    collections::BTreeMap,
    ops::{Deref, DerefMut},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

struct Scan {
    scopes: Vec<SearchScope>,
    index: usize,
    cursor: Option<SearchCursor>,
    remaining: (usize, usize, usize),
    scanned: usize,
}
pub(super) struct Dispatch {
    signed: ProviderProgressiveState<SearchCandidate>,
    scans: BTreeMap<String, Scan>,
    turn_budget: Arc<Mutex<(usize, usize, usize)>>,
}
impl Default for Dispatch {
    fn default() -> Self {
        Self {
            signed: ProviderProgressiveState::default(),
            scans: BTreeMap::new(),
            turn_budget: Arc::new(Mutex::new((128, 8 * 1024 * 1024, 40))),
        }
    }
}
struct TrackedBudget {
    budget: SearchBudget,
    turn: Arc<Mutex<(usize, usize, usize)>>,
}
impl Deref for TrackedBudget {
    type Target = SearchBudget;
    fn deref(&self) -> &SearchBudget {
        &self.budget
    }
}
impl DerefMut for TrackedBudget {
    fn deref_mut(&mut self) -> &mut SearchBudget {
        &mut self.budget
    }
}
impl Drop for TrackedBudget {
    fn drop(&mut self) {
        let left = self.budget.remaining_limits();
        let mut turn = self.turn.lock().unwrap_or_else(|e| e.into_inner());
        *turn = (turn.0.min(left.0), turn.1.min(left.1), turn.2.min(left.2));
    }
}

fn failure(e: search::SearchError) -> String {
    match e {
        search::SearchError::Cancelled => "cancelled",
        search::SearchError::Exhausted => "search_budget_exhausted",
        search::SearchError::Changed => "search_authority_changed",
        _ => "search_unavailable",
    }
    .into()
}
fn budget(
    context: &ReadExecutionContext<'_, '_>,
    remaining: (usize, usize, usize),
    deadline: Instant,
    turn: Arc<Mutex<(usize, usize, usize)>>,
) -> TrackedBudget {
    let cancellation = (*context.cancellation).clone();
    let total = *turn.lock().unwrap_or_else(|e| e.into_inner());
    TrackedBudget {
        budget: SearchBudget::new(
            deadline,
            Arc::new(move || cancellation.is_cancelled()),
            remaining.0.min(total.0),
            remaining.1.min(total.1),
            remaining.2.min(total.2),
        ),
        turn,
    }
}
fn service(c: &SearchCandidate) -> String {
    match c.scope() {
        SearchScope::Mail { .. } => "mail".into(),
        SearchScope::Dav { service, .. } => service.clone(),
    }
}
fn candidate(c: SearchCandidate) -> Candidate<SearchCandidate> {
    // Scope is included in the identity so equal native IDs in distinct accounts or
    // DAV collections cannot alias. Native IDs remain private in memory.
    let parts: Vec<&str> = match c.scope() {
        SearchScope::Mail {
            native_account,
            mailbox_id,
            ..
        } => vec![
            "mail",
            native_account.as_deref().unwrap_or(""),
            mailbox_id.as_deref().unwrap_or(""),
            c.id(),
        ],
        SearchScope::Dav {
            service,
            collection,
            ..
        } => vec!["dav", service, collection, c.id()],
    };
    let mut bytes = b"isyncyou-provider-candidate/v1".to_vec();
    for part in parts {
        bytes.extend_from_slice(&(part.len() as u64).to_be_bytes());
        bytes.extend_from_slice(part.as_bytes());
    }
    let id = ring::digest::digest(&ring::digest::SHA256, &bytes)
        .as_ref()
        .iter()
        .map(|v| format!("{v:02x}"))
        .collect();
    Candidate {
        service: service(&c),
        id,
        name: c.name().to_owned(),
        item_type: c.item_type().to_owned(),
        native: c,
    }
}
fn next(
    cfg: &isyncyou_core::Config,
    account: &str,
    token: &str,
    scan: &mut Scan,
    b: &mut SearchBudget,
) -> Result<Vec<Candidate<SearchCandidate>>, String> {
    while scan.index < scan.scopes.len() {
        let page = search::page(
            cfg,
            account,
            token,
            &scan.scopes[scan.index],
            scan.cursor.as_ref(),
            12,
            b,
        )
        .map_err(failure)?;
        scan.cursor = page.next_cursor;
        if scan.cursor.is_none() {
            scan.index += 1;
        }
        scan.scanned += page.items.len();
        if !page.items.is_empty() {
            return Ok(page.items.into_iter().map(candidate).collect());
        }
    }
    Ok(Vec::new())
}
fn output(
    content: String,
    activity: String,
    count: usize,
    continuation: bool,
    deep: bool,
    reached: bool,
    context: &mut ReadExecutionContext<'_, '_>,
) -> Result<ReadExecutionOutputV2, AgentError> {
    if context.cancellation.is_cancelled() {
        return Err(AgentError::Cancelled);
    }
    if content.len() > if deep { 64 * 1024 } else { 192 * 1024 } {
        return Err(AgentError::Provider(
            "provider_search_budget_exhausted".into(),
        ));
    }
    context.input_budget.charge(&content)?;
    if context.mode == isyncyou_agent::ReadExecutionMode::Live {
        use isyncyou_agent::activity::{
            ActivityKind, SearchStage, StageProgressV1, StageStatus, ACTIVITY_SCHEMA_VERSION,
        };
        let mut emit =
            |stage, status, scanned, hits, complete, reached, more| -> Result<(), AgentError> {
                let event = StageProgressV1 {
                    schema_version: ACTIVITY_SCHEMA_VERSION,
                    activity_id: activity.clone(),
                    activity_kind: ActivityKind::ProviderSearch,
                    stage,
                    status,
                    scanned,
                    total: None,
                    hits,
                    current_item: None,
                    coverage_complete: complete,
                    budget_reached: reached,
                    continuation_available: more,
                };
                event
                    .validate()
                    .map_err(|_| AgentError::Provider("search event unavailable".into()))?;
                context
                    .events
                    .emit(isyncyou_agent::StreamEvent::StageProgress(event))
            };
        if !deep {
            emit(
                SearchStage::Names,
                StageStatus::Complete,
                count.min(1_000_000) as u32,
                count.min(1_000_000) as u32,
                Some(false),
                Some(reached),
                Some(continuation),
            )?;
            emit(
                SearchStage::Bodies,
                StageStatus::Queued,
                0,
                0,
                None,
                None,
                None,
            )?;
            emit(
                SearchStage::Bodies,
                StageStatus::Running,
                0,
                0,
                None,
                None,
                None,
            )?;
            emit(
                SearchStage::Bodies,
                StageStatus::Complete,
                0,
                0,
                Some(false),
                Some(reached),
                Some(continuation),
            )?;
            emit(
                SearchStage::Deep,
                StageStatus::Queued,
                0,
                0,
                None,
                None,
                None,
            )?;
            if continuation {
                emit(
                    SearchStage::Deep,
                    StageStatus::Running,
                    0,
                    0,
                    None,
                    None,
                    None,
                )?;
            } else {
                emit(
                    SearchStage::Deep,
                    StageStatus::Skipped,
                    0,
                    0,
                    Some(false),
                    Some(reached),
                    Some(false),
                )?;
            }
        } else if continuation {
            emit(
                SearchStage::Deep,
                StageStatus::Running,
                0,
                0,
                None,
                None,
                None,
            )?;
        } else {
            emit(
                SearchStage::Deep,
                StageStatus::Complete,
                count.min(1_000_000) as u32,
                count.min(1_000_000) as u32,
                Some(false),
                Some(reached),
                Some(false),
            )?;
        }
    }
    let parsed: serde_json::Value = serde_json::from_str(&content)
        .map_err(|_| AgentError::Provider("provider search unavailable".into()))?;
    let sources = parsed
        .get(if deep { "selected" } else { "results" })
        .and_then(serde_json::Value::as_array)
        .map(|rows| super::provider_mail_sources(rows))
        .unwrap_or_default();
    let public_sources = sources.iter().take(3).cloned().collect();
    let out = SeparatedSearchOutputV2 {
        provider_content: content,
        public_projection: isyncyou_agent::PublicToolResultV1 {
            schema_version: isyncyou_agent::activity::ACTIVITY_SCHEMA_VERSION,
            operation: if deep { "deep-search" } else { "search" }.into(),
            activity_id: activity,
            visible_hits: count as u32,
            coverage_complete: false,
            budget_reached: reached,
            continuation_available: continuation,
            sources: public_sources,
        },
        assistant_sources: sources,
    };
    Ok(if deep {
        ReadExecutionOutputV2::DeepSearch(out)
    } else {
        ReadExecutionOutputV2::Search(out)
    })
}
impl super::ProviderMailExecutor {
    pub(super) fn progressive_read(
        &self,
        action: &ToolAction,
        mut context: ReadExecutionContext<'_, '_>,
    ) -> Result<ReadExecutionOutputV2, AgentError> {
        let deadline = Instant::now() + Duration::from_secs(10);
        if context.binding.resolved_account_key != self.account
            || context.binding.admission_account_digest
                != isyncyou_agent::admission_account_digest(&self.account)?
        {
            return Err(AgentError::ToolArgs("account mismatch".into()));
        }
        if matches!(action, ToolAction::Search { account, .. } if account != &self.account) {
            return Err(AgentError::ToolArgs("account mismatch".into()));
        }
        if context.cancellation.is_cancelled() {
            return Err(AgentError::Cancelled);
        }
        let authority = context
            .progressive_authority
            .ok_or_else(|| AgentError::Provider("search authority unavailable".into()))?;
        let search_id = if matches!(action, ToolAction::Search { .. })
            && context.mode == isyncyou_agent::ReadExecutionMode::Live
        {
            let id = authority.activity_id(context.binding)?;
            for status in [
                isyncyou_agent::activity::StageStatus::Queued,
                isyncyou_agent::activity::StageStatus::Running,
            ] {
                let event = isyncyou_agent::activity::StageProgressV1 {
                    schema_version: isyncyou_agent::activity::ACTIVITY_SCHEMA_VERSION,
                    activity_id: id.clone(),
                    activity_kind: isyncyou_agent::activity::ActivityKind::ProviderSearch,
                    stage: isyncyou_agent::activity::SearchStage::Names,
                    status,
                    scanned: 0,
                    total: None,
                    hits: 0,
                    current_item: None,
                    coverage_complete: None,
                    budget_reached: None,
                    continuation_available: None,
                };
                event
                    .validate()
                    .map_err(|_| AgentError::Provider("search event unavailable".into()))?;
                context
                    .events
                    .emit(isyncyou_agent::StreamEvent::StageProgress(event))?;
            }
            Some(id)
        } else {
            None
        };
        let origin = match self.cfg.provider_accounts.get(&self.account) {
            Some(isyncyou_core::config::ProviderConfig::SelfHosted { origin, .. }) => {
                origin.clone()
            }
            _ => return Err(AgentError::Provider("provider unavailable".into())),
        };
        // Open before the credential lock: the key lookup itself can take that lock.
        let needs_index = matches!(action, ToolAction::Search { services, .. } if services.is_empty() || services.iter().any(|s| matches!(s.as_str(), "calendar" | "todo" | "contacts")));
        let mut state = self
            .progressive
            .lock()
            .map_err(|_| AgentError::Provider("search state unavailable".into()))?;
        state.signed.prune_expired();
        let Dispatch { signed, scans, .. } = &mut *state;
        scans.retain(|id, _| signed.contains(id));
        let remaining = match action {
            ToolAction::DeepSearch { activity_id, .. } => state
                .scans
                .get(activity_id)
                .map(|s| s.remaining)
                .unwrap_or((0, 0, 0)),
            _ => (128, 8 * 1024 * 1024, 40),
        };
        let mut b = budget(&context, remaining, deadline, state.turn_budget.clone());
        let mut store = if needs_index {
            isyncyou_connectors::request_budget::with_search_budget(&b, || {
                isyncyou_engine::provider_store::open_readonly(&self.cfg, &self.account)
            })
            .ok()
        } else {
            None
        };
        let mut consumed = false;
        let result = isyncyou_connectors::request_budget::with_search_budget(&(*b).clone(), || {
            super::with_current_recovery_config(&self.config_path, &self.cfg, |cfg| {
                b.remaining().map_err(|_| "search_budget_exhausted")?;
                if cfg.provider_accounts.get(&self.account)
                    != self.cfg.provider_accounts.get(&self.account)
                    || !cfg.accounts.iter().any(|a| a.id == self.account)
                {
                    return Err("account changed".into());
                }
                isyncyou_engine::provider_auth::with_locked_provider_current_token(
                    cfg,
                    &self.account,
                    |subject, generation, token| {
                        b.remaining().map_err(|_| "search_budget_exhausted")?;
                        let grant = format!("{}:{}{}", subject.len(), subject, generation);
                        match action {
                            ToolAction::Search {
                                account,
                                services,
                                query,
                                limit,
                            } => {
                                if account != &self.account || query.trim().is_empty() {
                                    return Err("invalid search".into());
                                }
                                let requested = if services.is_empty() {
                                    vec![
                                        "mail".into(),
                                        "calendar".into(),
                                        "todo".into(),
                                        "contacts".into(),
                                    ]
                                } else {
                                    services.clone()
                                };
                                let services: Vec<String> = requested
                                    .iter()
                                    .filter(|s| {
                                        matches!(
                                            s.as_str(),
                                            "mail" | "calendar" | "todo" | "contacts"
                                        )
                                    })
                                    .cloned()
                                    .collect();
                                let unsupported: Vec<String> = requested
                                    .iter()
                                    .filter(|s| {
                                        !matches!(
                                            s.as_str(),
                                            "mail" | "calendar" | "todo" | "contacts"
                                        )
                                    })
                                    .cloned()
                                    .collect();
                                if services.is_empty() {
                                    return Err("no supported search service".into());
                                }
                                let mut scopes = Vec::new();
                                let mut unavailable_services = Vec::new();
                                let mut exhausted = false;
                                for group in
                                    [&["mail"][..], &["calendar", "todo"][..], &["contacts"][..]]
                                {
                                    let selected: Vec<String> = services
                                        .iter()
                                        .filter(|service| group.contains(&service.as_str()))
                                        .cloned()
                                        .collect();
                                    if selected.is_empty() {
                                        continue;
                                    }
                                    match search::scopes(
                                        cfg,
                                        &self.account,
                                        token,
                                        &selected,
                                        query,
                                        SearchMode::Keyword,
                                        &mut b,
                                    ) {
                                        Ok(mut found) => scopes.append(&mut found),
                                        Err(search::SearchError::Exhausted) => {
                                            exhausted = true;
                                            break;
                                        }
                                        Err(search::SearchError::Unavailable) => {
                                            unavailable_services.extend(selected)
                                        }
                                        Err(other) => return Err(failure(other)),
                                    }
                                }
                                let mut results = Vec::new();
                                let mut index_unavailable = false;
                                let mut failed_scopes = Vec::new();
                                let max = limit.unwrap_or(20).clamp(1, 100) as usize;
                                for scope in &scopes {
                                    if results.len() >= max || exhausted {
                                        break;
                                    }
                                    if matches!(scope, SearchScope::Dav { .. }) && store.is_none() {
                                        index_unavailable = true;
                                        continue;
                                    }
                                    let scope_start = results.len();
                                    let mut cursor = None;
                                    loop {
                                        let page = match scope {
                                            SearchScope::Dav { .. } => search::indexed_keyword(
                                                cfg,
                                                &self.account,
                                                token,
                                                scope,
                                                store
                                                    .as_mut()
                                                    .ok_or("provider cache unavailable")?,
                                                (max - results.len()).min(12),
                                                &mut b,
                                            ),
                                            _ => search::page(
                                                cfg,
                                                &self.account,
                                                token,
                                                scope,
                                                cursor.as_ref(),
                                                (max - results.len()).min(12),
                                                &mut b,
                                            ),
                                        };
                                        let page = match page {
                                            Ok(page) => page,
                                            Err(search::SearchError::Exhausted) => {
                                                exhausted = true;
                                                break;
                                            }
                                            Err(search::SearchError::Unavailable) => {
                                                results.truncate(scope_start);
                                                failed_scopes.push(scope.clone());
                                                match scope {
                                                    SearchScope::Mail { .. } => {
                                                        unavailable_services.push("mail".into())
                                                    }
                                                    SearchScope::Dav { service, .. }
                                                        if service == "calendar" =>
                                                    {
                                                        unavailable_services.extend(
                                                            services
                                                                .iter()
                                                                .filter(|s| {
                                                                    matches!(
                                                                        s.as_str(),
                                                                        "calendar" | "todo"
                                                                    )
                                                                })
                                                                .cloned(),
                                                        )
                                                    }
                                                    SearchScope::Dav { service, .. } => {
                                                        unavailable_services.push(service.clone())
                                                    }
                                                }
                                                break;
                                            }
                                            Err(e) => return Err(failure(e)),
                                        };
                                        cursor = page.next_cursor;
                                        for row in page.items {
                                            let (id, collection) = match row.scope() {
                                                SearchScope::Mail {
                                                    native_account: Some(native),
                                                    ..
                                                } => (
                                                    super::shared_mail_ref(native, row.id()),
                                                    Some(native.clone()),
                                                ),
                                                SearchScope::Mail { .. } => {
                                                    (row.id().to_owned(), None)
                                                }
                                                SearchScope::Dav { collection, .. } => {
                                                    (row.id().to_owned(), Some(collection.clone()))
                                                }
                                            };
                                            results.push(serde_json::json!({"account":self.account,"provider":origin,"service":service(&row),"collection":collection,"id":id,"name":row.name(),"item_type":row.item_type(),"provenance":row.provenance()}));
                                        }
                                        if cursor.is_none() || results.len() >= max {
                                            break;
                                        }
                                    }
                                }
                                let deep_scopes = if exhausted {
                                    None
                                } else {
                                    Some(
                                        scopes
                                            .into_iter()
                                            .filter(|scope| !failed_scopes.contains(scope))
                                            .map(|scope| match scope {
                                                SearchScope::Mail {
                                                    origin,
                                                    native_account,
                                                    mailbox_id,
                                                    query,
                                                    ..
                                                } => SearchScope::Mail {
                                                    origin,
                                                    native_account,
                                                    mailbox_id,
                                                    query,
                                                    mode: SearchMode::CandidateMetadata,
                                                },
                                                SearchScope::Dav {
                                                    origin,
                                                    service,
                                                    collection,
                                                    query,
                                                    ..
                                                } => SearchScope::Dav {
                                                    origin,
                                                    service,
                                                    collection,
                                                    query,
                                                    mode: SearchMode::CandidateMetadata,
                                                },
                                            })
                                            .collect(),
                                    )
                                };
                                let Some(deep_scopes) = deep_scopes else {
                                    let id = authority
                                        .activity_id(context.binding)
                                        .map_err(|_| "search binding failed")?;
                                    let count = results.len();
                                    return Ok((serde_json::json!({"query":query,"result_limit_reached":results.len()>=max,"results":results,"unsearched_services":unsupported,"unavailable_services":unavailable_services,"dav_index_unavailable":index_unavailable,"coverage_complete":false,"budget_reached":true,"deep_context":null}).to_string(),id,count,false,false,true));
                                };
                                let mut scan = Scan {
                                    scopes: deep_scopes,
                                    index: 0,
                                    cursor: None,
                                    remaining: b.remaining_limits(),
                                    scanned: 0,
                                };
                                let candidates = match next(
                                    cfg,
                                    &self.account,
                                    token,
                                    &mut scan,
                                    &mut b,
                                ) {
                                    Ok(value) => value,
                                    Err(e) if e == "search_budget_exhausted" => {
                                        let id = authority
                                            .activity_id(context.binding)
                                            .map_err(|_| "search binding failed")?;
                                        let count = results.len();
                                        return Ok((serde_json::json!({"query":query,"result_limit_reached":results.len()>=max,"results":results,"unsearched_services":unsupported,"unavailable_services":unavailable_services,"dav_index_unavailable":index_unavailable,"coverage_complete":false,"budget_reached":true,"deep_context":null}).to_string(),id,count,false,false,true));
                                    }
                                    Err(e) => return Err(e),
                                };
                                scan.remaining = b.remaining_limits();
                                let scope_bytes = serde_json::to_vec(&(
                                    "self_hosted/v1",
                                    account,
                                    &origin,
                                    subject,
                                    generation,
                                    &services,
                                    query,
                                    limit,
                                ))
                                .map_err(|_| "invalid scope")?;
                                let digest: ring::digest::Digest =
                                    ring::digest::digest(&ring::digest::SHA256, &scope_bytes);
                                let mut bound = [0; 32];
                                bound.copy_from_slice(digest.as_ref());
                                let (id, continuation, metadata) = state
                                    .signed
                                    .issue(
                                        authority,
                                        context.binding,
                                        &origin,
                                        &grant,
                                        bound,
                                        candidates,
                                        context.provider_step_seq,
                                    )
                                    .map_err(|_| "search binding failed")?;
                                let has_candidates =
                                    !metadata.is_empty() || scan.index < scan.scopes.len();
                                let enabled = context.provider_steps_remaining_after_current >= 2
                                    && has_candidates;
                                let deep=enabled.then(||serde_json::json!({"contract_version":1,"activity_id":id,"continuation":continuation,"candidates":metadata,"scanned":scan.scanned,"total":null,"body_reads_used":0,"body_reads_remaining":scan.remaining.2,"provider_steps_remaining":context.provider_steps_remaining_after_current}));
                                let count = results.len();
                                let reached = scan.remaining.0 == 0
                                    || scan.remaining.1 == 0
                                    || scan.remaining.2 == 0
                                    || (context.provider_steps_remaining_after_current < 2
                                        && has_candidates);
                                if enabled {
                                    state.scans.insert(id.clone(), scan);
                                } else {
                                    state.signed.remove(&id);
                                }
                                Ok((serde_json::json!({"query":query,"result_limit_reached":results.len()>=max,"results":results,"unsearched_services":unsupported,"unavailable_services":unavailable_services,"dav_index_unavailable":index_unavailable,"coverage_complete":false,"budget_reached":reached,"deep_context":deep}).to_string(),id,count,enabled,false,reached))
                            }
                            ToolAction::DeepSearch {
                                activity_id,
                                continuation,
                                candidates,
                            } => {
                                if !candidates.is_empty() {
                                    context
                                        .input_budget
                                        .ensure_can_charge_upper_bound(64 * 1024)
                                        .map_err(|_| "search input budget exhausted")?;
                                }
                                let selected = state
                                    .signed
                                    .consume(
                                        authority,
                                        context.binding,
                                        &origin,
                                        &grant,
                                        activity_id,
                                        continuation,
                                        candidates,
                                        context.provider_step_seq,
                                    )
                                    .map_err(|_| "search binding failed")?;
                                consumed = true;
                                let scan = state
                                    .scans
                                    .get_mut(activity_id)
                                    .ok_or("search state unavailable")?;
                                let mut rows = Vec::new();
                                let mut output_left = 48 * 1024usize;
                                let mut exhausted = false;
                                for item in selected {
                                    if output_left == 0 {
                                        exhausted = true;
                                        break;
                                    }
                                    let value = match search::read(
                                        cfg,
                                        &self.account,
                                        token,
                                        &item.native,
                                        output_left.min(32768),
                                        &mut b,
                                    ) {
                                        Ok(value) => value,
                                        Err(search::SearchError::Exhausted) => {
                                            exhausted = true;
                                            break;
                                        }
                                        Err(e) => return Err(failure(e)),
                                    };
                                    output_left = output_left.saturating_sub(value.content.len());
                                    let id = match item.native.scope() {
                                        SearchScope::Mail {
                                            native_account: Some(native),
                                            ..
                                        } => super::shared_mail_ref(native, item.native.id()),
                                        _ => item.native.id().to_owned(),
                                    };
                                    rows.push(serde_json::json!({"service":item.service,"id":id,"name":item.name,"content":value.content,"provenance":value.provenance}));
                                }
                                let can_page = !exhausted
                                    && b.remaining_limits().2 > 0
                                    && context.provider_steps_remaining_after_current >= 2;
                                let candidates = if can_page {
                                    next(cfg, &self.account, token, scan, &mut b)?
                                } else {
                                    Vec::new()
                                };
                                scan.remaining = b.remaining_limits();
                                let scanned = scan.scanned;
                                let remaining = scan.remaining;
                                let more = scan.index < scan.scopes.len() || !candidates.is_empty();
                                let enabled = can_page && more;
                                let deep = if enabled {
                                    let (new_token, metadata) = state
                                        .signed
                                        .next_page(
                                            authority,
                                            context.binding,
                                            &origin,
                                            &grant,
                                            activity_id,
                                            candidates,
                                            context.provider_step_seq,
                                        )
                                        .map_err(|_| "search binding failed")?;
                                    Some(
                                        serde_json::json!({"contract_version":1,"activity_id":activity_id,"continuation":new_token,"candidates":metadata,"scanned":scanned,"total":null,"body_reads_used":40-remaining.2,"body_reads_remaining":remaining.2,"provider_steps_remaining":context.provider_steps_remaining_after_current}),
                                    )
                                } else {
                                    None
                                };
                                if !enabled {
                                    state.signed.remove(activity_id);
                                    state.scans.remove(activity_id);
                                }
                                let reached = exhausted
                                    || remaining.0 == 0
                                    || remaining.1 == 0
                                    || remaining.2 == 0
                                    || (more && context.provider_steps_remaining_after_current < 2);
                                let count = rows.len();
                                Ok((serde_json::json!({"activity_id":activity_id,"stage":"deep","selected":rows,"coverage_complete":false,"budget_reached":reached,"deep_context":deep}).to_string(),activity_id.clone(),count,enabled,true,reached))
                            }
                            _ => Err("invalid operation".into()),
                        }
                    },
                )
            })
        });
        let result = match result {
            Ok(value) => value,
            Err(e) => {
                if let Some(id) = search_id {
                    let cancelled = e == "cancelled" || context.cancellation.is_cancelled();
                    let event = isyncyou_agent::activity::StageProgressV1 {
                        schema_version: isyncyou_agent::activity::ACTIVITY_SCHEMA_VERSION,
                        activity_id: id,
                        activity_kind: isyncyou_agent::activity::ActivityKind::ProviderSearch,
                        stage: isyncyou_agent::activity::SearchStage::Names,
                        status: if cancelled {
                            isyncyou_agent::activity::StageStatus::Cancelled
                        } else {
                            isyncyou_agent::activity::StageStatus::Failed
                        },
                        scanned: 0,
                        total: None,
                        hits: 0,
                        current_item: None,
                        coverage_complete: Some(false),
                        budget_reached: Some(e == "search_budget_exhausted"),
                        continuation_available: Some(false),
                    };
                    event
                        .validate()
                        .map_err(|_| AgentError::Provider("search event unavailable".into()))?;
                    context
                        .events
                        .emit(isyncyou_agent::StreamEvent::StageProgress(event))?;
                }
                if consumed {
                    if let ToolAction::DeepSearch { activity_id, .. } = action {
                        state.signed.remove(activity_id);
                        state.scans.remove(activity_id);
                    }
                }
                return Err(if e == "cancelled" || context.cancellation.is_cancelled() {
                    AgentError::Cancelled
                } else if e == "search_budget_exhausted"
                    || e == "search input budget exhausted"
                    || e == "search output budget exhausted"
                {
                    AgentError::Provider("provider_search_budget_exhausted".into())
                } else {
                    AgentError::Provider("provider search unavailable".into())
                });
            }
        };
        let id = result.1.clone();
        let output = output(
            result.0,
            result.1,
            result.2,
            result.3,
            result.4,
            result.5,
            &mut context,
        );
        if output.is_err() {
            state.signed.remove(&id);
            state.scans.remove(&id);
        }
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn turn_budget_survives_failed_or_finished_call() {
        let total = Arc::new(Mutex::new((3, 100, 2)));
        {
            let b = TrackedBudget {
                budget: SearchBudget::new(
                    Instant::now() + Duration::from_secs(2),
                    Arc::new(|| false),
                    3,
                    100,
                    2,
                ),
                turn: total.clone(),
            };
            b.body_read().unwrap();
            let reservation = b.reserve_output(40).unwrap();
            reservation.finish(30).unwrap();
        }
        assert_eq!(*total.lock().unwrap(), (3, 70, 1));
    }
    #[test]
    fn provider_stage_events_follow_existing_reducer_contract() {
        use isyncyou_agent::activity::{ActivityKind, SearchStage, StageStatus};
        let binding = isyncyou_agent::ReadExecutionBindingV2 {
            session_id: "session".into(),
            request_id: "request".into(),
            tool_use_id: "search".into(),
            resolved_account_key: "account".into(),
            admission_account_digest: isyncyou_agent::admission_account_digest("account").unwrap(),
        };
        let token = isyncyou_agent::CancellationToken::default();
        let mut input = isyncyou_agent::ProviderInputBudgetV1::new(None, 100_000, 0);
        let mut events = Vec::new();
        let mut sink = |event| events.push(event);
        let mut context = ReadExecutionContext {
            binding: &binding,
            local_effect: None,
            mode: isyncyou_agent::ReadExecutionMode::Live,
            provider_step_seq: 0,
            provider_steps_remaining_after_current: 10,
            input_budget: &mut input,
            cancellation: &token,
            events: &mut sink,
            progressive_authority: None,
        };
        output(
            "{\"results\":[]}".into(),
            "abcdefghijklmnopqrstuv".into(),
            0,
            true,
            false,
            false,
            &mut context,
        )
        .unwrap();
        let stages: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                isyncyou_agent::StreamEvent::StageProgress(p) => {
                    Some((p.activity_kind, p.stage, p.status))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            stages,
            vec![
                (
                    ActivityKind::ProviderSearch,
                    SearchStage::Names,
                    StageStatus::Complete
                ),
                (
                    ActivityKind::ProviderSearch,
                    SearchStage::Bodies,
                    StageStatus::Queued
                ),
                (
                    ActivityKind::ProviderSearch,
                    SearchStage::Bodies,
                    StageStatus::Running
                ),
                (
                    ActivityKind::ProviderSearch,
                    SearchStage::Bodies,
                    StageStatus::Complete
                ),
                (
                    ActivityKind::ProviderSearch,
                    SearchStage::Deep,
                    StageStatus::Queued
                ),
                (
                    ActivityKind::ProviderSearch,
                    SearchStage::Deep,
                    StageStatus::Running
                ),
            ]
        );
    }

    #[test]
    fn wrong_account_and_cancel_fail_before_provider_access() {
        use isyncyou_agent::ToolExecutor;
        let executor = super::super::ProviderMailExecutor {
            cfg: isyncyou_core::Config::default(),
            account: "account".into(),
            config_path: Arc::new(std::sync::RwLock::new(None)),
            control_store: None,
            progressive: Mutex::new(Dispatch::default()),
        };
        let binding = isyncyou_agent::ReadExecutionBindingV2 {
            session_id: "session".into(),
            request_id: "request".into(),
            tool_use_id: "search".into(),
            resolved_account_key: "account".into(),
            admission_account_digest: isyncyou_agent::admission_account_digest("account").unwrap(),
        };
        let token = isyncyou_agent::CancellationToken::default();
        let mut input = isyncyou_agent::ProviderInputBudgetV1::new(None, 100_000, 0);
        let mut events = Vec::new();
        let mut sink = |event| events.push(event);
        let context = ReadExecutionContext {
            binding: &binding,
            local_effect: None,
            mode: isyncyou_agent::ReadExecutionMode::Live,
            provider_step_seq: 0,
            provider_steps_remaining_after_current: 10,
            input_budget: &mut input,
            cancellation: &token,
            events: &mut sink,
            progressive_authority: None,
        };
        let action = ToolAction::Search {
            account: "other".into(),
            services: vec!["mail".into()],
            query: "term".into(),
            limit: None,
        };
        assert!(matches!(
            executor.execute_read_with_context(&action, context),
            Err(AgentError::ToolArgs(_))
        ));
        assert!(events.is_empty());
        token.cancel();
        let mut sink = |event| events.push(event);
        let context = ReadExecutionContext {
            binding: &binding,
            local_effect: None,
            mode: isyncyou_agent::ReadExecutionMode::Live,
            provider_step_seq: 0,
            provider_steps_remaining_after_current: 10,
            input_budget: &mut input,
            cancellation: &token,
            events: &mut sink,
            progressive_authority: None,
        };
        let action = ToolAction::Search {
            account: "account".into(),
            services: vec!["mail".into()],
            query: "term".into(),
            limit: None,
        };
        assert!(matches!(
            executor.execute_read_with_context(&action, context),
            Err(AgentError::Cancelled)
        ));
        assert!(events.is_empty());
    }
}
