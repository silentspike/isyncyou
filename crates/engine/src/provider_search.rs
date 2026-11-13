//! Bounded live provider metadata and selected-resource reads.
//!
//! Callers bind the token, scope and opaque continuation to the current account,
//! grant and turn; neither tokens nor native identifiers are public output.
use isyncyou_connectors::dav::DavClient;
pub use isyncyou_connectors::request_budget::SearchBudget;
use isyncyou_connectors::request_budget::with_search_budget;
use isyncyou_core::{config::ProviderConfig, Config};
use isyncyou_store::ProviderScope;

#[derive(Clone, PartialEq, Eq)]
pub enum SearchMode { Keyword, CandidateMetadata }

#[derive(Clone, PartialEq, Eq)]
pub enum SearchScope {
    Mail { origin: String, native_account: Option<String>, mailbox_id: Option<String>, query: String, mode: SearchMode },
    Dav { origin: String, service: String, collection: String, query: String, mode: SearchMode },
}

#[derive(Clone)]
pub struct SearchCursor { account: String, scope: SearchScope, position: usize, revision: String }

#[derive(Clone)]
pub struct SearchCandidate {
    account: String,
    scope: SearchScope,
    id: String,
    name: String,
    item_type: String,
    provenance: String,
    revision: Option<String>,
}
impl SearchCandidate {
    pub fn scope(&self) -> &SearchScope { &self.scope }
    pub fn id(&self) -> &str { &self.id }
    pub fn name(&self) -> &str { &self.name }
    pub fn item_type(&self) -> &str { &self.item_type }
    pub fn provenance(&self) -> &str { &self.provenance }
}

pub struct SearchPage {
    pub items: Vec<SearchCandidate>,
    pub next_cursor: Option<SearchCursor>,
    pub source_revision: String,
    pub coverage_complete: bool,
}

pub struct SearchDetail { pub content: String, pub provenance: String }

#[derive(Debug)]
pub enum SearchError { Invalid, Unavailable, Exhausted, Cancelled, Changed }

fn budget_error(error: isyncyou_connectors::request_budget::BudgetSendError) -> SearchError {
    match error { isyncyou_connectors::request_budget::BudgetSendError::Cancelled => SearchError::Cancelled, _ => SearchError::Exhausted }
}
fn budget_result<T>(budget: &SearchBudget, result: Result<T, SearchError>) -> Result<T, SearchError> {
    budget.remaining().map_err(budget_error)?;
    if result.is_err() && budget.was_exhausted() { return Err(SearchError::Exhausted); }
    result
}

fn ensure_scope(cfg: &Config, account: &str, scope: &SearchScope) -> Result<(), SearchError> {
    let (origin, query) = match scope { SearchScope::Mail { origin, query, .. } | SearchScope::Dav { origin, query, .. } => (origin, query) };
    if query.trim().is_empty() || query.len() > 1024 || query.chars().any(char::is_control) { return Err(SearchError::Invalid); }
    match cfg.provider_accounts.get(account) {
        Some(ProviderConfig::SelfHosted { origin: actual, .. }) if actual == origin => Ok(()),
        _ => Err(SearchError::Changed),
    }
}
fn dav_scope(account: &str, scope: &SearchScope) -> Result<ProviderScope, SearchError> {
    match scope {
        SearchScope::Dav { origin, service, collection, .. } if matches!(service.as_str(), "calendar" | "contacts") => Ok(ProviderScope {
            account_id: account.into(), origin: origin.clone(), service: service.clone(), collection: collection.clone(),
        }),
        _ => Err(SearchError::Invalid),
    }
}
#[cfg(test)]
thread_local! { static DAV_SEARCH_LOOPBACK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }
fn dav_client(origin: &str, token: &str) -> Result<DavClient, SearchError> {
    #[cfg(test)]
    if DAV_SEARCH_LOOPBACK.with(|flag| flag.get()) {
        return isyncyou_connectors::dav::test_loopback_client(origin).map_err(|_| SearchError::Unavailable);
    }
    DavClient::new(origin, token.to_owned()).map_err(|_| SearchError::Unavailable)
}
fn dav_authorized(client: &DavClient, scope: &ProviderScope) -> Result<(), SearchError> {
    let collections = client.discover("/.well-known/caldav").map_err(|_| SearchError::Unavailable)?;
    if !collections.iter().any(|c| c.url == scope.collection && if scope.service == "calendar" { c.calendar } else { c.addressbook })
        || !client.permits(scope, None, "read").map_err(|_| SearchError::Unavailable)? {
        return Err(SearchError::Unavailable);
    }
    Ok(())
}

/// Enumerate only currently advertised scopes, with all protocol calls charged
/// to the same budget as subsequent pages and detail reads.
pub fn scopes(cfg: &Config, account: &str, access_token: &str, services: &[String],
    query: &str, mode: SearchMode, budget: &mut SearchBudget) -> Result<Vec<SearchScope>, SearchError> {
    if access_token.is_empty() || query.trim().is_empty() || query.len() > 1024 || query.chars().any(char::is_control)
        || services.iter().any(|s| !matches!(s.as_str(), "mail" | "calendar" | "todo" | "contacts")) {
        return Err(SearchError::Invalid);
    }
    let origin = match cfg.provider_accounts.get(account) {
        Some(ProviderConfig::SelfHosted { origin, .. }) => origin.clone(),
        _ => return Err(SearchError::Changed),
    };
    let result = with_search_budget(budget, || {
        budget.remaining().map_err(budget_error)?;
        let mut out = Vec::new();
        if services.iter().any(|v| v == "mail") {
            let client = crate::self_hosted_mail::client_with_token(cfg, account, access_token).map_err(|_| SearchError::Unavailable)?;
            out.push(SearchScope::Mail { origin: origin.clone(), native_account: None, mailbox_id: None, query: query.into(), mode: mode.clone() });
            let accounts = crate::self_hosted_mail::shared_mail_accessible_accounts(cfg, account, &client).map_err(|_| SearchError::Unavailable)?;
            if accounts.len() > 64 { return Err(SearchError::Exhausted); }
            for native in accounts {
                let readable = crate::self_hosted_mail::shared_mail_readable_mailboxes(cfg, account, &client, &native).map_err(|_| SearchError::Unavailable)?;
                if !readable.is_empty() {
                    out.push(SearchScope::Mail { origin: origin.clone(), native_account: Some(native), mailbox_id: None, query: query.into(), mode: mode.clone() });
                }
            }
        }
        if services.iter().any(|v| matches!(v.as_str(), "calendar" | "todo" | "contacts")) {
            let client = dav_client(&origin, access_token)?;
            let collections = client.discover("/.well-known/caldav").map_err(|_| SearchError::Unavailable)?;
            for collection in collections {
                for (enabled, service) in [
                    (collection.calendar && services.iter().any(|v| v == "calendar" || v == "todo"), "calendar"),
                    (collection.addressbook && services.iter().any(|v| v == "contacts"), "contacts"),
                ] {
                    if enabled {
                        let native = ProviderScope { account_id: account.into(), origin: origin.clone(), service: service.into(), collection: collection.url.clone() };
                        if client.permits(&native, None, "read").map_err(|_| SearchError::Unavailable)? {
                            out.push(SearchScope::Dav { origin: origin.clone(), service: service.into(), collection: collection.url.clone(), query: query.into(), mode: mode.clone() });
                        }
                    }
                    if out.len() > 64 { return Err(SearchError::Exhausted); }
                }
            }
        }
        Ok(out)
    });
    budget_result(budget, result)
}

pub fn page(cfg: &Config, account: &str, access_token: &str, scope: &SearchScope,
    cursor: Option<&SearchCursor>, limit: usize, budget: &mut SearchBudget) -> Result<SearchPage, SearchError> {
    ensure_scope(cfg, account, scope)?;
    if access_token.is_empty() || !(1..=12).contains(&limit) || cursor.is_some_and(|c| c.account != account || c.scope != *scope) { return Err(SearchError::Invalid); }
    let result = with_search_budget(budget, || {
        budget.remaining().map_err(budget_error)?;
        match scope {
            SearchScope::Mail { native_account, mailbox_id, query, mode, .. } => {
                let position = cursor.map_or(0, |c| c.position);
                let result = crate::self_hosted_mail::live_mail_metadata_search_with_token(cfg, account, access_token,
                    native_account.as_deref(), mailbox_id.as_deref(), if *mode == SearchMode::Keyword { Some(query) } else { None }, position, limit,
                    cursor.map(|c| c.revision.as_str())).map_err(|_| SearchError::Unavailable)?;
                let items = result.items.iter().map(|item| {
                    let id = item["id"].as_str().filter(|v| !v.is_empty()).ok_or(SearchError::Unavailable)?;
                    let name = item["subject"].as_str().unwrap_or("").chars().take(256).collect();
                    Ok(SearchCandidate { account: account.into(), scope: scope.clone(), id: id.into(), name, item_type: "message".into(), provenance: "jmap_live_metadata".into(), revision: None })
                }).collect::<Result<Vec<_>, SearchError>>()?;
                let next_cursor = result.next_position.map(|position| SearchCursor { account: account.into(), scope: scope.clone(), position, revision: result.query_state.clone() });
                Ok(SearchPage { items, coverage_complete: next_cursor.is_none(), next_cursor, source_revision: result.query_state })
            }
            SearchScope::Dav { origin, query, mode, .. } => {
                let native = dav_scope(account, scope)?;
                let client = dav_client(origin, access_token)?;
                dav_authorized(&client, &native)?;
                let (rows, revision, more) = client.search_metadata_page(&native, cursor.map(|c| c.revision.as_str()), limit).map_err(|_| SearchError::Unavailable)?;
                dav_authorized(&client, &native)?;
                let needle = query.to_lowercase();
                let items = rows.into_iter().filter_map(|(id, etag)| {
                    let name: String = id.rsplit('/').next().unwrap_or("").chars().take(256).collect();
                    if *mode == SearchMode::Keyword && !name.to_lowercase().contains(&needle) { return None; }
                    Some(SearchCandidate { account: account.into(), scope: scope.clone(), id, name, item_type: if native.service == "calendar" { "calendar_resource".into() } else { "contact_resource".into() }, provenance: "dav_live_href_metadata".into(), revision: Some(etag) })
                }).collect();
                let next_cursor = more.then(|| SearchCursor { account: account.into(), scope: scope.clone(), position: 0, revision: revision.clone() });
                // A DAV sync token is a continuation, not proof that concurrent
                // changes were absent across the whole multi-page traversal.
                Ok(SearchPage { items, next_cursor, source_revision: revision, coverage_complete: false })
            }
        }
    });
    budget_result(budget, result)
}

pub fn read(cfg: &Config, account: &str, access_token: &str, candidate: &SearchCandidate,
    max_bytes: usize, budget: &mut SearchBudget) -> Result<SearchDetail, SearchError> {
    ensure_scope(cfg, account, &candidate.scope)?;
    if candidate.account != account || access_token.is_empty() || max_bytes == 0 || max_bytes > 1024 * 1024 { return Err(SearchError::Invalid); }
    budget.body_read().map_err(budget_error)?;
    let result = with_search_budget(budget, || {
        let content = match &candidate.scope {
            SearchScope::Mail { native_account, .. } => {
                let provider = crate::self_hosted_mail::client_with_token(cfg, account, access_token).map_err(|_| SearchError::Unavailable)?;
                let value = match native_account {
                    Some(native) => crate::self_hosted_mail::read_body_in_scope(cfg, account, &provider, native, &candidate.id, max_bytes),
                    None => crate::self_hosted_mail::read_body(cfg, account, &provider, &candidate.id, max_bytes),
                }.map_err(|_| SearchError::Unavailable)?;
                serde_json::to_string(&value).map_err(|_| SearchError::Unavailable)?
            }
            SearchScope::Dav { origin, .. } => {
                let native = dav_scope(account, &candidate.scope)?;
                let client = dav_client(origin, access_token)?;
                dav_authorized(&client, &native)?;
                if !client.permits(&native, Some(&candidate.id), "read").map_err(|_| SearchError::Unavailable)? { return Err(SearchError::Unavailable); }
                let object = client.read_resource(&native, &candidate.id).map_err(|_| SearchError::Unavailable)?;
                if object.etag != candidate.revision { return Err(SearchError::Changed); }
                dav_authorized(&client, &native)?;
                if !client.permits(&native, Some(&candidate.id), "read").map_err(|_| SearchError::Unavailable)? { return Err(SearchError::Unavailable); }
                String::from_utf8(object.raw).map_err(|_| SearchError::Unavailable)?
            }
        };
        if content.len() > max_bytes { return Err(SearchError::Exhausted); }
        Ok(SearchDetail { content, provenance: "provider_live_authorized".into() })
    });
    budget_result(budget, result)
}

/// Search an already-open local DAV index, explicitly labelled local and
/// bounded by the same budget. The caller opens the store before taking any
/// credential lock; this method does not establish a bound on store opening.
pub fn indexed_keyword(cfg: &Config, account: &str, access_token: &str, scope: &SearchScope,
    store: &mut isyncyou_store::Store, limit: usize, budget: &mut SearchBudget) -> Result<SearchPage, SearchError> {
    ensure_scope(cfg, account, scope)?;
    let (origin, query) = match scope {
        SearchScope::Dav { origin, query, mode: SearchMode::Keyword, .. } => (origin, query),
        _ => return Err(SearchError::Invalid),
    };
    if access_token.is_empty() || query.trim().is_empty() || !(1..=12).contains(&limit) { return Err(SearchError::Invalid); }
    let literal_query = format!("\"{}\"", query.replace('"', "\"\""));
    if literal_query.len() > 1024 { return Err(SearchError::Invalid); }
    let native = dav_scope(account, scope)?;
    let result = with_search_budget(budget, || {
        let client = dav_client(origin, access_token)?;
        dav_authorized(&client, &native)?;
        let interrupt_budget = budget.clone();
        let interrupt: std::sync::Arc<dyn Fn() -> bool + Send + Sync> = std::sync::Arc::new(move || interrupt_budget.remaining().is_err());
        let before = store.provider_dav_state_bounded(&native, interrupt.clone()).map_err(|_| SearchError::Unavailable)?;
        let cursor = match &before { Some((Some(cursor), true)) => cursor.clone(), _ => return Err(SearchError::Unavailable) };
        let available = budget.remaining_limits().1.min(8 * 1024 * 1024);
        if available == 0 { return Err(SearchError::Exhausted); }
        let reservation = budget.reserve_output(available).map_err(budget_error)?;
        let (objects, _covered) = store.search_provider_dav_bounded(&native, &literal_query, limit, available, interrupt.clone())
            .map_err(|_| SearchError::Unavailable)?;
        let used = objects.iter().try_fold(0usize, |n, object| n.checked_add(object.raw.len())).ok_or(SearchError::Exhausted)?;
        if used > available { return Err(SearchError::Exhausted); }
        reservation.finish(used).map_err(budget_error)?;
        if store.provider_dav_state_bounded(&native, interrupt).map_err(|_| SearchError::Unavailable)? != before {
            return Err(SearchError::Changed);
        }
        dav_authorized(&client, &native)?;
        let items = objects.into_iter().map(|object| {
            let name: String = object.remote_id.rsplit('/').next().unwrap_or("").chars().take(256).collect();
            SearchCandidate { account: account.into(), scope: scope.clone(), id: object.remote_id, name,
                item_type: if native.service == "calendar" { "calendar_resource".into() } else { "contact_resource".into() }, provenance: "dav_local_index_live_acl".into(), revision: object.etag }
        }).collect();
        // The two bounded state reads do not form one long SQLite snapshot.
        Ok(SearchPage { items, next_cursor: None, source_revision: cursor, coverage_complete: false })
    });
    budget_result(budget, result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    #[test]
    fn cursor_and_candidate_cannot_cross_account_or_scope() {
        let origin = "https://example.test/".to_owned();
        let mut cfg = Config::default();
        for account in ["one", "two"] {
            cfg.provider_accounts.insert(account.into(), ProviderConfig::SelfHosted { origin: origin.clone(), client_id: "fixture".into() });
        }
        let scope = SearchScope::Mail { origin, native_account: None, mailbox_id: None, query: "term".into(), mode: SearchMode::CandidateMetadata };
        let cursor = SearchCursor { account: "one".into(), scope: scope.clone(), position: 12, revision: "state".into() };
        let candidate = SearchCandidate { account: "one".into(), scope: scope.clone(), id: "id".into(), name: "name".into(), item_type: "message".into(), provenance: "fixture".into(), revision: None };
        let mut budget = SearchBudget::new(Instant::now() + Duration::from_secs(1), Arc::new(|| false), 2, 1024, 1);
        assert!(matches!(page(&cfg, "two", "fixture", &scope, Some(&cursor), 12, &mut budget), Err(SearchError::Invalid)));
        assert!(matches!(read(&cfg, "two", "fixture", &candidate, 100, &mut budget), Err(SearchError::Invalid)));
        assert_eq!(budget.remaining_limits(), (2, 1024, 1));
    }

    #[test]
    fn live_mail_page_reports_request_budget_exhaustion_without_extra_request() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = format!("http://{}/", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            for index in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                let mut request = Vec::new();
                let mut buf = [0; 2048];
                loop {
                    let n = stream.read(&mut buf).unwrap(); assert_ne!(n, 0); request.extend_from_slice(&buf[..n]);
                    if let Some(end) = request.windows(4).position(|v| v == b"\r\n\r\n") {
                        let header = String::from_utf8_lossy(&request[..end]);
                        let len = header.lines().find_map(|line| line.to_ascii_lowercase().strip_prefix("content-length:").and_then(|v| v.trim().parse::<usize>().ok())).unwrap_or(0);
                        if request.len() >= end + 4 + len { break; }
                    }
                }
                let request = String::from_utf8(request).unwrap();
                let (method, value) = if index == 0 {
                    assert!(request.contains("Email/query"));
                    ("Email/query", serde_json::json!({"accountId":"account","queryState":"state","position":0,"total":2,"ids":["a"]}))
                } else {
                    assert!(request.contains("Email/get"));
                    ("Email/get", serde_json::json!({"accountId":"account","notFound":[],"list":[{"id":"a","receivedAt":"2026-01-01T00:00:00Z","subject":"hello"}]}))
                };
                let body = serde_json::json!({"methodResponses":[[method,value,"c1"]]}).to_string();
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
            listener.set_nonblocking(true).unwrap();
            let until = Instant::now() + Duration::from_millis(250);
            while Instant::now() < until {
                match listener.accept() {
                    Ok(_) => panic!("budget allowed an extra provider request"),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(5)),
                    Err(error) => panic!("fixture accept failed: {error}"),
                }
            }
        });
        let mut cfg = Config::default();
        cfg.provider_accounts.insert("local".into(), ProviderConfig::SelfHosted { origin: origin.clone(), client_id: "fixture".into() });
        let scope = SearchScope::Mail { origin, native_account: None, mailbox_id: None, query: "hello".into(), mode: SearchMode::CandidateMetadata };
        let mut budget = SearchBudget::new(Instant::now() + Duration::from_secs(3), Arc::new(|| false), 2, 100000, 1);
        crate::self_hosted_mail::SEARCH_LOOPBACK_FIXTURE.with(|v| v.set(true));
        assert!(matches!(page(&cfg, "local", "fixture", &scope, None, 1, &mut budget), Err(SearchError::Exhausted)));
        crate::self_hosted_mail::SEARCH_LOOPBACK_FIXTURE.with(|v| v.set(false));
        assert_eq!(budget.remaining_limits().0, 0);
        server.join().unwrap();
    }

    #[test]
    fn indexed_keyword_uses_local_index_and_rechecks_live_acl() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = format!("http://{}/", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            for index in 0..19 {
                let (mut stream, _) = listener.accept().unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                let mut request = Vec::new(); let mut buf = [0; 2048];
                loop {
                    let n = stream.read(&mut buf).unwrap(); assert_ne!(n, 0); request.extend_from_slice(&buf[..n]);
                    if let Some(end) = request.windows(4).position(|v| v == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&request[..end]);
                        let len = headers.lines().find_map(|line| line.to_ascii_lowercase().strip_prefix("content-length:").and_then(|v| v.trim().parse::<usize>().ok())).unwrap_or(0);
                        if request.len() >= end + 4 + len { break; }
                    }
                }
                let wire = String::from_utf8(request).unwrap();
                if index == 13 {
                    assert!(wire.starts_with("GET /cal/a.ics "));
                    let body = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//fixture//EN\r\nBEGIN:VEVENT\r\nUID:task\r\nDTSTAMP:20261001T100000Z\r\nDTSTART:20261028T100000Z\r\nSUMMARY:needle\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
                    write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nETag: \"v1\"\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
                    continue;
                }
                if index == 12 || index == 18 {
                    assert!(wire.starts_with("PROPFIND /cal/a.ics "));
                    let body = "<d:multistatus xmlns:d=\"DAV:\"><d:response><d:href>/cal/a.ics</d:href><d:propstat><d:prop><d:current-user-privilege-set><d:privilege><d:read/></d:privilege></d:current-user-privilege-set></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>";
                    write!(stream, "HTTP/1.1 207 Multi-Status\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
                    continue;
                }
                let phase = if index >= 14 { index - 14 } else { index % 4 };
                let item = match phase {
                    0 => { assert!(wire.starts_with("PROPFIND /.well-known/caldav ")); "<d:response><d:href>/.well-known/caldav</d:href><d:propstat><d:prop><d:current-user-principal><d:href>/principal/</d:href></d:current-user-principal></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>" }
                    1 => { assert!(wire.starts_with("PROPFIND /principal/ ")); "<d:response><d:href>/principal/</d:href><d:propstat><d:prop><c:calendar-home-set><d:href>/home/</d:href></c:calendar-home-set></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat><d:propstat><d:prop><a:addressbook-home-set/></d:prop><d:status>HTTP/1.1 404 Not Found</d:status></d:propstat></d:response>" }
                    2 => { assert!(wire.starts_with("PROPFIND /home/ ")); "<d:response><d:href>/home/</d:href><d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response><d:response><d:href>/cal/</d:href><d:propstat><d:prop><d:resourcetype><d:collection/><c:calendar/></d:resourcetype></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>" }
                    _ => { assert!(wire.starts_with("PROPFIND /cal/ ")); "<d:response><d:href>/cal/</d:href><d:propstat><d:prop><d:current-user-privilege-set><d:privilege><d:read/></d:privilege></d:current-user-privilege-set></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>" }
                };
                let body = format!("<d:multistatus xmlns:d=\"DAV:\" xmlns:c=\"urn:ietf:params:xml:ns:caldav\" xmlns:a=\"urn:ietf:params:xml:ns:carddav\">{item}</d:multistatus>");
                write!(stream, "HTTP/1.1 207 Multi-Status\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
        });
        let mut cfg = Config::default();
        cfg.provider_accounts.insert("local".into(), ProviderConfig::SelfHosted { origin: origin.clone(), client_id: "fixture".into() });
        let scope = SearchScope::Dav { origin: origin.clone(), service: "calendar".into(), collection: format!("{origin}cal/"), query: "needle".into(), mode: SearchMode::Keyword };
        let native = dav_scope("local", &scope).unwrap();
        let mut store = isyncyou_store::Store::open_in_memory().unwrap();
        let raw = b"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//fixture//EN\r\nBEGIN:VEVENT\r\nUID:task\r\nDTSTAMP:20261001T100000Z\r\nDTSTART:20261028T100000Z\r\nSUMMARY:needle\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        store.reconcile_provider_snapshot(&native, &[isyncyou_store::ProviderObject { remote_id: format!("{origin}cal/a.ics"), etag: Some("\"v1\"".into()), raw: raw.to_vec() }], "cursor").unwrap();
        let mut budget = SearchBudget::new(Instant::now() + Duration::from_secs(3), Arc::new(|| false), 20, 100000, 1);
        DAV_SEARCH_LOOPBACK.with(|flag| flag.set(true));
        let result = indexed_keyword(&cfg, "local", "fixture", &scope, &mut store, 12, &mut budget).unwrap();
        assert_eq!(result.items.len(), 1);
        assert_eq!(result.items[0].provenance(), "dav_local_index_live_acl");
        assert!(!result.coverage_complete);
        assert!(budget.remaining_limits().1 < 100000);
        let detail = read(&cfg, "local", "fixture", &result.items[0], 4096, &mut budget).unwrap();
        assert!(detail.content.contains("SUMMARY:needle"));
        assert_eq!(detail.provenance, "provider_live_authorized");
        assert_eq!(budget.remaining_limits().2, 0);
        DAV_SEARCH_LOOPBACK.with(|flag| flag.set(false));
        server.join().unwrap();
    }
}
