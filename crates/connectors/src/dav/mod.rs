//! CalDAV and CardDAV collection synchronization and conditional resource writes.
//!
//! The stored payload is the original server representation; it is never
//! regenerated from a partial Graph-shaped projection.

pub mod projection;
pub mod edit;
pub mod reminders;

use isyncyou_store::{ProviderAlarmIndex, ProviderChange, ProviderDavIndex, ProviderObject, ProviderScope, Store};
use quick_xml::events::Event;
use quick_xml::name::ResolveResult;
use quick_xml::reader::NsReader;
use reqwest::blocking::Client;
use reqwest::header::{CONTENT_TYPE, ETAG, IF_MATCH, IF_NONE_MATCH};
use std::collections::HashSet;
use std::io::Read;
use url::Url;

#[derive(Debug)]
pub enum DavError {
    InvalidUrl,
    Http(u16),
    Conflict,
    InvalidSyncToken,
    InvalidResponse,
    Transport(String),
    Store(String),
}

impl std::fmt::Display for DavError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for DavError {}

#[derive(Default)]
struct Resource {
    href: String,
    status: Option<u16>,
    prop_status: Option<u16>,
    propstats: usize,
    etag: Option<String>,
    self_href_count: usize,
    self_status_count: usize,
    self_prop_status_count: usize,
    self_prop_count: usize,
    self_etag_count: usize,
    self_unexpected: bool,
}

struct Page {
    token: String,
    entries: Vec<Resource>,
}

fn valid_collection_self(entry: &Resource) -> bool {
    !entry.self_unexpected && entry.self_href_count == 1 && entry.self_status_count == 0 &&
        entry.propstats == 1 && entry.self_prop_count == 1 && entry.self_prop_status_count == 1 &&
        entry.prop_status == Some(200) && entry.self_etag_count == 1 &&
        entry.etag.as_deref().is_some_and(|etag| !etag.is_empty())
}

fn accept_collection_self(entry: &Resource, seen: &mut bool) -> Result<(), DavError> {
    if *seen || !valid_collection_self(entry) { return Err(DavError::InvalidResponse); }
    *seen = true;
    Ok(())
}

struct Staging<'a> {
    store: &'a Store,
    scope: &'a ProviderScope,
    generation: String,
    complete: bool,
}

impl<'a> Staging<'a> {
    fn begin(store: &'a Store, scope: &'a ProviderScope) -> Result<Self, DavError> {
        use ring::rand::{SecureRandom, SystemRandom};
        let mut bytes = [0u8; 16];
        SystemRandom::new().fill(&mut bytes).map_err(|_| DavError::Transport("DAV staging random source unavailable".into()))?;
        let generation: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        store.begin_provider_staging(scope, &generation).map_err(|e| DavError::Store(e.to_string()))?;
        Ok(Self { store, scope, generation, complete: false })
    }

    fn put(&self, object: &ProviderObject) -> Result<(), DavError> {
        self.store.replace_provider_staged_object(self.scope, &self.generation, object).map_err(|e| DavError::Store(e.to_string()))?;
        if self.scope.service == "calendar" {
            self.store.stage_provider_dav_index(self.scope, &self.generation, &object.remote_id, &dav_index(&object.raw)).map_err(|e| DavError::Store(e.to_string()))?;
            self.store.stage_provider_alarm_index(self.scope, &self.generation, &object.remote_id, &alarm_index(&object.raw)).map_err(|e| DavError::Store(e.to_string()))?;
        }
        Ok(())
    }

    fn remove(&self, id: &str) -> Result<(), DavError> {
        self.store.remove_provider_staged_object(self.scope, &self.generation, id).map_err(|e| DavError::Store(e.to_string()))
    }

    fn finish(&mut self, cursor: &str) -> Result<(), DavError> {
        self.store.finish_provider_staging(self.scope, &self.generation, cursor).map_err(|e| DavError::Store(e.to_string()))?;
        self.complete = true;
        Ok(())
    }
}

fn dav_index(raw: &[u8]) -> ProviderDavIndex {
    match edit::resource_index(raw) {
        Ok(edit::ResourceIndex::Event { uid }) => ProviderDavIndex::Valid { uid, kind:"event".into(), parent_uid:None },
        Ok(edit::ResourceIndex::Task(task)) => ProviderDavIndex::Valid { uid:task.uid, kind:"task".into(), parent_uid:task.parent_uid },
        Err(_) => ProviderDavIndex::Invalid,
    }
}

fn alarm_index(raw: &[u8]) -> ProviderAlarmIndex {
    match edit::resource_has_alarms(raw) {
        Ok(has_alarm) => ProviderAlarmIndex::Valid { has_alarm },
        Err(_) => ProviderAlarmIndex::Invalid,
    }
}

impl Drop for Staging<'_> {
    fn drop(&mut self) {
        if !self.complete { let _ = self.store.abort_provider_staging(self.scope, &self.generation); }
    }
}

fn bounded_body(response: reqwest::blocking::Response) -> Result<Vec<u8>, DavError> {
    const MAX: u64 = 16 * 1024 * 1024;
    let mut bytes = Vec::new();
    response.take(MAX + 1).read_to_end(&mut bytes).map_err(|_| DavError::Transport("DAV response failed".into()))?;
    if bytes.len() as u64 > MAX { return Err(DavError::InvalidResponse); }
    Ok(bytes)
}

fn status(s: &str) -> Option<u16> {
    s.split_whitespace().nth(1)?.parse().ok()
}

fn invalid_sync_token(bytes: &[u8]) -> Result<bool, DavError> {
    let mut reader = NsReader::from_reader(bytes);
    loop {
        let (ns, event) = reader.read_resolved_event().map_err(|_| DavError::InvalidResponse)?;
        let dav = matches!(ns, ResolveResult::Bound(ref uri) if uri.as_ref() == b"DAV:");
        match event {
            Event::Start(e) | Event::Empty(e) if dav && e.local_name().as_ref() == b"valid-sync-token" => return Ok(true),
            Event::DocType(_) => return Err(DavError::InvalidResponse),
            Event::Eof => return Ok(false),
            _ => {}
        }
    }
}

// Namespace-aware parsing prevents a foreign XML vocabulary from impersonating
// DAV elements. A failed/unsupported property is never treated as deletion.
fn parse_page(bytes: &[u8]) -> Result<Page, DavError> {
    let mut reader = NsReader::from_reader(bytes);
    let mut stack: Vec<String> = Vec::new();
    let mut text = String::new();
    let mut current: Option<Resource> = None;
    let mut entries = Vec::new();
    let mut token = None;
    loop {
        let (ns, event) = reader.read_resolved_event().map_err(|_| DavError::InvalidResponse)?;
        let dav = matches!(ns, ResolveResult::Bound(ref uri) if uri.as_ref() == b"DAV:");
        match event {
            Event::Start(e) => {
                let local = e.local_name();
                let name = if dav { String::from_utf8_lossy(local.as_ref()).into_owned() } else { String::new() };
                if name == "response" { current = Some(Resource::default()); }
                if name == "propstat" { if let Some(r) = &mut current { r.propstats += 1; } }
                stack.push(name);
                if let Some(r) = &mut current {
                    let path: Vec<&str> = stack.iter().map(String::as_str).collect();
                    match path.as_slice() {
                        ["multistatus", "response"] | ["multistatus", "response", "propstat"] => {},
                        ["multistatus", "response", "href"] => r.self_href_count += 1,
                        ["multistatus", "response", "status"] => r.self_status_count += 1,
                        ["multistatus", "response", "propstat", "prop"] => r.self_prop_count += 1,
                        ["multistatus", "response", "propstat", "status"] => r.self_prop_status_count += 1,
                        ["multistatus", "response", "propstat", "prop", "getetag"] => r.self_etag_count += 1,
                        _ => r.self_unexpected = true,
                    }
                }
                text.clear();
            }
            Event::End(_) => {
                let path: Vec<&str> = stack.iter().map(String::as_str).collect();
                let value = text.trim().to_owned();
                match path.as_slice() {
                    ["multistatus", "sync-token"] => token = Some(value),
                    ["multistatus", "response", "href"] => if let Some(r) = &mut current { r.href = value; },
                    ["multistatus", "response", "status"] => if let Some(r) = &mut current { r.status = status(&value); },
                    ["multistatus", "response", "propstat", "status"] => if let Some(r) = &mut current { r.prop_status = status(&value); },
                    ["multistatus", "response", "propstat", "prop", "getetag"] => if let Some(r) = &mut current { r.etag = Some(value); },
                    _ => {}
                }
                if stack.last().map(String::as_str) == Some("response") {
                    entries.push(current.take().ok_or(DavError::InvalidResponse)?);
                }
                stack.pop().ok_or(DavError::InvalidResponse)?;
            }
            Event::Text(e) => {
                text.push_str(&e.decode().map_err(|_| DavError::InvalidResponse)?);
            }
            Event::GeneralRef(e) => {
                let entity = e.decode().map_err(|_| DavError::InvalidResponse)?;
                let encoded = format!("&{entity};");
                text.push_str(&quick_xml::escape::unescape(&encoded).map_err(|_| DavError::InvalidResponse)?);
            }
            Event::Empty(_) => if let Some(r) = &mut current { r.self_unexpected = true; },
            Event::Eof => break,
            Event::DocType(_) => return Err(DavError::InvalidResponse),
            _ => {}
        }
    }
    if !stack.is_empty() || current.is_some() { return Err(DavError::InvalidResponse); }
    Ok(Page { token: token.filter(|s| !s.is_empty()).ok_or(DavError::InvalidResponse)?, entries })
}

fn escape_xml(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
}

fn valid_lock_token(token: &str) -> bool {
    let Some(inner) = token.strip_prefix('<').and_then(|value| value.strip_suffix('>')) else { return false; };
    !inner.is_empty() && inner.len() <= 4096 && !inner.chars().any(|c| c.is_control() || c.is_whitespace() || matches!(c,'<' | '>' | '(' | ')')) && Url::parse(inner).is_ok()
}

#[derive(Default)]
struct XmlNode {
    namespace: String,
    name: String,
    text: String,
    children: Vec<XmlNode>,
    has_attributes: bool,
}

impl XmlNode {
    fn child(&self, namespace: &str, name: &str) -> Option<&Self> {
        self.children.iter().find(|n| n.namespace == namespace && n.name == name)
    }
}

fn parse_tree(bytes: &[u8]) -> Result<XmlNode, DavError> {
    let mut reader = NsReader::from_reader(bytes);
    let mut stack = vec![XmlNode::default()];
    let mut count = 0usize;
    loop {
        let (ns, event) = reader.read_resolved_event().map_err(|_| DavError::InvalidResponse)?;
        let namespace = match ns { ResolveResult::Bound(uri) => String::from_utf8_lossy(uri.as_ref()).into_owned(), ResolveResult::Unbound => String::new(), ResolveResult::Unknown(_) => return Err(DavError::InvalidResponse) };
        match event {
            Event::Start(e) => {
                count += 1;
                if count > 50_000 || stack.len() > 32 { return Err(DavError::InvalidResponse); }
                let mut has_attributes = false;
                for attr in e.attributes() { let attr = attr.map_err(|_| DavError::InvalidResponse)?; let key = attr.key.as_ref(); if key != b"xmlns" && !key.starts_with(b"xmlns:") { has_attributes = true; } }
                let node = XmlNode { namespace, name: String::from_utf8_lossy(e.local_name().as_ref()).into_owned(), has_attributes, ..Default::default() };
                stack.push(node);
            }
            Event::Empty(e) => {
                count += 1;
                if count > 50_000 || stack.len() > 32 { return Err(DavError::InvalidResponse); }
                let mut has_attributes = false;
                for attr in e.attributes() { let attr = attr.map_err(|_| DavError::InvalidResponse)?; let key = attr.key.as_ref(); if key != b"xmlns" && !key.starts_with(b"xmlns:") { has_attributes = true; } }
                let node = XmlNode { namespace, name: String::from_utf8_lossy(e.local_name().as_ref()).into_owned(), has_attributes, ..Default::default() };
                stack.last_mut().ok_or(DavError::InvalidResponse)?.children.push(node);
            }
            Event::End(_) => {
                if stack.len() < 2 { return Err(DavError::InvalidResponse); }
                let node = stack.pop().ok_or(DavError::InvalidResponse)?;
                stack.last_mut().ok_or(DavError::InvalidResponse)?.children.push(node);
            }
            Event::Text(e) => stack.last_mut().ok_or(DavError::InvalidResponse)?.text.push_str(&e.decode().map_err(|_| DavError::InvalidResponse)?),
            Event::CData(e) => stack.last_mut().ok_or(DavError::InvalidResponse)?.text.push_str(&e.decode().map_err(|_| DavError::InvalidResponse)?),
            Event::GeneralRef(e) => {
                let encoded = format!("&{};", e.decode().map_err(|_| DavError::InvalidResponse)?);
                stack.last_mut().ok_or(DavError::InvalidResponse)?.text.push_str(&quick_xml::escape::unescape(&encoded).map_err(|_| DavError::InvalidResponse)?);
            }
            Event::DocType(_) => return Err(DavError::InvalidResponse),
            Event::Eof => break,
            _ => {}
        }
    }
    if stack.len() != 1 || stack[0].children.len() != 1 { return Err(DavError::InvalidResponse); }
    stack.pop().and_then(|mut root| root.children.pop()).ok_or(DavError::InvalidResponse)
}

/// A discovered DAV collection. `url` is bound to the authenticated origin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Collection {
    pub url: String,
    pub name: String,
    pub calendar: bool,
    pub addressbook: bool,
    /// `Some(true)` only when DAV:owner equals the authenticated principal.
    /// Shared or unknown ownership must not be treated as backup ownership.
    pub owned_by_current_principal: Option<bool>,
}

/// Authenticated, server-advertised personal DAV home. Fields are private so a
/// caller cannot manufacture a home outside the principal's discovery result.
#[derive(Debug, Clone)]
pub struct PersonalDavHome { home: Url, principal: Url, service: String }

// Some DAV servers redirect discovery to a collection URL without its final
// slash, then identify that same collection with a trailing slash in the 207.
fn dav_response_matches(request: &Url, response: &Url, allow_slash: bool) -> bool {
    if request == response { return true; }
    if !allow_slash || request.path().ends_with('/') { return false; }
    let mut canonical = request.clone();
    canonical.set_path(&format!("{}/", request.path()));
    &canonical == response
}

impl PersonalDavHome {
    pub fn home_url(&self) -> &str { self.home.as_str() }
    pub fn principal_url(&self) -> &str { self.principal.as_str() }
    pub fn service(&self) -> &str { &self.service }
    /// The operation id must be a freshly generated, persisted 128-bit value.
    pub fn recovered_target(&self, operation_id: &str) -> Result<String, DavError> {
        if operation_id.len() != 32 || !operation_id.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)) { return Err(DavError::InvalidResponse); }
        Ok(self.home.join(&format!("wp16-recovered-{operation_id}/")).map_err(|_| DavError::InvalidUrl)?.to_string())
    }
}

/// Server-expanded calendar projection, separate from the stored raw original.
pub struct ExpandedCalendar {
    pub href: String,
    pub etag: String,
    pub calendar_data: String,
}

/// One server-reported access control entry. Protected/inherited entries are
/// presented as such and must not be rewritten by a client-side share editor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AclEntry {
    pub principal: String,
    pub granted: bool,
    pub privileges: Vec<String>,
    pub inherited: bool,
    pub protected: bool,
}

fn utc_time(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 16 && b[8] == b'T' && b[15] == b'Z' && ical::recur::IcalRecurDateTime::parse(s).is_ok()
}

/// A bearer token is held only in memory and is never included in errors.
pub struct DavClient {
    http: Client,
    origin: Url,
    token: String,
    trusted_public_origin: Option<Url>,
}

/// Read-only loopback client for an explicitly scoped local service credential.
/// The wrapped client is private so this surface cannot issue PUT or DELETE.
pub struct LoopbackDavReader(DavClient);

impl LoopbackDavReader {
    pub fn new(origin: &str, token: String) -> Result<Self, DavError> {
        let url = Url::parse(origin).map_err(|_| DavError::InvalidUrl)?;
        let host = url.host_str().ok_or(DavError::InvalidUrl)?;
        let ip: std::net::IpAddr = host.trim_matches(['[', ']']).parse().map_err(|_| DavError::InvalidUrl)?;
        if !ip.is_loopback() || url.scheme() != "http" || !url.username().is_empty() || url.password().is_some() || url.path() != "/" || url.query().is_some() || url.fragment().is_some() || token.is_empty() { return Err(DavError::InvalidUrl); }
        let http = Client::builder().no_proxy().timeout(std::time::Duration::from_secs(30)).redirect(reqwest::redirect::Policy::none()).build().map_err(|_| DavError::Transport("client initialization failed".into()))?;
        Ok(Self(DavClient { http, origin: url, token, trusted_public_origin:None }))
    }

    /// Explicitly map the configured public origin's DAV hrefs to the same
    /// paths on loopback, without sending this credential to that public host.
    pub fn new_with_public_origin(origin: &str, token: String, public_origin: &str) -> Result<Self, DavError> {
        let mut reader = Self::new(origin, token)?;
        let public = Url::parse(public_origin).map_err(|_| DavError::InvalidUrl)?;
        if public.scheme() != "https" || public.host_str().is_none() || !public.username().is_empty() || public.password().is_some() || public.path() != "/" || public.query().is_some() || public.fragment().is_some() { return Err(DavError::InvalidUrl); }
        reader.0.trusted_public_origin = Some(public);
        Ok(reader)
    }

    pub fn discover(&self, entry: &str) -> Result<Vec<Collection>, DavError> { self.0.discover(entry) }

    /// Only explicitly verified personal collections are eligible for the
    /// account's historical export. Shared and unknown ownership are excluded.
    pub fn discover_owned(&self, entry: &str) -> Result<Vec<Collection>, DavError> {
        Ok(self.0.discover(entry)?.into_iter().filter(|c| c.owned_by_current_principal == Some(true)).collect())
    }

    pub fn sync_collection(&self, store: &Store, scope: &ProviderScope) -> Result<usize, DavError> { self.0.sync_collection(store, scope) }
}

impl DavClient {
    pub fn new(origin: &str, token: String) -> Result<Self, DavError> {
        let origin = Url::parse(origin).map_err(|_| DavError::InvalidUrl)?;
        if token.is_empty() || origin.scheme() != "https" || origin.host_str().is_none() || !origin.username().is_empty() || origin.password().is_some() || origin.query().is_some() || origin.fragment().is_some() || origin.path() != "/" {
            return Err(DavError::InvalidUrl);
        }
        let http = Client::builder().timeout(std::time::Duration::from_secs(30)).redirect(reqwest::redirect::Policy::none()).build().map_err(|_| DavError::Transport("client initialization failed".into()))?;
        Ok(Self { http, origin, token, trusted_public_origin:None })
    }

    fn checked_url(&self, href: &str) -> Result<Url, DavError> {
        self.checked_from(&self.origin, href)
    }

    fn checked_from(&self, base: &Url, href: &str) -> Result<Url, DavError> {
        let mut url = base.join(href).map_err(|_| DavError::InvalidUrl)?;
        if self.trusted_public_origin.as_ref().is_some_and(|public| public.origin() == url.origin()) {
            if !url.username().is_empty() || url.password().is_some() || url.query().is_some() || url.fragment().is_some() { return Err(DavError::InvalidUrl); }
            let mut local = self.origin.clone();
            local.set_path(url.path());
            url = local;
        }
        let path = url.path().to_ascii_lowercase();
        if url.origin() != self.origin.origin() || !url.username().is_empty() || url.password().is_some() || url.query().is_some() || url.fragment().is_some() || path.contains("%2f") || path.contains("%5c") || path.contains("%2e") || path.contains("%25") { return Err(DavError::InvalidUrl); }
        Ok(url)
    }

    fn propfind(&self, mut url: Url, depth: &str, body: &str) -> Result<(Url, XmlNode), DavError> {
        let mut redirects = HashSet::new();
        let response = loop {
            if redirects.len() >= 4 || !redirects.insert(url.clone()) { return Err(DavError::InvalidResponse); }
            let response = self.http.request(reqwest::Method::from_bytes(b"PROPFIND").map_err(|_| DavError::InvalidResponse)?, url.clone())
                .bearer_auth(&self.token).header("Depth", depth).header(CONTENT_TYPE, "application/xml; charset=utf-8").body(body.to_owned()).send()
                .map_err(|_| DavError::Transport("DAV discovery failed".into()))?;
            if matches!(response.status().as_u16(), 301 | 302 | 307 | 308) {
                if self.origin.scheme() != "https" { return Err(DavError::Http(response.status().as_u16())); }
                let location = response.headers().get(reqwest::header::LOCATION).and_then(|v| v.to_str().ok()).ok_or(DavError::InvalidResponse)?;
                url = self.checked_from(&url, location)?;
                continue;
            }
            break response;
        };
        if response.status().as_u16() != 207 { return Err(DavError::Http(response.status().as_u16())); }
        let root = parse_tree(&bounded_body(response)?)?;
        if root.namespace != "DAV:" || root.name != "multistatus" { return Err(DavError::InvalidResponse); }
        Ok((url, root))
    }

    /// Resolve one unambiguous home directly from the authenticated principal.
    pub fn personal_home(&self, entry: &str, service: &str) -> Result<PersonalDavHome, DavError> {
        const D: &str = "DAV:";
        let (ns, home_property) = match service {
            "calendar" => ("urn:ietf:params:xml:ns:caldav", "calendar-home-set"),
            "contacts" => ("urn:ietf:params:xml:ns:carddav", "addressbook-home-set"),
            _ => return Err(DavError::InvalidResponse),
        };
        fn response<'a>(client: &DavClient, root: &'a XmlNode, target: &Url, allow_slash: bool) -> Result<(&'a XmlNode,Url), DavError> {
            let mut matches = Vec::new();
            let rows: Vec<_> = root.children.iter().filter(|n| n.namespace == "DAV:" && n.name == "response").collect();
            if rows.len() != 1 { return Err(DavError::InvalidResponse); }
            for node in rows {
                let hrefs: Vec<_> = node.children.iter().filter(|n| n.namespace == "DAV:" && n.name == "href").collect();
                if hrefs.len() != 1 || hrefs[0].text.trim().is_empty() { return Err(DavError::InvalidResponse); }
                let href = hrefs[0];
                let url = client.checked_from(target,href.text.trim())?;
                if dav_response_matches(target,&url,allow_slash) { matches.push((node,url)); }
            }
            if matches.len() != 1 { return Err(DavError::InvalidResponse); } Ok(matches.remove(0))
        }
        fn property<'a>(node: &'a XmlNode, ns: &str, name: &str) -> Result<&'a XmlNode, DavError> {
            let props: Vec<_> = node.children.iter().filter(|n| n.namespace == "DAV:" && n.name == "propstat").filter(|p| p.child("DAV:","status").and_then(|s| status(s.text.trim())) == Some(200)).filter_map(|p| p.child("DAV:","prop").and_then(|p| p.child(ns,name))).collect();
            if props.len() != 1 { return Err(DavError::InvalidResponse); } Ok(props[0])
        }
        let start = self.checked_url(entry)?;
        let (entry_url, entry_tree) = self.propfind(start, "0", r#"<d:propfind xmlns:d="DAV:"><d:prop><d:current-user-principal/></d:prop></d:propfind>"#)?;
        let (node,canonical_entry) = response(self,&entry_tree,&entry_url,true)?;
        let principal_href = property(node,D,"current-user-principal")?.child(D,"href").ok_or(DavError::InvalidResponse)?.text.trim();
        if principal_href.is_empty() { return Err(DavError::InvalidResponse); }
        let principal = self.checked_from(&canonical_entry,principal_href)?;
        let body = r#"<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav" xmlns:a="urn:ietf:params:xml:ns:carddav"><d:prop><c:calendar-home-set/><a:addressbook-home-set/></d:prop></d:propfind>"#;
        let (actual, tree) = self.propfind(principal.clone(),"0",body)?;
        if actual != principal { return Err(DavError::InvalidResponse); }
        let home_set = property(response(self,&tree,&principal,false)?.0,ns,home_property)?;
        let homes: Vec<_> = home_set.children.iter().filter(|n| n.namespace == D && n.name == "href" && !n.text.trim().is_empty()).collect();
        if homes.len() != 1 { return Err(DavError::InvalidResponse); }
        let home = self.checked_from(&principal,homes[0].text.trim())?;
        if !home.path().ends_with('/') { return Err(DavError::InvalidUrl); }
        Ok(PersonalDavHome {home,principal,service:service.into()})
    }

    /// Issue a single create-only request. The caller must durably claim the
    /// approved operation before dispatch and never repeat an uncertain write.
    pub fn create_recovered_collection(&self, home: &PersonalDavHome, operation_id: &str) -> Result<(), DavError> {
        let target = self.checked_url(&home.recovered_target(operation_id)?)?;
        if self.checked_url(home.home.as_str())? != home.home { return Err(DavError::InvalidUrl); }
        let scope = ProviderScope { account_id: String::new(), origin: self.origin.to_string(), service: home.service.clone(), collection: home.home.to_string() };
        if !self.permits(&scope,None,"bind")? { return Err(DavError::Http(403)); }
        let body = if home.service == "calendar" {
            format!(r#"<c:mkcalendar xmlns:c="urn:ietf:params:xml:ns:caldav" xmlns:d="DAV:" xmlns:i="urn:isyncyou:recovery"><d:set><d:prop><d:displayname>Recovered</d:displayname><i:operation>{operation_id}</i:operation></d:prop></d:set></c:mkcalendar>"#)
        } else if home.service == "contacts" {
            format!(r#"<d:mkcol xmlns:d="DAV:" xmlns:a="urn:ietf:params:xml:ns:carddav" xmlns:i="urn:isyncyou:recovery"><d:set><d:prop><d:resourcetype><d:collection/><a:addressbook/></d:resourcetype><d:displayname>Recovered</d:displayname><i:operation>{operation_id}</i:operation></d:prop></d:set></d:mkcol>"#)
        } else { return Err(DavError::InvalidResponse); };
        let method = if home.service == "calendar" { b"MKCALENDAR".as_slice() } else { b"MKCOL".as_slice() };
        let response = self.http.request(reqwest::Method::from_bytes(method).map_err(|_| DavError::InvalidResponse)?,target)
            .bearer_auth(&self.token).header(IF_NONE_MATCH,"*").header(CONTENT_TYPE,"application/xml; charset=utf-8").header("Prefer","return=minimal").body(body).send()
            .map_err(|_| DavError::Transport("DAV collection creation outcome unknown".into()))?;
        if response.status().as_u16() != 201 { return Err(DavError::Http(response.status().as_u16())); }
        Ok(())
    }

    /// Verify the exact marked collection, principal ownership and privileges.
    /// False means a native 404 only; malformed or foreign collections fail.
    pub fn read_recovered_collection(&self, home: &PersonalDavHome, operation_id: &str) -> Result<bool, DavError> {
        const D: &str = "DAV:";
        fn one<'a>(node: &'a XmlNode, ns: &str, name: &str) -> Result<&'a XmlNode, DavError> {
            let mut matches = node.children.iter().filter(|n| n.namespace == ns && n.name == name);
            let value = matches.next().ok_or(DavError::InvalidResponse)?;
            if matches.next().is_some() { return Err(DavError::InvalidResponse); }
            Ok(value)
        }
        let target = self.checked_url(&home.recovered_target(operation_id)?)?;
        let body = r#"<d:propfind xmlns:d="DAV:" xmlns:i="urn:isyncyou:recovery"><d:prop><d:displayname/><d:resourcetype/><d:owner/><d:current-user-privilege-set/><i:operation/></d:prop></d:propfind>"#;
        let (actual, root) = match self.propfind(target.clone(),"0",body) { Err(DavError::Http(404)) => return Ok(false), other => other? };
        if actual != target { return Err(DavError::InvalidResponse); }
        let rows: Vec<_> = root.children.iter().filter(|n| n.namespace == D && n.name == "response").collect();
        if rows.len() != 1 || rows[0].child(D,"status").is_some() || self.checked_from(&target,one(rows[0],D,"href")?.text.trim())? != target { return Err(DavError::InvalidResponse); }
        let props: Vec<_> = rows[0].children.iter().filter(|n| n.namespace == D && n.name == "propstat").collect();
        if props.len() != 1 || status(one(props[0],D,"status")?.text.trim()) != Some(200) { return Err(DavError::InvalidResponse); }
        let prop = one(props[0],D,"prop")?;
        let owner = one(one(prop,D,"owner")?,D,"href")?;
        let ty = one(prop,D,"resourcetype")?;
        let required_type = if home.service == "calendar" { ("urn:ietf:params:xml:ns:caldav","calendar") } else if home.service == "contacts" { ("urn:ietf:params:xml:ns:carddav","addressbook") } else { return Err(DavError::InvalidResponse); };
        if self.checked_from(&target,owner.text.trim())? != home.principal || one(prop,D,"displayname")?.text.trim() != "Recovered" || ty.child(D,"collection").is_none() || ty.child(required_type.0,required_type.1).is_none() || one(prop,"urn:isyncyou:recovery","operation")?.text.trim() != operation_id { return Err(DavError::InvalidResponse); }
        let privileges = one(prop,D,"current-user-privilege-set")?;
        let mut read = false; let mut bind = false;
        for privilege in privileges.children.iter().filter(|p| p.namespace == D && p.name == "privilege").flat_map(|p| p.children.iter()).filter(|p| p.namespace == D) {
            read |= privilege.name == "all" || privilege.name == "read";
            bind |= privilege.name == "all" || privilege.name == "write" || privilege.name == "bind";
        }
        if !read || !bind { return Err(DavError::InvalidResponse); }
        Ok(true)
    }

    /// Discover principal, home sets, and collections with bounded same-origin
    /// redirects. Credentials never go to a different origin. Entry is an authenticated
    /// server-provided DAV endpoint, not an arbitrary cross-origin URL.
    pub fn discover(&self, entry: &str) -> Result<Vec<Collection>, DavError> {
        const D: &str = "DAV:";
        const C: &str = "urn:ietf:params:xml:ns:caldav";
        const A: &str = "urn:ietf:params:xml:ns:carddav";
        fn property<'a>(response: &'a XmlNode, ns: &str, name: &str) -> Option<&'a XmlNode> {
            response.children.iter().filter(|n| n.namespace == D && n.name == "propstat").find_map(|p| {
                let response_status = p.child(D, "status")?.text.trim();
                if status(response_status) != Some(200) { return None; }
                p.child(D, "prop")?.child(ns, name)
            })
        }
        fn href(node: &XmlNode) -> Result<&str, DavError> {
            let mut values = node.children.iter().filter(|n| n.namespace == D && n.name == "href");
            let value = values.next().map(|n| n.text.trim()).filter(|s| !s.is_empty()).ok_or(DavError::InvalidResponse)?;
            if values.next().is_some() { return Err(DavError::InvalidResponse); }
            Ok(value)
        }
        let principal_body = r#"<d:propfind xmlns:d="DAV:"><d:prop><d:current-user-principal/></d:prop></d:propfind>"#;
        let entry_url = self.checked_url(entry)?;
        let (entry_url, result) = self.propfind(entry_url, "0", principal_body)?;
        let responses: Vec<_> = result.children.iter().filter(|n| n.namespace == D && n.name == "response").collect();
        if responses.len() != 1 { return Err(DavError::InvalidResponse); }
        let principal_response = responses[0];
        let canonical_entry = self.checked_from(&entry_url,href(principal_response)?)?;
        if !dav_response_matches(&entry_url,&canonical_entry,true) { return Err(DavError::InvalidResponse); }
        let principal = href(property(principal_response, D, "current-user-principal").ok_or(DavError::InvalidResponse)?)?;
        let homes_body = r#"<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav" xmlns:a="urn:ietf:params:xml:ns:carddav"><d:prop><c:calendar-home-set/><a:addressbook-home-set/></d:prop></d:propfind>"#;
        let principal_url = self.checked_from(&canonical_entry, principal)?;
        let (principal_url, homes) = self.propfind(principal_url, "0", homes_body)?;
        let home_response = homes.children.iter().find(|n| n.namespace == D && n.name == "response" && href(n).ok().and_then(|h| self.checked_from(&principal_url, h).ok()).as_ref() == Some(&principal_url)).ok_or(DavError::InvalidResponse)?;
        let mut result = Vec::new();
        for (ns, name) in [(C, "calendar-home-set"), (A, "addressbook-home-set")] {
            let Some(home) = property(home_response, ns, name) else { continue; };
            for home_href in home.children.iter().filter(|n| n.namespace == D && n.name == "href") {
                let home_url = self.checked_from(&principal_url, home_href.text.trim())?;
                if !home_url.path().ends_with('/') { return Err(DavError::InvalidUrl); }
                let body = r#"<d:propfind xmlns:d="DAV:"><d:prop><d:displayname/><d:resourcetype/><d:owner/></d:prop></d:propfind>"#;
                let (home_url, listing) = self.propfind(home_url, "1", body)?;
                if !home_url.path().ends_with('/') { return Err(DavError::InvalidUrl); }
                for response in listing.children.iter().filter(|n| n.namespace == D && n.name == "response") {
                    let url = self.checked_from(&home_url, href(response)?)?;
                    // Assisted discovery can return an authorized shared
                    // collection outside this principal's home path. The
                    // server's response is trusted only for this origin.
                    if !url.path().ends_with('/') { return Err(DavError::InvalidUrl); }
                    let Some(ty) = property(response, D, "resourcetype") else { continue; };
                    let calendar = ty.child(C, "calendar").is_some();
                    let addressbook = ty.child(A, "addressbook").is_some();
                    if !calendar && !addressbook { continue; }
                    let name = property(response, D, "displayname").map(|n| n.text.trim().to_owned()).unwrap_or_default();
                    let owners = property(response, D, "owner").map(|node| node.children.iter().filter(|n| n.namespace == D && n.name == "href").collect::<Vec<_>>()).unwrap_or_default();
                    let owned_by_current_principal = if owners.len() == 1 && !owners[0].text.trim().is_empty() {
                        Some(self.checked_from(&url, owners[0].text.trim())? == principal_url)
                    } else { None };
                    result.push(Collection { url: url.to_string(), name, calendar, addressbook, owned_by_current_principal });
                }
            }
        }
        Ok(result)
    }

    /// Server-declared scheduling addresses for the authenticated principal.
    /// Missing or ambiguous properties do not authorize an attendee response.
    pub fn calendar_user_addresses(&self, entry: &str) -> Result<Vec<String>, DavError> {
        const D: &str = "DAV:";
        const C: &str = "urn:ietf:params:xml:ns:caldav";
        let entry_url = self.checked_url(entry)?;
        let (actual, root) = self.propfind(entry_url,"0",r#"<d:propfind xmlns:d="DAV:"><d:prop><d:current-user-principal/></d:prop></d:propfind>"#)?;
        let response = root.children.iter().find(|n| n.namespace == D && n.name == "response" && n.child(D,"href").and_then(|h| self.checked_from(&actual,h.text.trim()).ok()).as_ref() == Some(&actual)).ok_or(DavError::InvalidResponse)?;
        let principal = response.children.iter().filter(|n| n.namespace == D && n.name == "propstat").find_map(|p| {
            if status(p.child(D,"status")?.text.trim()) != Some(200) { return None; }
            p.child(D,"prop")?.child(D,"current-user-principal")?.child(D,"href")
        }).ok_or(DavError::InvalidResponse)?;
        let principal_url = self.checked_from(&actual,principal.text.trim())?;
        let (actual, root) = self.propfind(principal_url.clone(),"0",r#"<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:prop><c:calendar-user-address-set/></d:prop></d:propfind>"#)?;
        if actual != principal_url { return Err(DavError::InvalidUrl); }
        let response = root.children.iter().find(|n| n.namespace == D && n.name == "response" && n.child(D,"href").and_then(|h| self.checked_from(&actual,h.text.trim()).ok()).as_ref() == Some(&actual)).ok_or(DavError::InvalidResponse)?;
        let addresses = response.children.iter().filter(|n| n.namespace == D && n.name == "propstat").find_map(|p| {
            if status(p.child(D,"status")?.text.trim()) != Some(200) { return None; }
            p.child(D,"prop")?.child(C,"calendar-user-address-set")
        }).ok_or(DavError::InvalidResponse)?;
        if addresses.children.is_empty() || addresses.children.len() > 128 || addresses.children.iter().any(|n| n.namespace != D || n.name != "href") { return Err(DavError::InvalidResponse); }
        let mut result = Vec::new();
        for value in &addresses.children {
            let address = value.text.trim();
            let parsed = Url::parse(address).map_err(|_| DavError::InvalidResponse)?;
            if address.len() > 4096 || address.chars().any(char::is_control) || !matches!(parsed.scheme(),"mailto" | "https") { return Err(DavError::InvalidResponse); }
            result.push(address.to_owned());
        }
        Ok(result)
    }

    fn collection(&self, scope: &ProviderScope) -> Result<Url, DavError> {
        let declared = Url::parse(&scope.origin).map_err(|_| DavError::InvalidUrl)?;
        if declared.origin() != self.origin.origin() { return Err(DavError::InvalidUrl); }
        let url = self.checked_url(&scope.collection)?;
        if !url.path().ends_with('/') { return Err(DavError::InvalidUrl); }
        Ok(url)
    }

    fn resource(&self, collection: &Url, href: &str) -> Result<Url, DavError> {
        let url = self.checked_from(collection, href)?;
        let path = url.path().to_ascii_lowercase();
        if url.origin() != self.origin.origin() || url.query().is_some() || url.fragment().is_some() || !url.username().is_empty() || url.password().is_some() || !url.path().starts_with(collection.path()) || url.path() == collection.path() || path.contains("%2f") || path.contains("%5c") || path.contains("%2e") || path.contains("%25") { return Err(DavError::InvalidUrl); }
        Ok(url)
    }

    /// Check an object destination before a caller records a mutation intent.
    pub fn validate_target(&self, scope: &ProviderScope, href: &str) -> Result<String, DavError> {
        Ok(self.resource(&self.collection(scope)?, href)?.to_string())
    }

    /// Read the server's current resource version without mutating the cache.
    /// Missing or inaccessible resources remain HTTP errors, not deletions.
    pub fn read_resource(&self, scope: &ProviderScope, href: &str) -> Result<ProviderObject, DavError> {
        let url = self.resource(&self.collection(scope)?, href)?;
        let response = self.http.get(url.clone()).bearer_auth(&self.token).send().map_err(|_| DavError::Transport("DAV read failed".into()))?;
        if response.status().as_u16() != 200 { return Err(DavError::Http(response.status().as_u16())); }
        let etag = response.headers().get(ETAG).and_then(|v| v.to_str().ok()).map(str::to_owned).ok_or(DavError::InvalidResponse)?;
        Ok(ProviderObject { remote_id: url.to_string(), etag: Some(etag), raw: bounded_body(response)? })
    }

    /// Re-read the server's effective WebDAV privilege set immediately before
    /// dispatch. `resource` is None for the collection, Some for an object.
    pub fn permits(&self, scope: &ProviderScope, resource: Option<&str>, privilege: &str) -> Result<bool, DavError> {
        const D: &str = "DAV:";
        if !matches!(privilege, "bind" | "unbind" | "write-content" | "read" | "read-acl" | "write-acl") { return Err(DavError::InvalidResponse); }
        let collection = self.collection(scope)?;
        let target = match resource { Some(href) => self.resource(&collection, href)?, None => collection };
        let body = r#"<d:propfind xmlns:d="DAV:"><d:prop><d:current-user-privilege-set/></d:prop></d:propfind>"#;
        let (actual, root) = self.propfind(target.clone(), "0", body)?;
        if actual != target { return Err(DavError::InvalidUrl); }
        let node = root.children.iter().find(|n| n.namespace == D && n.name == "response" && n.child(D,"href").and_then(|h| self.checked_from(&target,h.text.trim()).ok()).as_ref() == Some(&target)).ok_or(DavError::InvalidResponse)?;
        let property = node.children.iter().filter(|n| n.namespace == D && n.name == "propstat").find_map(|n| {
            if status(n.child(D,"status")?.text.trim()) != Some(200) { return None; }
            n.child(D,"prop")?.child(D,"current-user-privilege-set")
        }).ok_or(DavError::InvalidResponse)?;
        Ok(property.children.iter().filter(|n| n.namespace == D && n.name == "privilege").flat_map(|n| n.children.iter()).any(|p| p.namespace == D && (p.name == privilege || p.name == "all" || p.name == "write" && matches!(privilege, "bind" | "unbind" | "write-content"))))
    }

    /// Read the collection's native ACL. This operation does not mutate it.
    pub fn acl(&self, scope: &ProviderScope) -> Result<Vec<AclEntry>, DavError> {
        const D: &str = "DAV:";
        let target = self.collection(scope)?;
        let body = r#"<d:propfind xmlns:d="DAV:"><d:prop><d:acl/></d:prop></d:propfind>"#;
        let (actual, root) = self.propfind(target.clone(), "0", body)?;
        if actual != target { return Err(DavError::InvalidUrl); }
        let response = root.children.iter().find(|n| n.namespace == D && n.name == "response" && n.child(D,"href").and_then(|h| self.checked_from(&target,h.text.trim()).ok()).as_ref() == Some(&target)).ok_or(DavError::InvalidResponse)?;
        let acl = response.children.iter().filter(|n| n.namespace == D && n.name == "propstat").find_map(|n| {
            if status(n.child(D,"status")?.text.trim()) != Some(200) { return None; }
            n.child(D,"prop")?.child(D,"acl")
        }).ok_or(DavError::InvalidResponse)?;
        let mut result = Vec::new();
        fn representable(node: &XmlNode) -> bool { !node.has_attributes && node.children.iter().all(representable) }
        if !representable(acl) { return Err(DavError::InvalidResponse); }
        if acl.children.iter().any(|n| n.namespace != D || n.name != "ace") { return Err(DavError::InvalidResponse); }
        for ace in acl.children.iter().filter(|n| n.namespace == D && n.name == "ace") {
            if ace.children.iter().any(|n| n.namespace != D || !matches!(n.name.as_str(), "principal" | "grant" | "deny" | "protected" | "inherited")) { return Err(DavError::InvalidResponse); }
            for name in ["principal", "grant", "deny", "protected", "inherited"] {
                if ace.children.iter().filter(|n| n.namespace == D && n.name == name).count() > 1 { return Err(DavError::InvalidResponse); }
            }
            let principal = ace.child(D,"principal").ok_or(DavError::InvalidResponse)?;
            if principal.children.len() != 1 { return Err(DavError::InvalidResponse); }
            let identifier = if let Some(href) = principal.child(D,"href") {
                self.checked_from(&target,href.text.trim())?.to_string()
            } else {
                let alternatives: Vec<_> = principal.children.iter().filter(|n| n.namespace == D && matches!(n.name.as_str(), "all" | "authenticated" | "unauthenticated" | "self")).collect();
                if alternatives.len() != 1 { return Err(DavError::InvalidResponse); }
                format!("DAV:{}",alternatives[0].name)
            };
            let grant = ace.child(D,"grant"); let deny = ace.child(D,"deny");
            let (granted, privileges) = match (grant,deny) { (Some(value),None) => (true,value), (None,Some(value)) => (false,value), _ => return Err(DavError::InvalidResponse) };
            if privileges.children.iter().any(|n| n.namespace != D || n.name != "privilege") { return Err(DavError::InvalidResponse); }
            let mut names = Vec::new();
            for item in privileges.children.iter().filter(|n| n.namespace == D && n.name == "privilege") {
                if item.children.len() != 1 { return Err(DavError::InvalidResponse); }
                let privilege = &item.children[0];
                names.push(format!("{{{}}}{}",privilege.namespace,privilege.name));
            }
            if names.is_empty() { return Err(DavError::InvalidResponse); }
            result.push(AclEntry { principal:identifier, granted, privileges:names, inherited:ace.child(D,"inherited").is_some(), protected:ace.child(D,"protected").is_some() });
        }
        Ok(result)
    }

    /// Replace the visible editable ACEs while holding an exclusive WebDAV lock.
    /// Protected and inherited entries are excluded by RFC 3744; all editable
    /// entries are represented exactly or the operation is rejected.
    pub fn update_acl_locked(&self, scope: &ProviderScope, expected: &[AclEntry], principal: &str, privilege: Option<&str>) -> Result<(), DavError> {
        if !matches!(privilege, None | Some("read") | Some("write") | Some("read-write")) { return Err(DavError::InvalidResponse); }
        let target = self.collection(scope)?;
        let principal_url = self.checked_url(principal)?;
        if principal_url.query().is_some() || principal_url.fragment().is_some() || !principal_url.username().is_empty() || principal_url.password().is_some() { return Err(DavError::InvalidUrl); }
        if !self.permits(scope, None, "write-acl")? || !self.permits(scope, None, "read-acl")? { return Err(DavError::Http(403)); }
        let lock_body = r#"<d:lockinfo xmlns:d="DAV:"><d:lockscope><d:exclusive/></d:lockscope><d:locktype><d:write/></d:locktype></d:lockinfo>"#;
        let response = self.http.request(reqwest::Method::from_bytes(b"LOCK").map_err(|_| DavError::InvalidResponse)?, target.clone()).bearer_auth(&self.token)
            .header("Depth", "0").header("Timeout", "Second-60").header(CONTENT_TYPE, "application/xml; charset=utf-8").body(lock_body).send()
            .map_err(|_| DavError::Transport("DAV lock outcome unknown".into()))?;
        if !response.status().is_success() { return Err(DavError::Http(response.status().as_u16())); }
        let token = response.headers().get("Lock-Token").and_then(|v| v.to_str().ok()).filter(|v| valid_lock_token(v)).ok_or(DavError::InvalidResponse)?.to_owned();
        let result = (|| {
            let actual = self.acl(scope)?;
            if actual != expected { return Err(DavError::Conflict); }
            let selected: Vec<_> = actual.iter().filter(|ace| ace.principal == principal_url.as_str()).collect();
            if selected.iter().any(|ace| ace.protected || ace.inherited || !ace.granted) || selected.len() > 1 { return Err(DavError::InvalidResponse); }
            let replacement = privilege.map(|p| match p { "read" => "<d:privilege><d:read/></d:privilege>", "write" => "<d:privilege><d:write/></d:privilege>", _ => "<d:privilege><d:read/></d:privilege><d:privilege><d:write/></d:privilege>" });
            let mut xml = String::from("<d:acl xmlns:d=\"DAV:\">");
            let mut inserted = false;
            for ace in actual.iter().filter(|ace| !ace.protected && !ace.inherited) {
                if ace.principal == principal_url.as_str() {
                    if let Some(privileges) = replacement { xml.push_str(&format!("<d:ace><d:principal><d:href>{}</d:href></d:principal><d:grant>{privileges}</d:grant></d:ace>", escape_xml(principal_url.as_str()))); }
                    inserted = true;
                    continue;
                }
                let principal_xml = if ace.principal.starts_with("DAV:") {
                    let name = &ace.principal[4..];
                    if !matches!(name, "all" | "authenticated" | "unauthenticated" | "self") { return Err(DavError::InvalidResponse); }
                    format!("<d:{name}/>")
                } else { format!("<d:href>{}</d:href>", escape_xml(&ace.principal)) };
                xml.push_str(&format!("<d:ace><d:principal>{principal_xml}</d:principal><d:{}>", if ace.granted {"grant"} else {"deny"}));
                for name in &ace.privileges {
                    let (namespace, local) = name.strip_prefix('{').and_then(|v| v.split_once('}')).ok_or(DavError::InvalidResponse)?;
                    if local.is_empty() || !local.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') || namespace.is_empty() { return Err(DavError::InvalidResponse); }
                    xml.push_str(&format!("<d:privilege><p:{local} xmlns:p=\"{}\"/></d:privilege>", escape_xml(namespace)));
                }
                xml.push_str(if ace.granted {"</d:grant></d:ace>"} else {"</d:deny></d:ace>"});
            }
            if !inserted { if let Some(privileges) = replacement { xml.push_str(&format!("<d:ace><d:principal><d:href>{}</d:href></d:principal><d:grant>{privileges}</d:grant></d:ace>", escape_xml(principal_url.as_str()))); } }
            xml.push_str("</d:acl>");
            let response = self.http.request(reqwest::Method::from_bytes(b"ACL").map_err(|_| DavError::InvalidResponse)?, target.clone()).bearer_auth(&self.token)
                .header("If", format!("({token})")).header("Cache-Control", "no-cache").header(CONTENT_TYPE, "application/xml; charset=utf-8").body(xml).send()
                .map_err(|_| DavError::Transport("DAV ACL outcome unknown".into()))?;
            if matches!(response.status().as_u16(), 409 | 412) { return Err(DavError::Conflict); }
            if !response.status().is_success() { return Err(DavError::Http(response.status().as_u16())); }
            Ok(())
        })();
        let _ = self.http.request(reqwest::Method::from_bytes(b"UNLOCK").map_err(|_| DavError::InvalidResponse)?, target).bearer_auth(&self.token).header("Lock-Token", token).send();
        result
    }

    /// CalDAV free-busy REPORT for a single collection. This has collection
    /// semantics and is not equivalent to a scheduling-outbox availability
    /// query across all calendars of an attendee.
    pub fn free_busy(&self, scope: &ProviderScope, start: &str, end: &str) -> Result<Vec<u8>, DavError> {
        if scope.service != "calendar" || !utc_time(start) || !utc_time(end) || start >= end { return Err(DavError::InvalidResponse); }
        let collection = self.collection(scope)?;
        let body = format!("<c:free-busy-query xmlns:c=\"urn:ietf:params:xml:ns:caldav\"><c:time-range start=\"{start}\" end=\"{end}\"/></c:free-busy-query>");
        let response = self.http.request(reqwest::Method::from_bytes(b"REPORT").map_err(|_| DavError::InvalidResponse)?, collection).bearer_auth(&self.token)
            .header("Depth", "1").header(CONTENT_TYPE, "application/xml; charset=utf-8").body(body).send()
            .map_err(|_| DavError::Transport("DAV availability request failed".into()))?;
        if response.status().as_u16() != 200 { return Err(DavError::Http(response.status().as_u16())); }
        bounded_body(response)
    }

    /// Ask the server to expand event recurrence for a bounded UTC window.
    /// This is a read-only view; it must never replace the original ICS bytes.
    pub fn expanded_events(&self, scope: &ProviderScope, start: &str, end: &str) -> Result<Vec<ExpandedCalendar>, DavError> {
        self.expanded_components(scope, start, end, "VEVENT")
    }

    /// Expand native tasks independently of events; VTODO and VEVENT may
    /// reside in the same calendar collection.
    pub fn expanded_tasks(&self, scope: &ProviderScope, start: &str, end: &str) -> Result<Vec<ExpandedCalendar>, DavError> {
        self.expanded_components(scope, start, end, "VTODO")
    }

    fn expanded_components(&self, scope: &ProviderScope, start: &str, end: &str, kind: &str) -> Result<Vec<ExpandedCalendar>, DavError> {
        const D: &str = "DAV:";
        const C: &str = "urn:ietf:params:xml:ns:caldav";
        if scope.service != "calendar" || !utc_time(start) || !utc_time(end) || start >= end { return Err(DavError::InvalidResponse); }
        let collection = self.collection(scope)?;
        let body = format!("<c:calendar-query xmlns:c=\"{C}\" xmlns:d=\"DAV:\"><d:prop><d:getetag/><c:calendar-data><c:expand start=\"{start}\" end=\"{end}\"/></c:calendar-data></d:prop><c:filter><c:comp-filter name=\"VCALENDAR\"><c:comp-filter name=\"{kind}\"><c:time-range start=\"{start}\" end=\"{end}\"/></c:comp-filter></c:comp-filter></c:filter></c:calendar-query>");
        let response = self.http.request(reqwest::Method::from_bytes(b"REPORT").map_err(|_| DavError::InvalidResponse)?, collection.clone()).bearer_auth(&self.token)
            .header("Depth", "1").header(CONTENT_TYPE, "application/xml; charset=utf-8").body(body).send()
            .map_err(|_| DavError::Transport("DAV calendar query failed".into()))?;
        if response.status().as_u16() != 207 { return Err(DavError::Http(response.status().as_u16())); }
        let root = parse_tree(&bounded_body(response)?)?;
        if root.namespace != D || root.name != "multistatus" { return Err(DavError::InvalidResponse); }
        let mut out = Vec::new();
        for node in root.children.iter().filter(|n| n.namespace == D && n.name == "response") {
            let href = node.child(D,"href").ok_or(DavError::InvalidResponse)?.text.trim();
            let url = self.resource(&collection,href)?;
            let propstats: Vec<_> = node.children.iter().filter(|n| n.namespace == D && n.name == "propstat").collect();
            if propstats.len() != 1 || status(propstats[0].child(D,"status").ok_or(DavError::InvalidResponse)?.text.trim()) != Some(200) { return Err(DavError::InvalidResponse); }
            let prop = propstats[0].child(D,"prop").ok_or(DavError::InvalidResponse)?;
            let etag = prop.child(D,"getetag").ok_or(DavError::InvalidResponse)?.text.trim().to_owned();
            let calendar_data = prop.child(C,"calendar-data").ok_or(DavError::InvalidResponse)?.text.clone();
            if etag.is_empty() || calendar_data.is_empty() { return Err(DavError::InvalidResponse); }
            out.push(ExpandedCalendar { href:url.to_string(), etag, calendar_data });
        }
        Ok(out)
    }

    /// Advance each completed page transactionally. A denied collection or
    /// resource preserves existing objects and its previous checkpoint.
    pub fn sync_collection(&self, store: &Store, scope: &ProviderScope) -> Result<usize, DavError> {
        let collection = self.collection(scope)?;
        let mut cursor = store.provider_cursor(scope).map_err(|e| DavError::Store(e.to_string()))?.unwrap_or_default();
        let mut full = cursor.is_empty() || scope.service == "calendar" && (store.provider_dav_unindexed_count(scope).map_err(|e| DavError::Store(e.to_string()))? > 0 || store.provider_alarm_unindexed_count(scope).map_err(|e| DavError::Store(e.to_string()))? > 0);
        if full { cursor.clear(); }
        let mut staging = if full { Some(Staging::begin(store, scope)?) } else { None };
        let mut count = 0;
        let mut seen_tokens = HashSet::new();
        loop {
            if !seen_tokens.insert(cursor.clone()) { return Err(DavError::InvalidResponse); }
            let body = format!("<?xml version=\"1.0\"?><d:sync-collection xmlns:d=\"DAV:\"><d:sync-token>{}</d:sync-token><d:sync-level>1</d:sync-level><d:prop><d:getetag/></d:prop></d:sync-collection>", escape_xml(&cursor));
            let response = self.http.request(reqwest::Method::from_bytes(b"REPORT").map_err(|_| DavError::InvalidResponse)?, collection.clone()).bearer_auth(&self.token).header("Depth", "0").header(CONTENT_TYPE, "application/xml; charset=utf-8").body(body).send().map_err(|_| DavError::Transport("DAV report failed".into()))?;
            let code = response.status().as_u16();
            if code == 403 {
                let error = bounded_body(response)?;
                if invalid_sync_token(&error)? {
                    if full { return Err(DavError::InvalidSyncToken); }
                    cursor.clear();
                    full = true;
                    staging = Some(Staging::begin(store, scope)?);
                    seen_tokens.clear();
                    continue;
                }
                store.set_provider_accessible(scope, false).map_err(|e| DavError::Store(e.to_string()))?;
                return Err(DavError::Http(code));
            }
            if code != 207 { return Err(DavError::Http(code)); }
            let bytes = bounded_body(response)?;
            let page = parse_page(&bytes)?;
            let mut changes = Vec::new();
            let mut page_bytes = 0usize;
            let mut more = false;
            let mut seen_collection_self = false;
            for entry in page.entries {
                let candidate = self.checked_from(&collection, &entry.href)?;
                if candidate == collection && entry.status == Some(507) { more = true; continue; }
                if candidate == collection {
                    accept_collection_self(&entry, &mut seen_collection_self)?;
                    continue;
                }
                let url = self.resource(&collection, &entry.href)?;
                let id = url.as_str().to_owned();
                match entry.status {
                    Some(404) => {
                        if let Some(stage) = &staging { stage.remove(&id)?; }
                        else { changes.push(ProviderChange::Destroyed(id)); }
                    },
                    Some(code) if code != 200 => return Err(DavError::Http(code)),
                    _ => {
                        if entry.propstats != 1 || entry.prop_status != Some(200) { return Err(DavError::InvalidResponse); }
                        let etag = entry.etag.ok_or(DavError::InvalidResponse)?;
                        let existing = store.provider_object(scope, &id).map_err(|e| DavError::Store(e.to_string()))?;
                        if existing.as_ref().and_then(|o| o.etag.as_deref()) == Some(etag.as_str()) {
                            if let (Some(stage), Some(object)) = (&staging, existing) { stage.put(&object)?; }
                            continue;
                        }
                        let response = self.http.get(url).bearer_auth(&self.token).send().map_err(|_| DavError::Transport("DAV object fetch failed".into()))?;
                        let code = response.status().as_u16();
                        if code == 403 { store.set_provider_accessible(scope, false).map_err(|e| DavError::Store(e.to_string()))?; return Err(DavError::Http(code)); }
                        if code != 200 { return Err(DavError::Http(code)); }
                        let actual = response.headers().get(ETAG).and_then(|v| v.to_str().ok()).map(str::to_owned).ok_or(DavError::InvalidResponse)?;
                        let raw = bounded_body(response)?;
                        if !full {
                            page_bytes = page_bytes.saturating_add(raw.len());
                            if page_bytes > 64 * 1024 * 1024 { return Err(DavError::InvalidResponse); }
                        }
                        let object = ProviderObject { remote_id: id, etag: Some(actual), raw };
                        if let Some(stage) = &staging { stage.put(&object)?; count += 1; }
                        else { changes.push(ProviderChange::Upsert(object)); }
                    }
                }
            }
            count += changes.len();
            if full {
                if !more {
                    staging.as_mut().ok_or(DavError::InvalidResponse)?.finish(&page.token)?;
                    return Ok(count);
                }
            } else {
                if scope.service == "calendar" {
                    let indexed: Vec<_> = changes.into_iter().map(|change| {
                        let index = match &change { ProviderChange::Upsert(object) => Some(dav_index(&object.raw)), ProviderChange::Destroyed(_) => None };
                        let alarm = match &change { ProviderChange::Upsert(object) => Some(alarm_index(&object.raw)), ProviderChange::Destroyed(_) => None };
                        (change, index, alarm)
                    }).collect();
                    store.apply_provider_dav_page_indexed(scope, &indexed, &page.token).map_err(|e| DavError::Store(e.to_string()))?;
                } else {
                    store.apply_provider_page(scope, &changes, &page.token).map_err(|e| DavError::Store(e.to_string()))?;
                }
            }
            cursor = page.token;
            if !more { return Ok(count); }
        }
    }

    /// Create only when absent, or replace exactly the version the caller read.
    pub fn put(&self, scope: &ProviderScope, href: &str, raw: Vec<u8>, etag: Option<&str>) -> Result<Option<String>, DavError> {
        self.put_with_collection_precondition(scope, href, raw, etag, None)
    }

    /// Execute an operation while the server holds an exclusive depth-infinity
    /// lock on the collection. Lock lifetime is bounded if the client crashes.
    pub fn with_locked_collection<T>(&self, scope: &ProviderScope, operation: impl FnOnce(&str) -> Result<T, DavError>) -> Result<T, DavError> {
        let target = self.collection(scope)?;
        let body = r#"<d:lockinfo xmlns:d="DAV:"><d:lockscope><d:exclusive/></d:lockscope><d:locktype><d:write/></d:locktype></d:lockinfo>"#;
        let response = self.http.request(reqwest::Method::from_bytes(b"LOCK").map_err(|_| DavError::InvalidResponse)?,target.clone()).bearer_auth(&self.token)
            .header("Depth","infinity").header("Timeout","Second-60").header(CONTENT_TYPE,"application/xml; charset=utf-8").body(body).send()
            .map_err(|_| DavError::Transport("DAV lock outcome unknown".into()))?;
        if !response.status().is_success() { return Err(DavError::Http(response.status().as_u16())); }
        let token = response.headers().get("Lock-Token").and_then(|v| v.to_str().ok()).filter(|v| valid_lock_token(v)).ok_or(DavError::InvalidResponse)?.to_owned();
        let result = operation(&token);
        let _ = self.http.request(reqwest::Method::from_bytes(b"UNLOCK").map_err(|_| DavError::InvalidResponse)?,target).bearer_auth(&self.token).header("Lock-Token",token).send();
        result
    }

    /// Replace a resource under an already-held ancestor lock and strong ETag.
    pub fn put_under_lock(&self, scope: &ProviderScope, href: &str, raw: Vec<u8>, etag: &str, token: &str) -> Result<Option<String>, DavError> {
        if etag.is_empty() || !valid_lock_token(token) { return Err(DavError::InvalidResponse); }
        let url = self.resource(&self.collection(scope)?,href)?;
        let media = match scope.service.as_str() {"calendar" => "text/calendar; charset=utf-8", "contacts" => "text/vcard; charset=utf-8", _ => return Err(DavError::InvalidUrl)};
        let response = self.http.put(url).bearer_auth(&self.token).header(CONTENT_TYPE,media).header(IF_MATCH,etag).header("If",format!("({token})")).header("Cache-Control","no-cache").body(raw).send().map_err(|_| DavError::Transport("DAV write outcome unknown".into()))?;
        if matches!(response.status().as_u16(),409 | 412 | 423) { return Err(DavError::Conflict); }
        if !response.status().is_success() { return Err(DavError::Http(response.status().as_u16())); }
        Ok(response.headers().get(ETAG).and_then(|v| v.to_str().ok()).map(str::to_owned))
    }

    /// Delete a resource only while its ancestor lock and ETag both match.
    pub fn delete_under_lock(&self, scope: &ProviderScope, href: &str, etag: &str, token: &str) -> Result<(), DavError> {
        if etag.is_empty() || !valid_lock_token(token) { return Err(DavError::InvalidResponse); }
        let url = self.resource(&self.collection(scope)?,href)?;
        let response = self.http.delete(url).bearer_auth(&self.token).header(IF_MATCH,etag).header("If",format!("({token})")).header("Cache-Control","no-cache").send().map_err(|_| DavError::Transport("DAV delete outcome unknown".into()))?;
        if matches!(response.status().as_u16(),409 | 412 | 423) { return Err(DavError::Conflict); }
        if !response.status().is_success() { return Err(DavError::Http(response.status().as_u16())); }
        Ok(())
    }

    /// A collection sync-token condition protects cross-resource invariants,
    /// such as an acyclic parent graph, against concurrent collection changes.
    pub fn put_with_collection_precondition(&self, scope: &ProviderScope, href: &str, raw: Vec<u8>, etag: Option<&str>, token: Option<&str>) -> Result<Option<String>, DavError> {
        let collection = self.collection(scope)?;
        let url = self.resource(&collection, href)?;
        let media = match scope.service.as_str() { "calendar" | "tasks" => "text/calendar; charset=utf-8", "contacts" => "text/vcard; charset=utf-8", _ => return Err(DavError::InvalidUrl) };
        let req = self.http.put(url).bearer_auth(&self.token).header(CONTENT_TYPE, media).body(raw);
        let mut req = match etag { Some(tag) if !tag.is_empty() => req.header(IF_MATCH, tag), None => req.header(IF_NONE_MATCH, "*"), _ => return Err(DavError::InvalidResponse) };
        if let Some(token) = token {
            if token.len() > 4096 || Url::parse(token).is_err() || token.chars().any(|c| c.is_control() || c.is_whitespace() || matches!(c, '<' | '>' | '(' | ')')) { return Err(DavError::InvalidResponse); }
            req = req.header("If", format!("<{collection}> (<{token}>)"));
        }
        let response = req.send().map_err(|_| DavError::Transport("DAV write outcome unknown".into()))?;
        let code = response.status().as_u16();
        if code == 412 { return Err(DavError::Conflict); }
        if !response.status().is_success() { return Err(DavError::Http(code)); }
        Ok(response.headers().get(ETAG).and_then(|v| v.to_str().ok()).map(str::to_owned))
    }

    pub fn delete(&self, scope: &ProviderScope, href: &str, etag: &str) -> Result<(), DavError> {
        if etag.is_empty() { return Err(DavError::InvalidResponse); }
        let url = self.resource(&self.collection(scope)?, href)?;
        let response = self.http.delete(url).bearer_auth(&self.token).header(IF_MATCH, etag).send().map_err(|_| DavError::Transport("DAV delete outcome unknown".into()))?;
        let code = response.status().as_u16();
        if code == 412 { return Err(DavError::Conflict); }
        if !response.status().is_success() { return Err(DavError::Http(code)); }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn personal_home_accepts_only_collection_slash_and_rebases_relative_principal() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = format!("http://{}/",listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            for n in 0..2 {
                let (mut socket,_) = listener.accept().unwrap(); let mut request=Vec::new(); let mut buffer=[0u8;4096];
                loop { let read=socket.read(&mut buffer).unwrap(); assert_ne!(read,0); request.extend_from_slice(&buffer[..read]);
                    if let Some(end)=request.windows(4).position(|w| w==b"\r\n\r\n") {
                        let headers=String::from_utf8_lossy(&request[..end]);
                        let length=headers.lines().find_map(|line| line.to_ascii_lowercase().strip_prefix("content-length:").and_then(|x| x.trim().parse::<usize>().ok())).unwrap_or(0);
                        if request.len()>=end+4+length { break; }
                    }
                }
                let request=String::from_utf8(request).unwrap();
                let body=if n==0 {
                    assert!(request.starts_with("PROPFIND /dav/cal "));
                    r#"<d:multistatus xmlns:d="DAV:"><d:response><d:href>/dav/cal/</d:href><d:propstat><d:prop><d:current-user-principal><d:href>../pal/me</d:href></d:current-user-principal></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>"#
                } else {
                    assert!(request.starts_with("PROPFIND /dav/pal/me "));
                    r#"<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:response><d:href>/dav/pal/me</d:href><d:propstat><d:prop><c:calendar-home-set><d:href>/dav/cal/me/</d:href></c:calendar-home-set></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>"#
                };
                write!(socket,"HTTP/1.1 207 Multi-Status\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
            }
        });
        let client=super::DavClient {http:reqwest::blocking::Client::builder().no_proxy().timeout(std::time::Duration::from_secs(5)).build().unwrap(),origin:url::Url::parse(&origin).unwrap(),token:"fixture".into(),trusted_public_origin:None};
        let home=client.personal_home("/dav/cal","calendar").unwrap();
        assert_eq!(home.home_url(),format!("{origin}dav/cal/me/"));
        assert_eq!(home.principal_url(),format!("{origin}dav/pal/me"));
        let target=url::Url::parse(&format!("{origin}dav/cal")).unwrap();
        assert!(!super::dav_response_matches(&target,&url::Url::parse(&format!("{origin}dav/card/")).unwrap(),true));
        assert!(!super::dav_response_matches(&target,&url::Url::parse(&format!("{origin}dav/cal/")).unwrap(),false));
        server.join().unwrap();
    }

    #[test]
    fn personal_home_rejects_ambiguous_discovery_responses() {
        use std::io::{Read, Write};
        let listener=std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let origin=format!("http://{}/",listener.local_addr().unwrap());
        let server=std::thread::spawn(move || {
            let (mut socket,_)=listener.accept().unwrap(); let mut bytes=Vec::new(); let mut buf=[0;4096];
            loop { let n=socket.read(&mut buf).unwrap(); assert_ne!(n,0); bytes.extend_from_slice(&buf[..n]);
                if let Some(end)=bytes.windows(4).position(|w| w==b"\r\n\r\n") {
                    let headers=String::from_utf8_lossy(&bytes[..end]); let size=headers.lines().find_map(|line| line.to_ascii_lowercase().strip_prefix("content-length:").and_then(|v| v.trim().parse::<usize>().ok())).unwrap_or(0);
                    if bytes.len()>=end+4+size { break; }
                }
            }
            let body=r#"<d:multistatus xmlns:d="DAV:"><d:response><d:href>/dav/cal</d:href></d:response><d:response><d:href>/dav/cal/</d:href></d:response></d:multistatus>"#;
            write!(socket,"HTTP/1.1 207 Multi-Status\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
        });
        let client=super::DavClient {http:reqwest::blocking::Client::builder().no_proxy().timeout(std::time::Duration::from_secs(5)).build().unwrap(),origin:url::Url::parse(&origin).unwrap(),token:"fixture".into(),trusted_public_origin:None};
        assert!(matches!(client.personal_home("/dav/cal","calendar"),Err(super::DavError::InvalidResponse)));
        server.join().unwrap();
    }
    #[test]
    fn recovered_collection_creation_is_conditional_and_marked_for_each_dav_type() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = format!("http://{}/",listener.local_addr().unwrap());
        let operation = "0123456789abcdef0123456789abcdef";
        let server = std::thread::spawn(move || {
            for n in 0..4 {
                let (mut socket,_) = listener.accept().unwrap(); let mut request = Vec::new(); let mut buffer=[0u8;4096];
                loop {
                    let read=socket.read(&mut buffer).unwrap(); assert_ne!(read,0); request.extend_from_slice(&buffer[..read]);
                    if let Some(end)=request.windows(4).position(|w| w==b"\r\n\r\n") {
                        let headers=String::from_utf8_lossy(&request[..end]);
                        let length=headers.lines().find_map(|line| line.to_ascii_lowercase().strip_prefix("content-length:").and_then(|x| x.trim().parse::<usize>().ok())).unwrap_or(0);
                        if request.len()>=end+4+length {break;}
                    }
                }
                let request=String::from_utf8(request).unwrap();
                let (status,body) = if n%2==0 {
                    assert!(request.starts_with("PROPFIND /home/"));
                    ("207 Multi-Status",r#"<d:multistatus xmlns:d="DAV:"><d:response><d:href>/home/</d:href><d:propstat><d:prop><d:current-user-privilege-set><d:privilege><d:bind/></d:privilege></d:current-user-privilege-set></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>"#.to_owned())
                } else {
                    assert!(request.starts_with(if n==1 {"MKCALENDAR "} else {"MKCOL "}));
                    assert!(request.to_ascii_lowercase().contains("if-none-match: *"));
                    assert!(request.contains(operation)); assert!(request.contains("<d:displayname>Recovered</d:displayname>"));
                    ("201 Created",String::new())
                };
                write!(socket,"HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
            }
        });
        let client=super::DavClient {http:reqwest::blocking::Client::builder().no_proxy().build().unwrap(),origin:url::Url::parse(&origin).unwrap(),token:"fixture".into(),trusted_public_origin:None};
        for service in ["calendar","contacts"] {
            let home=super::PersonalDavHome {home:url::Url::parse(&format!("{origin}home/")).unwrap(),principal:url::Url::parse(&format!("{origin}principal/")).unwrap(),service:service.into()};
            client.create_recovered_collection(&home,operation).unwrap();
        }
        server.join().unwrap();
    }
    #[test]
    fn recovered_collection_readback_requires_matching_marker_owner_and_privileges() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = format!("http://{}/", listener.local_addr().unwrap());
        let operation = "0123456789abcdef0123456789abcdef";
        let server = std::thread::spawn(move || {
            for n in 0..3 {
                let (mut socket,_) = listener.accept().unwrap();
                let mut request = Vec::new(); let mut buffer = [0u8; 4096];
                loop {
                    let read = socket.read(&mut buffer).unwrap(); assert_ne!(read,0); request.extend_from_slice(&buffer[..read]);
                    if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&request[..end]);
                        let length = headers.lines().find_map(|line| line.to_ascii_lowercase().strip_prefix("content-length:").and_then(|x| x.trim().parse::<usize>().ok())).unwrap_or(0);
                        if request.len() >= end + 4 + length { break; }
                    }
                }
                let marker = if n == 1 { "ffffffffffffffffffffffffffffffff" } else { operation };
                let body = if n == 2 { String::new() } else { format!(r#"<d:multistatus xmlns:d="DAV:" xmlns:a="urn:ietf:params:xml:ns:carddav" xmlns:i="urn:isyncyou:recovery"><d:response><d:href>/home/wp16-recovered-{operation}/</d:href><d:propstat><d:prop><d:displayname>Recovered</d:displayname><d:resourcetype><d:collection/><a:addressbook/></d:resourcetype><d:owner><d:href>/principal/</d:href></d:owner><d:current-user-privilege-set><d:privilege><d:all/></d:privilege></d:current-user-privilege-set><i:operation>{marker}</i:operation></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>"#) };
                let status = if n == 2 { "404 Not Found" } else { "207 Multi-Status" };
                write!(socket,"HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
            }
        });
        let client = super::DavClient {http:reqwest::blocking::Client::builder().no_proxy().build().unwrap(),origin:url::Url::parse(&origin).unwrap(),token:"fixture".into(),trusted_public_origin:None};
        let home = super::PersonalDavHome {home:url::Url::parse(&format!("{origin}home/")).unwrap(),principal:url::Url::parse(&format!("{origin}principal/")).unwrap(),service:"contacts".into()};
        assert!(client.read_recovered_collection(&home,operation).unwrap());
        assert!(client.read_recovered_collection(&home,operation).is_err());
        assert!(!client.read_recovered_collection(&home,operation).unwrap());
        assert!(home.recovered_target("../../foreign").is_err());
        server.join().unwrap();
    }
    #[test]
    fn collection_self_requires_exact_successful_propstat() {
        fn response(inner: &str) -> super::Resource {
            let xml = format!("<d:multistatus xmlns:d=\"DAV:\"><d:response>{inner}</d:response><d:sync-token>next</d:sync-token></d:multistatus>");
            super::parse_page(xml.as_bytes()).unwrap().entries.remove(0)
        }
        let ok = "<d:href>/cal/</d:href><d:propstat><d:prop><d:getetag>&quot;v1&quot;</d:getetag></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>";
        assert!(super::valid_collection_self(&response(ok)));
        assert!(super::valid_collection_self(&response(&ok.replace("/cal/", "/card/"))));
        let mut seen = false;
        assert!(super::accept_collection_self(&response(ok), &mut seen).is_ok());
        assert!(super::accept_collection_self(&response(ok), &mut seen).is_err());
        for invalid in [
            ok.replace("200 OK", "403 Forbidden"),
            ok.replace("200 OK", "404 Not Found"),
            "<d:href>/cal/</d:href><d:status>HTTP/1.1 403 Forbidden</d:status>".into(),
            "<d:href>/cal/</d:href><d:status>HTTP/1.1 404 Not Found</d:status>".into(),
            format!("{ok}<d:status>HTTP/1.1 200 OK</d:status>"),
            ok.replace("<d:getetag>&quot;v1&quot;</d:getetag>", "<d:getetag/>"),
            ok.replace("</d:prop>", "<d:extra/></d:prop>"),
            format!("{ok}<d:href>/cal/</d:href>"),
        ] { assert!(!super::valid_collection_self(&response(&invalid))); }
    }
    use super::*;

    #[test]
    fn acl_update_locks_checks_snapshot_and_preserves_other_editable_entries() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = format!("http://{}/", listener.local_addr().unwrap());
        let other = format!("{}users/other", origin);
        let selected = format!("{}users/selected", origin);
        let server = std::thread::spawn(move || {
            for index in 0..6 {
                let (mut socket, _) = listener.accept().unwrap();
                let mut request = Vec::new(); let mut buf = [0u8; 2048];
                loop {
                    let n = socket.read(&mut buf).unwrap(); assert_ne!(n, 0); request.extend_from_slice(&buf[..n]);
                    if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&request[..end]);
                        let length = headers.lines().find_map(|line| line.to_ascii_lowercase().strip_prefix("content-length:").and_then(|n| n.trim().parse::<usize>().ok())).unwrap_or(0);
                        if request.len() >= end + 4 + length { break; }
                    }
                }
                let wire = String::from_utf8_lossy(&request);
                let (status, headers, body) = match index {
                    0 | 1 => ("207 Multi-Status", "", "<d:multistatus xmlns:d=\"DAV:\"><d:response><d:href>/cal/</d:href><d:propstat><d:prop><d:current-user-privilege-set><d:privilege><d:all/></d:privilege></d:current-user-privilege-set></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>"),
                    2 => { assert!(wire.starts_with("LOCK ")); ("200 OK", "Lock-Token: <opaquelocktoken:fixture>\r\n", "") },
                    3 => ("207 Multi-Status", "", "<d:multistatus xmlns:d=\"DAV:\"><d:response><d:href>/cal/</d:href><d:propstat><d:prop><d:acl><d:ace><d:principal><d:href>/users/other</d:href></d:principal><d:grant><d:privilege><d:read/></d:privilege></d:grant></d:ace><d:ace><d:principal><d:all/></d:principal><d:deny><d:privilege><d:write/></d:privilege></d:deny><d:protected/></d:ace></d:acl></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>"),
                    4 => { assert!(wire.starts_with("ACL ")); assert!(wire.contains("(\u{3c}opaquelocktoken:fixture\u{3e})")); assert!(wire.contains("users/other")); assert!(wire.contains("users/selected")); assert!(!wire.contains("<d:protected")); ("200 OK", "", "") },
                    _ => { assert!(wire.starts_with("UNLOCK ")); ("204 No Content", "", "") },
                };
                write!(socket, "HTTP/1.1 {status}\r\nContent-Length: {}\r\n{headers}Connection: close\r\n\r\n{body}", body.len()).unwrap();
            }
        });
        let client = DavClient { http: Client::builder().no_proxy().timeout(std::time::Duration::from_secs(5)).redirect(reqwest::redirect::Policy::none()).build().unwrap(), origin: Url::parse(&origin).unwrap(), token: "fixture".into(), trusted_public_origin:None };
        let scope = ProviderScope { account_id:"fixture".into(), origin, service:"calendar".into(), collection:"/cal/".into() };
        let expected = vec![AclEntry {principal:other,granted:true,privileges:vec!["{DAV:}read".into()],inherited:false,protected:false}, AclEntry {principal:"DAV:all".into(),granted:false,privileges:vec!["{DAV:}write".into()],inherited:false,protected:true}];
        client.update_acl_locked(&scope, &expected, &selected, Some("read")).unwrap();
        server.join().unwrap();
    }

    #[test]
    fn calendar_addresses_are_taken_from_authenticated_principal() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = format!("http://{}/",listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            for index in 0..2 {
                let (mut socket,_) = listener.accept().unwrap();
                let mut data = Vec::new(); let mut buf = [0u8;2048];
                loop {
                    let n = socket.read(&mut buf).unwrap(); assert_ne!(n,0); data.extend_from_slice(&buf[..n]);
                    if let Some(end) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&data[..end]);
                        let length = headers.lines().find_map(|line| line.to_ascii_lowercase().strip_prefix("content-length:").and_then(|n| n.trim().parse::<usize>().ok())).unwrap_or(0);
                        if data.len() >= end + 4 + length { break; }
                    }
                }
                let body = if index == 0 { "<d:multistatus xmlns:d=\"DAV:\"><d:response><d:href>/dav/</d:href><d:propstat><d:prop><d:current-user-principal><d:href>/principals/me/</d:href></d:current-user-principal></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>" } else { "<d:multistatus xmlns:d=\"DAV:\" xmlns:c=\"urn:ietf:params:xml:ns:caldav\"><d:response><d:href>/principals/me/</d:href><d:propstat><d:prop><c:calendar-user-address-set><d:href>mailto:me@example.test</d:href></c:calendar-user-address-set></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>" };
                write!(socket,"HTTP/1.1 207 Multi-Status\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
            }
        });
        let client = DavClient {http:Client::builder().no_proxy().timeout(std::time::Duration::from_secs(5)).redirect(reqwest::redirect::Policy::none()).build().unwrap(),origin:Url::parse(&origin).unwrap(),token:"fixture".into(),trusted_public_origin:None};
        assert_eq!(client.calendar_user_addresses("/dav/").unwrap(),vec!["mailto:me@example.test"]);
        server.join().unwrap();
    }

    #[test]
    fn collection_lock_put_uses_lock_token_and_resource_etag() {
        use std::io::{Read,Write}; use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = format!("http://{}/",listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            for index in 0..4 {
                let (mut socket,_) = listener.accept().unwrap(); let mut data=Vec::new(); let mut buf=[0u8;2048];
                loop { let n=socket.read(&mut buf).unwrap(); assert_ne!(n,0); data.extend_from_slice(&buf[..n]);
                    if let Some(end)=data.windows(4).position(|w|w==b"\r\n\r\n") { let headers=String::from_utf8_lossy(&data[..end]); let length=headers.lines().find_map(|line|line.to_ascii_lowercase().strip_prefix("content-length:").and_then(|v|v.trim().parse::<usize>().ok())).unwrap_or(0); if data.len()>=end+4+length {break;} }
                }
                let wire=String::from_utf8_lossy(&data).to_ascii_lowercase();
                let (status,headers)=match index {
                    0 => {assert!(wire.starts_with("lock ")); assert!(wire.contains("depth: infinity")); ("200 OK","Lock-Token: <opaquelocktoken:fixture>\r\n")},
                    1 => {assert!(wire.starts_with("put ")); assert!(wire.contains("if: (<opaquelocktoken:fixture>)")); assert!(wire.contains("if-match: \"v1\"")); ("204 No Content","")},
                    2 => {assert!(wire.starts_with("delete ")); assert!(wire.contains("if: (<opaquelocktoken:fixture>)")); assert!(wire.contains("if-match: \"v1\"")); ("204 No Content","")},
                    _ => {assert!(wire.starts_with("unlock ")); ("204 No Content","")},
                };
                write!(socket,"HTTP/1.1 {status}\r\nContent-Length: 0\r\n{headers}Connection: close\r\n\r\n").unwrap();
            }
        });
        let client=DavClient {http:Client::builder().no_proxy().timeout(std::time::Duration::from_secs(5)).redirect(reqwest::redirect::Policy::none()).build().unwrap(),origin:Url::parse(&origin).unwrap(),token:"fixture".into(),trusted_public_origin:None};
        let scope=ProviderScope {account_id:"fixture".into(),origin,service:"calendar".into(),collection:"/cal/".into()};
        assert!(!valid_lock_token("<opaquelocktoken:fixture><urn:other>"));
        client.with_locked_collection(&scope,|token| {client.put_under_lock(&scope,"task.ics",b"fixture".to_vec(),"\"v1\"",token)?; client.delete_under_lock(&scope,"task.ics","\"v1\"",token)}).unwrap();
        server.join().unwrap();
    }

    #[test]
    fn collection_lock_releases_after_failed_graph_check() {
        use std::io::{Read,Write}; use std::net::TcpListener;
        let listener=TcpListener::bind("127.0.0.1:0").unwrap();
        let origin=format!("http://{}/",listener.local_addr().unwrap());
        let server=std::thread::spawn(move || {
            for index in 0..2 {
                let (mut socket,_)=listener.accept().unwrap(); let mut data=Vec::new(); let mut buf=[0u8;2048];
                loop { let n=socket.read(&mut buf).unwrap(); assert_ne!(n,0); data.extend_from_slice(&buf[..n]);
                    if let Some(end)=data.windows(4).position(|w|w==b"\r\n\r\n") { let headers=String::from_utf8_lossy(&data[..end]); let length=headers.lines().find_map(|line|line.to_ascii_lowercase().strip_prefix("content-length:").and_then(|v|v.trim().parse::<usize>().ok())).unwrap_or(0); if data.len()>=end+4+length {break;} }
                }
                let wire=String::from_utf8_lossy(&data); let (status,headers)=if index == 0 {assert!(wire.starts_with("LOCK ")); ("200 OK","Lock-Token: <opaquelocktoken:fixture>\r\n")} else {assert!(wire.starts_with("UNLOCK ")); ("204 No Content","")};
                write!(socket,"HTTP/1.1 {status}\r\nContent-Length: 0\r\n{headers}Connection: close\r\n\r\n").unwrap();
            }
        });
        let client=DavClient {http:Client::builder().no_proxy().timeout(std::time::Duration::from_secs(5)).redirect(reqwest::redirect::Policy::none()).build().unwrap(),origin:Url::parse(&origin).unwrap(),token:"fixture".into(),trusted_public_origin:None};
        let scope=ProviderScope {account_id:"fixture".into(),origin,service:"calendar".into(),collection:"/cal/".into()};
        assert!(matches!(client.with_locked_collection::<()>(&scope,|_| Err(DavError::Conflict)),Err(DavError::Conflict)));
        server.join().unwrap();
    }

    #[test]
    fn sync_page_distinguishes_deleted_resources_and_page_boundary() {
        let xml = br#"<d:multistatus xmlns:d="DAV:"><d:response><d:href>/cal/a.ics</d:href><d:propstat><d:prop><d:getetag>&quot;v1&quot;</d:getetag></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response><d:response><d:href>/cal/b.ics</d:href><d:status>HTTP/1.1 404 Not Found</d:status></d:response><d:response><d:href>/cal/</d:href><d:status>HTTP/1.1 507 Insufficient Storage</d:status></d:response><d:sync-token>https://example.test/token?a=1&amp;b=2</d:sync-token></d:multistatus>"#;
        let page = parse_page(xml).unwrap();
        assert_eq!(page.token, "https://example.test/token?a=1&b=2");
        assert_eq!(page.entries.len(), 3);
        assert_eq!(page.entries[0].etag.as_deref(), Some("\"v1\""));
        assert_eq!(page.entries[1].status, Some(404));
        assert_eq!(page.entries[2].status, Some(507));
    }

    #[test]
    fn foreign_namespace_and_unsafe_destinations_are_rejected() {
        assert!(parse_page(br#"<multistatus xmlns="urn:foreign"><sync-token>fake</sync-token></multistatus>"#).is_err());
        let client = DavClient::new("https://example.test/", "token".into()).unwrap();
        let collection = Url::parse("https://example.test/cal/").unwrap();
        assert!(client.resource(&collection, "https://other.test/cal/a").is_err());
        assert!(client.checked_from(&collection, "https://other.test/cal/").is_err());
        assert!(client.resource(&collection, "../other/a").is_err());
        assert!(client.resource(&collection, "a.ics").is_ok());
        assert!(invalid_sync_token(br#"<d:error xmlns:d="DAV:"><d:valid-sync-token/></d:error>"#).unwrap());
        assert!(!invalid_sync_token(br#"<e:error xmlns:e="urn:other"><e:valid-sync-token/></e:error>"#).unwrap());
        assert!(LoopbackDavReader::new("http://127.0.0.1:8080/", "fixture".into()).is_ok());
        assert!(LoopbackDavReader::new("http://localhost:8080/", "fixture".into()).is_err());
        assert!(LoopbackDavReader::new("http://192.0.2.1:8080/", "fixture".into()).is_err());
        assert!(LoopbackDavReader::new("http://127.0.0.1:8080/path", "fixture".into()).is_err());
        let mapped = LoopbackDavReader::new_with_public_origin("http://127.0.0.1:8080/", "fixture".into(), "https://example.test/").unwrap();
        let mapped_url = mapped.0.checked_url("https://example.test/dav/cal/a%40example.test/").unwrap();
        assert_eq!(mapped_url.as_str(), "http://127.0.0.1:8080/dav/cal/a%40example.test/");
        assert!(mapped.0.checked_url("https://other.test/dav/cal/").is_err());
        assert!(mapped.0.checked_url("https://example.test:444/dav/cal/").is_err());
        assert!(mapped.0.checked_url("https://example.test/dav/cal/?secret=x").is_err());
    }

    #[test]
    fn discovery_tree_preserves_namespaces_and_escaped_values() {
        let xml = br#"<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:response><d:href>/home/</d:href><d:propstat><d:prop><d:displayname>A &amp; B</d:displayname><d:resourcetype><d:collection/><c:calendar/></d:resourcetype></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>"#;
        let root = parse_tree(xml).unwrap();
        let response = root.child("DAV:", "response").unwrap();
        let prop = response.child("DAV:", "propstat").unwrap().child("DAV:", "prop").unwrap();
        assert_eq!(prop.child("DAV:", "displayname").unwrap().text, "A & B");
        assert!(prop.child("DAV:", "resourcetype").unwrap().child("urn:ietf:params:xml:ns:caldav", "calendar").is_some());
        assert!(parse_tree(br#"<!DOCTYPE x [<!ENTITY e "bad">]><x>&e;</x>"#).is_err());
    }

    #[test]
    fn sync_uses_conditional_cache_and_preserves_raw_bytes() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = format!("http://{}/", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            for index in 0..3 {
                let (mut socket, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                let mut buffer = [0u8; 2048];
                loop {
                    let n = socket.read(&mut buffer).unwrap();
                    assert_ne!(n, 0);
                    request.extend_from_slice(&buffer[..n]);
                    if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&request[..end]);
                        let length = headers.lines().find_map(|line| line.to_ascii_lowercase().strip_prefix("content-length:").and_then(|n| n.trim().parse::<usize>().ok())).unwrap_or(0);
                        if request.len() >= end + 4 + length { break; }
                    }
                }
                let first = String::from_utf8_lossy(&request);
                let body = if index == 1 {
                    assert!(first.starts_with("GET /cal/a.ics "));
                    "BEGIN:VCALENDAR\r\nX-CUSTOM;FOO=BAR:value\r\nEND:VCALENDAR\r\n".to_owned()
                } else {
                    assert!(first.starts_with("REPORT /cal/ "));
                    format!("<d:multistatus xmlns:d=\"DAV:\"><d:response><d:href>/cal/</d:href><d:propstat><d:prop><d:getetag>&quot;collection&quot;</d:getetag></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response><d:response><d:href>/cal/a.ics</d:href><d:propstat><d:prop><d:getetag>&quot;v1&quot;</d:getetag></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response><d:sync-token>token-{index}</d:sync-token></d:multistatus>")
                };
                let etag = if index == 1 { "ETag: \"v1\"\r\n" } else { "" };
                let status = if index == 1 { "200 OK" } else { "207 Multi-Status" };
                write!(socket, "HTTP/1.1 {status}\r\nContent-Length: {}\r\n{etag}Connection: close\r\n\r\n{body}", body.len()).unwrap();
            }
        });
        // Plain HTTP is permitted only in this isolated loopback fixture.
        let client = DavClient { http: Client::builder().no_proxy().timeout(std::time::Duration::from_secs(5)).redirect(reqwest::redirect::Policy::none()).build().unwrap(), origin: Url::parse(&origin).unwrap(), token: "fixture".into(), trusted_public_origin:None };
        let scope = ProviderScope { account_id: "fixture".into(), origin: origin.clone(), service: "calendar".into(), collection: "/cal/".into() };
        let store = Store::open_in_memory().unwrap();
        assert_eq!(client.sync_collection(&store, &scope).unwrap(), 1);
        assert_eq!(client.sync_collection(&store, &scope).unwrap(), 0);
        let object = store.provider_objects(&scope).unwrap().pop().unwrap();
        assert_eq!(object.raw, b"BEGIN:VCALENDAR\r\nX-CUSTOM;FOO=BAR:value\r\nEND:VCALENDAR\r\n");
        assert_eq!(object.etag.as_deref(), Some("\"v1\""));
        assert_eq!(store.provider_cursor(&scope).unwrap().as_deref(), Some("token-2"));
        server.join().unwrap();
    }

    #[test]
    fn stale_etag_is_a_conflict_and_does_not_retry() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = format!("http://{}/", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buffer = [0u8; 2048];
            loop {
                let n = socket.read(&mut buffer).unwrap();
                assert_ne!(n, 0);
                request.extend_from_slice(&buffer[..n]);
                if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request[..end]);
                    let length = headers.lines().find_map(|line| line.to_ascii_lowercase().strip_prefix("content-length:").and_then(|n| n.trim().parse::<usize>().ok())).unwrap_or(0);
                    if request.len() >= end + 4 + length { break; }
                }
            }
            let headers = String::from_utf8_lossy(&request).to_ascii_lowercase();
            assert!(headers.starts_with("put /cal/a.ics "));
            assert!(headers.contains("if-match: \"old\"\r\n"));
            assert!(headers.contains("if: <http://127.0.0.1:"));
            assert!(headers.contains("(<https://example.test/token/1>)"));
            write!(socket, "HTTP/1.1 412 Precondition Failed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
        });
        let client = DavClient { http: Client::builder().no_proxy().timeout(std::time::Duration::from_secs(5)).redirect(reqwest::redirect::Policy::none()).build().unwrap(), origin: Url::parse(&origin).unwrap(), token: "fixture".into(), trusted_public_origin:None };
        let scope = ProviderScope { account_id: "fixture".into(), origin, service: "calendar".into(), collection: "/cal/".into() };
        assert!(matches!(client.put_with_collection_precondition(&scope, "a.ics", b"fixture".to_vec(), Some("\"old\""), Some("https://example.test/token/1")), Err(DavError::Conflict)));
        server.join().unwrap();
    }

    #[test]
    fn denied_collection_keeps_cached_data_and_cursor() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = format!("http://{}/", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buffer = [0u8; 2048];
            loop {
                let n = socket.read(&mut buffer).unwrap();
                assert_ne!(n, 0);
                request.extend_from_slice(&buffer[..n]);
                if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request[..end]);
                    let length = headers.lines().find_map(|line| line.to_ascii_lowercase().strip_prefix("content-length:").and_then(|n| n.trim().parse::<usize>().ok())).unwrap_or(0);
                    if request.len() >= end + 4 + length { break; }
                }
            }
            let body = "<d:error xmlns:d=\"DAV:\"><d:need-privileges/></d:error>";
            write!(socket, "HTTP/1.1 403 Forbidden\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        });
        let client = DavClient { http: Client::builder().no_proxy().timeout(std::time::Duration::from_secs(5)).redirect(reqwest::redirect::Policy::none()).build().unwrap(), origin: Url::parse(&origin).unwrap(), token: "fixture".into(), trusted_public_origin:None };
        let scope = ProviderScope { account_id: "fixture".into(), origin, service: "calendar".into(), collection: "/cal/".into() };
        let store = Store::open_in_memory().unwrap();
        let object = ProviderObject { remote_id: "saved".into(), etag: Some("old".into()), raw: b"original".to_vec() };
        store.apply_provider_page(&scope, &[ProviderChange::Upsert(object.clone())], "old-cursor").unwrap();
        assert!(matches!(client.sync_collection(&store, &scope), Err(DavError::Http(403))));
        assert_eq!(store.provider_accessible(&scope).unwrap(), Some(false));
        assert_eq!(store.provider_cursor(&scope).unwrap().as_deref(), Some("old-cursor"));
        assert_eq!(store.provider_object(&scope, "saved").unwrap(), Some(object));
        server.join().unwrap();
    }

    #[test]
    fn discovery_rejects_cross_origin_redirect() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = format!("http://{}/", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buffer = [0u8; 2048];
            loop {
                let n = socket.read(&mut buffer).unwrap();
                assert_ne!(n, 0);
                request.extend_from_slice(&buffer[..n]);
                if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request[..end]);
                    let length = headers.lines().find_map(|line| line.to_ascii_lowercase().strip_prefix("content-length:").and_then(|n| n.trim().parse::<usize>().ok())).unwrap_or(0);
                    if request.len() >= end + 4 + length { break; }
                }
            }
            write!(socket, "HTTP/1.1 302 Found\r\nLocation: https://other.invalid/dav/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
        });
        let client = DavClient { http: Client::builder().no_proxy().timeout(std::time::Duration::from_secs(5)).redirect(reqwest::redirect::Policy::none()).build().unwrap(), origin: Url::parse(&origin).unwrap(), token: "fixture".into(), trusted_public_origin:None };
        assert!(matches!(client.discover("/.well-known/caldav"), Err(DavError::Http(302))));
        server.join().unwrap();
    }

    #[test]
    fn free_busy_reports_collection_members_at_depth_one() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = format!("http://{}/", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = Vec::new(); let mut buf = [0u8; 2048];
            loop {
                let n = socket.read(&mut buf).unwrap(); assert_ne!(n, 0); request.extend_from_slice(&buf[..n]);
                if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request[..end]);
                    let length = headers.lines().find_map(|line| line.to_ascii_lowercase().strip_prefix("content-length:").and_then(|n| n.trim().parse::<usize>().ok())).unwrap_or(0);
                    if request.len() >= end + 4 + length { break; }
                }
            }
            let wire = String::from_utf8_lossy(&request).to_ascii_lowercase();
            assert!(wire.starts_with("report /cal/ "));
            assert!(wire.contains("depth: 1\r\n"));
            assert!(wire.contains("free-busy-query"));
            let body = "BEGIN:VCALENDAR\r\nBEGIN:VFREEBUSY\r\nEND:VFREEBUSY\r\nEND:VCALENDAR\r\n";
            write!(socket, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        });
        let client = DavClient { http: Client::builder().no_proxy().timeout(std::time::Duration::from_secs(5)).redirect(reqwest::redirect::Policy::none()).build().unwrap(), origin: Url::parse(&origin).unwrap(), token: "fixture".into(), trusted_public_origin:None };
        let scope = ProviderScope { account_id:"fixture".into(), origin, service:"calendar".into(), collection:"/cal/".into() };
        let body = client.free_busy(&scope,"20261028T080000Z","20261028T090000Z").unwrap();
        assert!(body.windows(9).any(|w| w == b"VFREEBUSY"));
        assert!(client.free_busy(&scope,"bad","20261028T090000Z").is_err());
        server.join().unwrap();
    }

    #[test]
    fn interrupted_invalid_cursor_rebuild_preserves_previous_scope() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = format!("http://{}/", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            for index in 0..4 {
                let (mut socket, _) = listener.accept().unwrap();
                let mut request = Vec::new(); let mut buf = [0u8; 2048];
                loop {
                    let n = socket.read(&mut buf).unwrap(); assert_ne!(n, 0); request.extend_from_slice(&buf[..n]);
                    if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&request[..end]);
                        let length = headers.lines().find_map(|line| line.to_ascii_lowercase().strip_prefix("content-length:").and_then(|n| n.trim().parse::<usize>().ok())).unwrap_or(0);
                        if request.len() >= end + 4 + length { break; }
                    }
                }
                let (status, headers, body) = match index {
                    0 => ("403 Forbidden", "", "<d:error xmlns:d=\"DAV:\"><d:valid-sync-token/></d:error>"),
                    1 => ("207 Multi-Status", "", "<d:multistatus xmlns:d=\"DAV:\"><d:response><d:href>/cal/new.ics</d:href><d:propstat><d:prop><d:getetag>&quot;v1&quot;</d:getetag></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response><d:response><d:href>/cal/</d:href><d:status>HTTP/1.1 507 Insufficient Storage</d:status></d:response><d:sync-token>partial</d:sync-token></d:multistatus>"),
                    2 => ("200 OK", "ETag: \"v1\"\r\n", "BEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n"),
                    _ => ("403 Forbidden", "", "<d:error xmlns:d=\"DAV:\"><d:need-privileges/></d:error>"),
                };
                write!(socket, "HTTP/1.1 {status}\r\nContent-Length: {}\r\n{headers}Connection: close\r\n\r\n{body}", body.len()).unwrap();
            }
        });
        let client = DavClient { http: Client::builder().no_proxy().timeout(std::time::Duration::from_secs(5)).redirect(reqwest::redirect::Policy::none()).build().unwrap(), origin: Url::parse(&origin).unwrap(), token: "fixture".into(), trusted_public_origin:None };
        let scope = ProviderScope { account_id:"fixture".into(), origin, service:"calendar".into(), collection:"/cal/".into() };
        let store = Store::open_in_memory().unwrap();
        let old = ProviderObject { remote_id:"old".into(), etag:Some("\"old\"".into()), raw:b"preserved".to_vec() };
        // The cached resource already has index entries, as it would after a
        // completed DAV sync. An unindexed legacy resource deliberately starts
        // a full rebuild before the expired cursor can be tested.
        store.apply_provider_dav_page_indexed(&scope, &[(ProviderChange::Upsert(old.clone()), Some(ProviderDavIndex::Invalid), Some(ProviderAlarmIndex::Invalid))], "expired").unwrap();
        assert!(matches!(client.sync_collection(&store, &scope), Err(DavError::Http(403))));
        assert_eq!(store.provider_cursor(&scope).unwrap().as_deref(), Some("expired"));
        assert_eq!(store.provider_object(&scope, "old").unwrap(), Some(old));
        assert_eq!(store.provider_objects(&scope).unwrap().len(), 1);
        assert_eq!(store.provider_accessible(&scope).unwrap(), Some(false));
        server.join().unwrap();
    }

    #[test]
    fn expanded_calendar_query_returns_separate_server_view() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = format!("http://{}/", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = Vec::new(); let mut buf = [0u8; 2048];
            loop {
                let n = socket.read(&mut buf).unwrap(); assert_ne!(n, 0); request.extend_from_slice(&buf[..n]);
                if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request[..end]);
                    let length = headers.lines().find_map(|line| line.to_ascii_lowercase().strip_prefix("content-length:").and_then(|n| n.trim().parse::<usize>().ok())).unwrap_or(0);
                    if request.len() >= end + 4 + length { break; }
                }
            }
            let wire = String::from_utf8_lossy(&request).to_ascii_lowercase();
            assert!(wire.contains("depth: 1\r\n"));
            assert!(wire.contains("c:expand start="));
            let body = "<d:multistatus xmlns:d=\"DAV:\" xmlns:c=\"urn:ietf:params:xml:ns:caldav\"><d:response><d:href>/cal/a.ics</d:href><d:propstat><d:prop><d:getetag>&quot;v1&quot;</d:getetag><c:calendar-data><![CDATA[BEGIN:VCALENDAR\nBEGIN:VEVENT\nSUMMARY:Expanded\nEND:VEVENT\nEND:VCALENDAR]]></c:calendar-data></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>";
            write!(socket, "HTTP/1.1 207 Multi-Status\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        });
        let client = DavClient { http: Client::builder().no_proxy().timeout(std::time::Duration::from_secs(5)).redirect(reqwest::redirect::Policy::none()).build().unwrap(), origin: Url::parse(&origin).unwrap(), token: "fixture".into(), trusted_public_origin:None };
        let scope = ProviderScope { account_id:"fixture".into(), origin, service:"calendar".into(), collection:"/cal/".into() };
        let view = client.expanded_events(&scope,"20261028T080000Z","20261029T080000Z").unwrap();
        assert_eq!(view.len(),1);
        assert_eq!(view[0].etag,"\"v1\"");
        assert!(view[0].calendar_data.contains("SUMMARY:Expanded"));
        server.join().unwrap();
    }

    #[test]
    fn privilege_check_uses_current_user_effective_rights() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = format!("http://{}/", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            for _ in 0..2 {
                let (mut socket, _) = listener.accept().unwrap();
                let mut request = Vec::new(); let mut buf = [0u8; 2048];
                loop {
                    let n = socket.read(&mut buf).unwrap(); assert_ne!(n, 0); request.extend_from_slice(&buf[..n]);
                    if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&request[..end]);
                        let length = headers.lines().find_map(|line| line.to_ascii_lowercase().strip_prefix("content-length:").and_then(|n| n.trim().parse::<usize>().ok())).unwrap_or(0);
                        if request.len() >= end + 4 + length { break; }
                    }
                }
                let body = "<d:multistatus xmlns:d=\"DAV:\"><d:response><d:href>/cal/</d:href><d:propstat><d:prop><d:current-user-privilege-set><d:privilege><d:read/></d:privilege><d:privilege><d:bind/></d:privilege></d:current-user-privilege-set></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>";
                write!(socket, "HTTP/1.1 207 Multi-Status\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
        });
        let client = DavClient { http: Client::builder().no_proxy().timeout(std::time::Duration::from_secs(5)).redirect(reqwest::redirect::Policy::none()).build().unwrap(), origin: Url::parse(&origin).unwrap(), token: "fixture".into(), trusted_public_origin:None };
        let scope = ProviderScope { account_id:"fixture".into(), origin, service:"calendar".into(), collection:"/cal/".into() };
        assert!(client.permits(&scope,None,"bind").unwrap());
        assert!(!client.permits(&scope,None,"unbind").unwrap());
        server.join().unwrap();
    }

    #[test]
    fn discovery_distinguishes_own_shared_and_unknown_ownership() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = format!("http://{}/", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            for index in 0..3 {
                let (mut socket, _) = listener.accept().unwrap();
                let mut request = Vec::new(); let mut buf = [0u8; 2048];
                loop {
                    let n = socket.read(&mut buf).unwrap(); assert_ne!(n, 0); request.extend_from_slice(&buf[..n]);
                    if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&request[..end]);
                        let length = headers.lines().find_map(|line| line.to_ascii_lowercase().strip_prefix("content-length:").and_then(|n| n.trim().parse::<usize>().ok())).unwrap_or(0);
                        if request.len() >= end + 4 + length { break; }
                    }
                }
                let body = match index {
                    0 => "<d:multistatus xmlns:d=\"DAV:\"><d:response><d:href>/dav/</d:href><d:propstat><d:prop><d:current-user-principal><d:href>/dav/pal/me/</d:href></d:current-user-principal></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>",
                    1 => "<d:multistatus xmlns:d=\"DAV:\" xmlns:c=\"urn:ietf:params:xml:ns:caldav\"><d:response><d:href>/dav/pal/me/</d:href><d:propstat><d:prop><c:calendar-home-set><d:href>/dav/cal/me/</d:href></c:calendar-home-set></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>",
                    _ => "<d:multistatus xmlns:d=\"DAV:\" xmlns:c=\"urn:ietf:params:xml:ns:caldav\"><d:response><d:href>/dav/cal/me/own/</d:href><d:propstat><d:prop><d:resourcetype><c:calendar/></d:resourcetype><d:owner><d:href>/dav/pal/me/</d:href></d:owner></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response><d:response><d:href>/dav/cal/other/shared/</d:href><d:propstat><d:prop><d:resourcetype><c:calendar/></d:resourcetype><d:owner><d:href>/dav/pal/other/</d:href></d:owner></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response><d:response><d:href>/dav/cal/me/unknown/</d:href><d:propstat><d:prop><d:resourcetype><c:calendar/></d:resourcetype></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>",
                };
                write!(socket, "HTTP/1.1 207 Multi-Status\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
        });
        let reader = LoopbackDavReader::new(&origin,"fixture".into()).unwrap();
        let result = reader.discover("/dav/").unwrap();
        assert_eq!(result.len(),3);
        assert_eq!(result[0].owned_by_current_principal,Some(true));
        assert_eq!(result[1].owned_by_current_principal,Some(false));
        assert_eq!(result[2].owned_by_current_principal,None);
        server.join().unwrap();
    }
}
