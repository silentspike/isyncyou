//! CalDAV and CardDAV collection synchronization and conditional resource writes.
//!
//! The stored payload is the original server representation; it is never
//! regenerated from a partial Graph-shaped projection.

pub mod projection;

use isyncyou_store::{ProviderChange, ProviderObject, ProviderScope, Store};
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
}

struct Page {
    token: String,
    entries: Vec<Resource>,
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
        self.store.replace_provider_staged_object(self.scope, &self.generation, object).map_err(|e| DavError::Store(e.to_string()))
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

#[derive(Default)]
struct XmlNode {
    namespace: String,
    name: String,
    text: String,
    children: Vec<XmlNode>,
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
                let node = XmlNode { namespace, name: String::from_utf8_lossy(e.local_name().as_ref()).into_owned(), ..Default::default() };
                stack.push(node);
            }
            Event::Empty(e) => {
                count += 1;
                if count > 50_000 || stack.len() > 32 { return Err(DavError::InvalidResponse); }
                let node = XmlNode { namespace, name: String::from_utf8_lossy(e.local_name().as_ref()).into_owned(), ..Default::default() };
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
}

/// Server-expanded calendar projection, separate from the stored raw original.
pub struct ExpandedCalendar {
    pub href: String,
    pub etag: String,
    pub calendar_data: String,
}

fn utc_time(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 16 && b[8] == b'T' && b[15] == b'Z' && b.iter().enumerate().all(|(i, c)| i == 8 || i == 15 || c.is_ascii_digit())
}

/// A bearer token is held only in memory and is never included in errors.
pub struct DavClient {
    http: Client,
    origin: Url,
    token: String,
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
        Ok(Self(DavClient { http, origin: url, token }))
    }

    pub fn discover(&self, entry: &str) -> Result<Vec<Collection>, DavError> { self.0.discover(entry) }

    pub fn sync_collection(&self, store: &Store, scope: &ProviderScope) -> Result<usize, DavError> { self.0.sync_collection(store, scope) }
}

impl DavClient {
    pub fn new(origin: &str, token: String) -> Result<Self, DavError> {
        let origin = Url::parse(origin).map_err(|_| DavError::InvalidUrl)?;
        if token.is_empty() || origin.scheme() != "https" || origin.host_str().is_none() || !origin.username().is_empty() || origin.password().is_some() || origin.query().is_some() || origin.fragment().is_some() || origin.path() != "/" {
            return Err(DavError::InvalidUrl);
        }
        let http = Client::builder().timeout(std::time::Duration::from_secs(30)).redirect(reqwest::redirect::Policy::none()).build().map_err(|_| DavError::Transport("client initialization failed".into()))?;
        Ok(Self { http, origin, token })
    }

    fn checked_url(&self, href: &str) -> Result<Url, DavError> {
        self.checked_from(&self.origin, href)
    }

    fn checked_from(&self, base: &Url, href: &str) -> Result<Url, DavError> {
        let url = base.join(href).map_err(|_| DavError::InvalidUrl)?;
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
            node.child(D, "href").map(|n| n.text.trim()).filter(|s| !s.is_empty()).ok_or(DavError::InvalidResponse)
        }
        let principal_body = r#"<d:propfind xmlns:d="DAV:"><d:prop><d:current-user-principal/></d:prop></d:propfind>"#;
        let entry_url = self.checked_url(entry)?;
        let (entry_url, result) = self.propfind(entry_url, "0", principal_body)?;
        let principal_response = result.children.iter().find(|n| n.namespace == D && n.name == "response" && href(n).ok().and_then(|h| self.checked_from(&entry_url, h).ok()).as_ref() == Some(&entry_url)).ok_or(DavError::InvalidResponse)?;
        let principal = href(property(principal_response, D, "current-user-principal").ok_or(DavError::InvalidResponse)?)?;
        let homes_body = r#"<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav" xmlns:a="urn:ietf:params:xml:ns:carddav"><d:prop><c:calendar-home-set/><a:addressbook-home-set/></d:prop></d:propfind>"#;
        let principal_url = self.checked_from(&entry_url, principal)?;
        let (principal_url, homes) = self.propfind(principal_url, "0", homes_body)?;
        let home_response = homes.children.iter().find(|n| n.namespace == D && n.name == "response" && href(n).ok().and_then(|h| self.checked_from(&principal_url, h).ok()).as_ref() == Some(&principal_url)).ok_or(DavError::InvalidResponse)?;
        let mut result = Vec::new();
        for (ns, name) in [(C, "calendar-home-set"), (A, "addressbook-home-set")] {
            let Some(home) = property(home_response, ns, name) else { continue; };
            for home_href in home.children.iter().filter(|n| n.namespace == D && n.name == "href") {
                let home_url = self.checked_from(&principal_url, home_href.text.trim())?;
                if !home_url.path().ends_with('/') { return Err(DavError::InvalidUrl); }
                let body = r#"<d:propfind xmlns:d="DAV:"><d:prop><d:displayname/><d:resourcetype/></d:prop></d:propfind>"#;
                let (home_url, listing) = self.propfind(home_url, "1", body)?;
                if !home_url.path().ends_with('/') { return Err(DavError::InvalidUrl); }
                for response in listing.children.iter().filter(|n| n.namespace == D && n.name == "response") {
                    let url = self.checked_from(&home_url, href(response)?)?;
                    if !url.path().starts_with(home_url.path()) || !url.path().ends_with('/') { return Err(DavError::InvalidUrl); }
                    let Some(ty) = property(response, D, "resourcetype") else { continue; };
                    let calendar = ty.child(C, "calendar").is_some();
                    let addressbook = ty.child(A, "addressbook").is_some();
                    if !calendar && !addressbook { continue; }
                    let name = property(response, D, "displayname").map(|n| n.text.trim().to_owned()).unwrap_or_default();
                    result.push(Collection { url: url.to_string(), name, calendar, addressbook });
                }
            }
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
        let url = collection.join(href).map_err(|_| DavError::InvalidUrl)?;
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
        const D: &str = "DAV:";
        const C: &str = "urn:ietf:params:xml:ns:caldav";
        if scope.service != "calendar" || !utc_time(start) || !utc_time(end) || start >= end { return Err(DavError::InvalidResponse); }
        let collection = self.collection(scope)?;
        let body = format!("<c:calendar-query xmlns:c=\"{C}\" xmlns:d=\"DAV:\"><d:prop><d:getetag/><c:calendar-data><c:expand start=\"{start}\" end=\"{end}\"/></c:calendar-data></d:prop><c:filter><c:comp-filter name=\"VCALENDAR\"><c:comp-filter name=\"VEVENT\"><c:time-range start=\"{start}\" end=\"{end}\"/></c:comp-filter></c:comp-filter></c:filter></c:calendar-query>");
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
        let mut full = cursor.is_empty();
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
            let mut more = false;
            for entry in page.entries {
                let candidate = collection.join(&entry.href).map_err(|_| DavError::InvalidUrl)?;
                if candidate == collection && entry.status == Some(507) { more = true; continue; }
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
                store.apply_provider_page(scope, &changes, &page.token).map_err(|e| DavError::Store(e.to_string()))?;
            }
            cursor = page.token;
            if !more { return Ok(count); }
        }
    }

    /// Create only when absent, or replace exactly the version the caller read.
    pub fn put(&self, scope: &ProviderScope, href: &str, raw: Vec<u8>, etag: Option<&str>) -> Result<Option<String>, DavError> {
        let collection = self.collection(scope)?;
        let url = self.resource(&collection, href)?;
        let media = match scope.service.as_str() { "calendar" | "tasks" => "text/calendar; charset=utf-8", "contacts" => "text/vcard; charset=utf-8", _ => return Err(DavError::InvalidUrl) };
        let req = self.http.put(url).bearer_auth(&self.token).header(CONTENT_TYPE, media).body(raw);
        let req = match etag { Some(tag) if !tag.is_empty() => req.header(IF_MATCH, tag), None => req.header(IF_NONE_MATCH, "*"), _ => return Err(DavError::InvalidResponse) };
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
    use super::*;

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
                    format!("<d:multistatus xmlns:d=\"DAV:\"><d:response><d:href>/cal/a.ics</d:href><d:propstat><d:prop><d:getetag>&quot;v1&quot;</d:getetag></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response><d:sync-token>token-{index}</d:sync-token></d:multistatus>")
                };
                let etag = if index == 1 { "ETag: \"v1\"\r\n" } else { "" };
                let status = if index == 1 { "200 OK" } else { "207 Multi-Status" };
                write!(socket, "HTTP/1.1 {status}\r\nContent-Length: {}\r\n{etag}Connection: close\r\n\r\n{body}", body.len()).unwrap();
            }
        });
        // Plain HTTP is permitted only in this isolated loopback fixture.
        let client = DavClient { http: Client::builder().no_proxy().timeout(std::time::Duration::from_secs(5)).redirect(reqwest::redirect::Policy::none()).build().unwrap(), origin: Url::parse(&origin).unwrap(), token: "fixture".into() };
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
            write!(socket, "HTTP/1.1 412 Precondition Failed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
        });
        let client = DavClient { http: Client::builder().no_proxy().timeout(std::time::Duration::from_secs(5)).redirect(reqwest::redirect::Policy::none()).build().unwrap(), origin: Url::parse(&origin).unwrap(), token: "fixture".into() };
        let scope = ProviderScope { account_id: "fixture".into(), origin, service: "calendar".into(), collection: "/cal/".into() };
        assert!(matches!(client.put(&scope, "a.ics", b"fixture".to_vec(), Some("\"old\"")), Err(DavError::Conflict)));
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
        let client = DavClient { http: Client::builder().no_proxy().timeout(std::time::Duration::from_secs(5)).redirect(reqwest::redirect::Policy::none()).build().unwrap(), origin: Url::parse(&origin).unwrap(), token: "fixture".into() };
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
        let client = DavClient { http: Client::builder().no_proxy().timeout(std::time::Duration::from_secs(5)).redirect(reqwest::redirect::Policy::none()).build().unwrap(), origin: Url::parse(&origin).unwrap(), token: "fixture".into() };
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
        let client = DavClient { http: Client::builder().no_proxy().timeout(std::time::Duration::from_secs(5)).redirect(reqwest::redirect::Policy::none()).build().unwrap(), origin: Url::parse(&origin).unwrap(), token: "fixture".into() };
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
        let client = DavClient { http: Client::builder().no_proxy().timeout(std::time::Duration::from_secs(5)).redirect(reqwest::redirect::Policy::none()).build().unwrap(), origin: Url::parse(&origin).unwrap(), token: "fixture".into() };
        let scope = ProviderScope { account_id:"fixture".into(), origin, service:"calendar".into(), collection:"/cal/".into() };
        let store = Store::open_in_memory().unwrap();
        let old = ProviderObject { remote_id:"old".into(), etag:Some("\"old\"".into()), raw:b"preserved".to_vec() };
        store.apply_provider_page(&scope, &[ProviderChange::Upsert(old.clone())], "expired").unwrap();
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
        let client = DavClient { http: Client::builder().no_proxy().timeout(std::time::Duration::from_secs(5)).redirect(reqwest::redirect::Policy::none()).build().unwrap(), origin: Url::parse(&origin).unwrap(), token: "fixture".into() };
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
        let client = DavClient { http: Client::builder().no_proxy().timeout(std::time::Duration::from_secs(5)).redirect(reqwest::redirect::Policy::none()).build().unwrap(), origin: Url::parse(&origin).unwrap(), token: "fixture".into() };
        let scope = ProviderScope { account_id:"fixture".into(), origin, service:"calendar".into(), collection:"/cal/".into() };
        assert!(client.permits(&scope,None,"bind").unwrap());
        assert!(!client.permits(&scope,None,"unbind").unwrap());
        server.join().unwrap();
    }
}
