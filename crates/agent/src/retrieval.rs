//! The read-class tool executor, backed by an [`ArchiveSource`]. It implements
//! [`crate::ToolExecutor`] for `Search`/`Read`/`List`/`Export`, returning JSON whose
//! every result carries `{service, id, path}` source citations (REQ-AGENT-009) and
//! honours a `max_bytes` read budget. The logic is generic over `ArchiveSource`, so it
//! is tested with an in-memory fake (no store).

use crate::archive::{ArchiveSource, ItemRef};
use crate::provider::StreamEvent;
use crate::tool::{ToolAction, ToolClass};
use crate::turn::ToolExecutor;
use crate::AgentError;

/// Default read budget when the model does not set `max_bytes` (64 KiB).
pub const DEFAULT_READ_BUDGET: u64 = 64 * 1024;
/// Default search result cap when the model does not set `limit`.
pub const DEFAULT_SEARCH_LIMIT: u32 = 20;
/// Default flat list page size when the model does not set `limit`.
pub const DEFAULT_LIST_LIMIT: u32 = 50;
/// Hard cap for public list pages; deep-search uses its own candidate budget.
pub const MAX_LIST_LIMIT: u32 = 200;
/// Body preview length (chars) attached to each hit: enough for a real content preview in
/// the expanded card, not just the one-line header. Whitespace is collapsed first.
pub const PREVIEW_CHARS: usize = 1200;
/// Default number of candidate bodies a single deep-search pass reads (budget). Bounds
/// cost on a large mailbox; the model resumes via `next_cursor` to "search deeper".
pub const DEFAULT_DEEP_READS: u32 = 12;
/// Hard cap on a deep-search pass regardless of the model's `max_reads`.
pub const MAX_DEEP_READS: u32 = 40;
/// The M365 services a deep scan covers when the model names none.
pub const SCANNABLE_SERVICES: &[&str] = &[
    "mail", "onedrive", "calendar", "contacts", "todo", "onenote",
];

/// Executes read-class actions against an [`ArchiveSource`].
pub struct RetrievalExecutor<A: ArchiveSource> {
    source: A,
}

impl<A: ArchiveSource> RetrievalExecutor<A> {
    pub fn new(source: A) -> Self {
        Self { source }
    }

    fn ensure_account(&self, action: &ToolAction) -> Result<(), AgentError> {
        let account = match action {
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
        };
        if account != self.source.account() {
            return Err(AgentError::ToolArgs(format!(
                "account mismatch: tool requested {account}, executor is bound to {}",
                self.source.account()
            )));
        }
        Ok(())
    }

    fn citation_ref(it: &ItemRef) -> serde_json::Value {
        serde_json::json!({
            "service": it.service,
            "id": it.id,
            "path": it.path,
        })
    }

    fn source_ref(it: &ItemRef) -> serde_json::Value {
        serde_json::json!({
            "service": it.service,
            "id": it.id,
            "name": it.name,
            "item_type": it.item_type,
            "path": it.path,
            "source": Self::citation_ref(it),
        })
    }

    /// Like [`source_ref`], plus a body `preview` (best-effort) so the UI can render a real
    /// content preview per hit — the card header shows the first line, the expanded panel
    /// shows this whole preview — and the model can judge relevance without a second
    /// round-trip. Whitespace-collapsed and capped at [`PREVIEW_CHARS`]; empty when the
    /// item has no archived body or the read fails.
    fn hit_json(&self, it: &ItemRef) -> serde_json::Value {
        let mut v = Self::source_ref(it);
        let preview = if it.path.is_some() {
            self.source
                .read_body(&it.service, &it.id)
                .ok()
                .map(|b| Self::body_preview(&it.service, &b))
                .unwrap_or_default()
        } else {
            String::new()
        };
        v["snippet"] = serde_json::Value::String(preview);
        v
    }

    /// Turn a raw archived body into a readable, whitespace-collapsed preview capped at
    /// [`PREVIEW_CHARS`]. Mail bodies are full `.eml` MIME, so extract the `text/plain`
    /// part (tag-stripped `text/html` fallback) — the same [`isyncyou_connectors::mime`]
    /// text the store indexes — instead of showing raw headers/boundaries. Other services
    /// archive already-readable bodies (ics/vCard/text), so pass them through.
    fn body_preview(service: &str, body: &[u8]) -> String {
        Self::body_model_text(service, body)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(PREVIEW_CHARS)
            .collect()
    }

    /// Convert archived bytes into the text exposed to the model. Counters and
    /// truncation are defined over this final UTF-8 string, not the raw archive bytes.
    fn body_model_text(service: &str, body: &[u8]) -> String {
        #[cfg(feature = "retrieval")]
        let text = if service == "mail" {
            // Real mail is `.eml` MIME → extract the readable text. A non-MIME/plain body
            // (or a message extract_text can't parse) falls back to raw so nothing is lost.
            let t = isyncyou_connectors::mime::extract_text(body);
            if t.trim().is_empty() {
                String::from_utf8_lossy(body).into_owned()
            } else {
                t
            }
        } else {
            String::from_utf8_lossy(body).into_owned()
        };
        #[cfg(not(feature = "retrieval"))]
        let text = {
            let _ = service;
            String::from_utf8_lossy(body).into_owned()
        };
        text
    }

    fn content_kind(service: &str) -> &'static str {
        match service {
            "mail" => "mail-text",
            "calendar" | "contacts" | "todo" => "json",
            "onedrive" | "onenote" => "text",
            _ => "text",
        }
    }

    fn utf8_budget_slice(text: &str, max_bytes: usize) -> (&str, bool) {
        if text.len() <= max_bytes {
            return (text, false);
        }
        let mut end = max_bytes;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        (&text[..end], true)
    }

    fn list_limit(limit: Option<u32>) -> u32 {
        limit.unwrap_or(DEFAULT_LIST_LIMIT).min(MAX_LIST_LIMIT)
    }

    fn page_items(items: Vec<ItemRef>, limit: u32, offset: u32) -> Vec<ItemRef> {
        items
            .into_iter()
            .skip(offset as usize)
            .take(limit as usize)
            .collect()
    }

    fn search(
        &self,
        services: &[String],
        query: &str,
        limit: Option<u32>,
    ) -> Result<String, AgentError> {
        let cap = limit.unwrap_or(DEFAULT_SEARCH_LIMIT) as usize;
        let mut hits = self.source.search_names(query)?;
        let mut seen: std::collections::HashSet<(String, String)> = hits
            .iter()
            .map(|i| (i.service.clone(), i.id.clone()))
            .collect();
        for (service, id) in self.source.search_bodies(query)? {
            if seen.insert((service.clone(), id.clone())) {
                if let Some(it) = self.source.get(&service, &id)? {
                    hits.push(it);
                }
            }
        }
        if !services.is_empty() {
            hits.retain(|i| services.iter().any(|s| s == &i.service));
        }
        let total = hits.len();
        let results: Vec<serde_json::Value> =
            hits.iter().take(cap).map(|it| self.hit_json(it)).collect();
        Ok(serde_json::json!({
            "query": query,
            "returned": results.len(),
            "total_matches": total,
            "results": results,
        })
        .to_string())
    }

    /// Progressive search (S-AG.18/#643): the same merge as [`search`], but run as
    /// visible stages — **stage 1** fast name/subject match, **stage 2** full-text over
    /// bodies — emitting a `SearchStage` boundary + a `PartialResult` of the newly-added,
    /// deduped, source-tagged hits after each, so the UI grows the list live. The final
    /// JSON is identical to [`search`] (plus a `deep_search_hint`): the returned string is
    /// what the model answers from; **stage 3** is the [`deep_search`](Self::deep_search)
    /// op, which the model calls (guided by that hint) to surface matches whose wording the
    /// query never contains.
    fn search_staged(
        &self,
        services: &[String],
        query: &str,
        limit: Option<u32>,
        emit: &mut dyn FnMut(StreamEvent),
    ) -> Result<String, AgentError> {
        let in_scope =
            |it: &ItemRef| services.is_empty() || services.iter().any(|s| s == &it.service);
        let mut seen: std::collections::HashSet<(String, String)> =
            std::collections::HashSet::new();
        let mut hits: Vec<ItemRef> = Vec::new();

        // Stage 1 — fast name/subject match (indexed).
        emit(StreamEvent::SearchStage {
            stage: "names".into(),
            status: "running".into(),
            hits: 0,
        });
        let mut stage1: Vec<serde_json::Value> = Vec::new();
        for it in self.source.search_names(query)? {
            if in_scope(&it) && seen.insert((it.service.clone(), it.id.clone())) {
                stage1.push(self.hit_json(&it));
                hits.push(it);
            }
        }
        emit(StreamEvent::PartialResult {
            stage: "names".into(),
            items: serde_json::Value::Array(stage1),
        });
        emit(StreamEvent::SearchStage {
            stage: "names".into(),
            status: "done".into(),
            hits: hits.len(),
        });

        // Stage 2 — full-text over indexed bodies (only items stage 1 didn't already have).
        emit(StreamEvent::SearchStage {
            stage: "bodies".into(),
            status: "running".into(),
            hits: hits.len(),
        });
        let mut stage2: Vec<serde_json::Value> = Vec::new();
        for (service, id) in self.source.search_bodies(query)? {
            if seen.insert((service.clone(), id.clone())) {
                if let Some(it) = self.source.get(&service, &id)? {
                    if in_scope(&it) {
                        stage2.push(self.hit_json(&it));
                        hits.push(it);
                    }
                }
            }
        }
        emit(StreamEvent::PartialResult {
            stage: "bodies".into(),
            items: serde_json::Value::Array(stage2),
        });
        emit(StreamEvent::SearchStage {
            stage: "bodies".into(),
            status: "done".into(),
            hits: hits.len(),
        });

        let cap = limit.unwrap_or(DEFAULT_SEARCH_LIMIT) as usize;
        let total = hits.len();
        let results: Vec<serde_json::Value> =
            hits.iter().take(cap).map(|it| self.hit_json(it)).collect();
        Ok(serde_json::json!({
            "query": query,
            "returned": results.len(),
            "total_matches": total,
            "results": results,
            "deep_search_hint": "Keyword passes (name + full-text) are done. If the user may mean something these missed (different wording/synonyms), call `deep-search` — it scans metadata and reads unmatched candidate bodies for you to judge; resume with its `next_cursor` to search deeper.",
        })
        .to_string())
    }

    /// Stage 3 — agentic deep read (S-AG.18/#643). Keyword search (`search`) only finds
    /// items whose name or body literally contains the query; this surfaces the ones it
    /// missed so the model can judge them semantically. It builds the keyword-matched set
    /// (to exclude), metadata-scans the in-scope services, then reads up to a **budget** of
    /// unmatched candidate bodies from `cursor`, returning short snippets + a coverage note
    /// and a `next_cursor` to continue. It never reads the whole mailbox in one pass.
    fn deep_search(
        &self,
        services: &[String],
        query: &str,
        cursor: Option<u32>,
        max_reads: Option<u32>,
        emit: &mut dyn FnMut(StreamEvent),
    ) -> Result<String, AgentError> {
        emit(StreamEvent::SearchStage {
            stage: "deep".into(),
            status: "running".into(),
            hits: 0,
        });

        // Items the keyword passes already found — the deep read only covers the misses.
        let mut matched: std::collections::HashSet<(String, String)> =
            std::collections::HashSet::new();
        for it in self.source.search_names(query)? {
            matched.insert((it.service, it.id));
        }
        for pair in self.source.search_bodies(query)? {
            matched.insert(pair);
        }

        // Metadata scan (names only, cheap) across the in-scope services, stable order,
        // keeping only unmatched items that actually have an archived body to read.
        let scan: Vec<String> = if services.is_empty() {
            SCANNABLE_SERVICES.iter().map(|s| s.to_string()).collect()
        } else {
            services.to_vec()
        };
        let mut candidates: Vec<ItemRef> = Vec::new();
        for svc in &scan {
            for it in self.source.list_page(svc, u32::MAX, 0)? {
                if it.path.is_some() && !matched.contains(&(it.service.clone(), it.id.clone())) {
                    candidates.push(it);
                }
            }
        }
        let total = candidates.len();

        // Budgeted read window from `cursor`.
        let start = cursor.unwrap_or(0) as usize;
        let budget = max_reads.unwrap_or(DEFAULT_DEEP_READS).min(MAX_DEEP_READS) as usize;
        let mut read_items: Vec<serde_json::Value> = Vec::new();
        for it in candidates.iter().skip(start).take(budget) {
            // Same content-preview shape as the keyword hits (header + body preview).
            read_items.push(self.hit_json(it));
        }
        let next = start + read_items.len();
        let more = next < total;

        emit(StreamEvent::PartialResult {
            stage: "deep".into(),
            items: serde_json::Value::Array(read_items.clone()),
        });
        emit(StreamEvent::SearchStage {
            stage: "deep".into(),
            status: "done".into(),
            hits: read_items.len(),
        });

        let coverage = if more {
            format!(
                "Read {} of {total} unmatched candidates (from {start}). Judge these by \
                 content; to search deeper, call deep-search again with cursor={next}.",
                read_items.len()
            )
        } else {
            format!("Read all {total} unmatched candidates — this is the full deep scan.")
        };
        Ok(serde_json::json!({
            "query": query,
            "stage": "deep",
            "candidates_total": total,
            "read": read_items.len(),
            "cursor": start,
            "next_cursor": if more { Some(next) } else { None },
            "budget_reached": more,
            "candidates": read_items,
            "coverage_note": coverage,
        })
        .to_string())
    }

    fn read(&self, service: &str, id: &str, max_bytes: Option<u64>) -> Result<String, AgentError> {
        let item = self
            .source
            .get(service, id)?
            .ok_or_else(|| AgentError::ToolArgs(format!("no item {service}/{id}")))?;
        let bytes = self.source.read_body(service, id)?;
        let text = Self::body_model_text(service, &bytes);
        let budget = max_bytes.unwrap_or(DEFAULT_READ_BUDGET) as usize;
        let (content, truncated) = Self::utf8_budget_slice(&text, budget);
        let source = Self::citation_ref(&item);
        Ok(serde_json::json!({
            "service": item.service,
            "id": item.id,
            "name": item.name,
            "path": item.path,
            "source": source,
            "content_kind": Self::content_kind(service),
            "bytes_total": text.len(),
            "bytes_returned": content.len(),
            "truncated": truncated,
            "content": content,
        })
        .to_string())
    }

    fn list(
        &self,
        service: &str,
        parent: Option<&str>,
        limit: Option<u32>,
        offset: Option<u32>,
    ) -> Result<String, AgentError> {
        let limit = Self::list_limit(limit);
        let offset = offset.unwrap_or(0);
        let items = match parent {
            Some("root") | Some("") => Self::page_items(self.source.roots(service)?, limit, offset),
            Some(parent) => Self::page_items(self.source.children(service, parent)?, limit, offset),
            None => self.source.list_page(service, limit, offset)?,
        };
        let count = self.source.count(service)?;
        let results: Vec<serde_json::Value> = items.iter().map(Self::source_ref).collect();
        Ok(serde_json::json!({
            "service": service,
            "parent": parent,
            "limit": limit,
            "offset": offset,
            "service_total": count,
            "returned": results.len(),
            "results": results,
        })
        .to_string())
    }

    fn export(&self, service: &str, id: &str) -> Result<String, AgentError> {
        let item = self
            .source
            .get(service, id)?
            .ok_or_else(|| AgentError::ToolArgs(format!("no item {service}/{id}")))?;
        let bytes = self.source.read_body(service, id)?;
        let (format, content) = convert_export(service, &bytes)?;
        let source = Self::citation_ref(&item);
        Ok(serde_json::json!({
            "service": item.service,
            "id": item.id,
            "path": item.path,
            "source": source,
            "format": format,
            "content": content,
        })
        .to_string())
    }
}

/// Convert an archived body to a portable export. Calendar→ics / Contacts→vcard need
/// the `retrieval` feature (the connectors converters); otherwise everything is `raw`.
fn convert_export(service: &str, bytes: &[u8]) -> Result<(&'static str, String), AgentError> {
    #[cfg(feature = "retrieval")]
    {
        if service == "calendar" || service == "contacts" {
            let v: serde_json::Value = serde_json::from_slice(bytes)
                .map_err(|e| AgentError::Provider(format!("export parse: {e}")))?;
            return Ok(match service {
                "calendar" => ("ics", isyncyou_connectors::event_to_ics(&v)),
                _ => ("vcard", isyncyou_connectors::contact_to_vcard(&v)),
            });
        }
    }
    let _ = service;
    Ok(("raw", String::from_utf8_lossy(bytes).into_owned()))
}

impl<A: ArchiveSource> ToolExecutor for RetrievalExecutor<A> {
    fn execute_read(&self, action: &ToolAction) -> Result<String, AgentError> {
        // Defensive: the loop only routes read-class actions here.
        if action.class() != ToolClass::Read {
            return Err(AgentError::ToolArgs(format!(
                "{} is destructive and must go through confirmation, not the read executor",
                action.op()
            )));
        }
        self.ensure_account(action)?;
        match action {
            ToolAction::Search {
                services,
                query,
                limit,
                ..
            } => self.search(services, query, *limit),
            ToolAction::DeepSearch {
                services,
                query,
                cursor,
                max_reads,
                ..
            } => self.deep_search(services, query, *cursor, *max_reads, &mut |_| {}),
            ToolAction::Read {
                service,
                id,
                max_bytes,
                ..
            } => self.read(service, id, *max_bytes),
            ToolAction::List {
                service,
                parent,
                limit,
                offset,
                ..
            } => self.list(service, parent.as_deref(), *limit, *offset),
            ToolAction::Export { service, id, .. } => self.export(service, id),
            // restore-local is read-class but writes a local file — it lands in S-AG.9/#624.
            ToolAction::RestoreLocal { .. } => Err(AgentError::ToolArgs(
                "restore-local is implemented in the operations layer (S-AG.9/#624)".into(),
            )),
            other => Err(AgentError::ToolArgs(format!(
                "unsupported read op: {}",
                other.op()
            ))),
        }
    }

    fn execute_read_streamed(
        &self,
        action: &ToolAction,
        emit: &mut dyn FnMut(StreamEvent),
    ) -> Result<String, AgentError> {
        if action.class() != ToolClass::Read {
            return self.execute_read(action);
        }
        self.ensure_account(action)?;
        // Search + deep-search run as visible stages; every other read is single-shot.
        match action {
            ToolAction::Search {
                services,
                query,
                limit,
                ..
            } => self.search_staged(services, query, *limit, emit),
            ToolAction::DeepSearch {
                services,
                query,
                cursor,
                max_reads,
                ..
            } => self.deep_search(services, query, *cursor, *max_reads, emit),
            _ => self.execute_read(action),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// In-memory archive for testing the executor logic without a store.
    struct FakeArchive {
        account: String,
        items: Vec<(ItemRef, Option<Vec<u8>>)>,
    }
    impl FakeArchive {
        fn item(
            service: &str,
            id: &str,
            name: &str,
            body: Option<&str>,
        ) -> (ItemRef, Option<Vec<u8>>) {
            (
                ItemRef {
                    service: service.into(),
                    id: id.into(),
                    name: name.into(),
                    item_type: "message".into(),
                    path: Some(format!("{service}/{id}.bin")),
                },
                body.map(|b| b.as_bytes().to_vec()),
            )
        }
    }
    impl ArchiveSource for FakeArchive {
        fn account(&self) -> &str {
            &self.account
        }

        fn search_names(&self, query: &str) -> Result<Vec<ItemRef>, AgentError> {
            let q = query.to_lowercase();
            Ok(self
                .items
                .iter()
                .filter(|(i, _)| i.name.to_lowercase().contains(&q))
                .map(|(i, _)| i.clone())
                .collect())
        }
        fn search_bodies(&self, query: &str) -> Result<Vec<(String, String)>, AgentError> {
            let q = query.to_lowercase();
            Ok(self
                .items
                .iter()
                .filter(|(_, b)| {
                    b.as_ref()
                        .map(|b| String::from_utf8_lossy(b).to_lowercase().contains(&q))
                        .unwrap_or(false)
                })
                .map(|(i, _)| (i.service.clone(), i.id.clone()))
                .collect())
        }
        fn get(&self, service: &str, id: &str) -> Result<Option<ItemRef>, AgentError> {
            Ok(self
                .items
                .iter()
                .find(|(i, _)| i.service == service && i.id == id)
                .map(|(i, _)| i.clone()))
        }
        fn read_body(&self, service: &str, id: &str) -> Result<Vec<u8>, AgentError> {
            self.items
                .iter()
                .find(|(i, _)| i.service == service && i.id == id)
                .and_then(|(_, b)| b.clone())
                .ok_or_else(|| AgentError::ToolArgs(format!("no body {service}/{id}")))
        }
        fn list_page(
            &self,
            service: &str,
            limit: u32,
            offset: u32,
        ) -> Result<Vec<ItemRef>, AgentError> {
            Ok(self
                .items
                .iter()
                .filter(|(i, _)| i.service == service)
                .skip(offset as usize)
                .take(limit as usize)
                .map(|(i, _)| i.clone())
                .collect())
        }
        fn roots(&self, service: &str) -> Result<Vec<ItemRef>, AgentError> {
            Ok(self
                .items
                .iter()
                .filter(|(i, _)| i.service == service)
                .map(|(i, _)| i.clone())
                .collect())
        }
        fn children(&self, service: &str, parent: &str) -> Result<Vec<ItemRef>, AgentError> {
            let _ = parent;
            self.roots(service)
        }
        fn count(&self, service: &str) -> Result<u64, AgentError> {
            Ok(self
                .items
                .iter()
                .filter(|(i, _)| i.service == service)
                .count() as u64)
        }
    }

    struct RoutingArchive {
        account: String,
        calls: std::rc::Rc<std::cell::RefCell<Vec<String>>>,
    }

    impl RoutingArchive {
        fn new(calls: std::rc::Rc<std::cell::RefCell<Vec<String>>>) -> Self {
            Self {
                account: "me".into(),
                calls,
            }
        }

        fn route_item(service: &str, id: &str, name: &str) -> ItemRef {
            ItemRef {
                service: service.into(),
                id: id.into(),
                name: name.into(),
                item_type: "folder".into(),
                path: Some(format!("{service}/{id}.bin")),
            }
        }
    }

    impl ArchiveSource for RoutingArchive {
        fn account(&self) -> &str {
            &self.account
        }

        fn search_names(&self, _query: &str) -> Result<Vec<ItemRef>, AgentError> {
            Ok(Vec::new())
        }

        fn search_bodies(&self, _query: &str) -> Result<Vec<(String, String)>, AgentError> {
            Ok(Vec::new())
        }

        fn get(&self, _service: &str, _id: &str) -> Result<Option<ItemRef>, AgentError> {
            Ok(None)
        }

        fn read_body(&self, service: &str, id: &str) -> Result<Vec<u8>, AgentError> {
            Err(AgentError::ToolArgs(format!("no body {service}/{id}")))
        }

        fn list_page(
            &self,
            service: &str,
            limit: u32,
            offset: u32,
        ) -> Result<Vec<ItemRef>, AgentError> {
            self.calls
                .borrow_mut()
                .push(format!("list_page:{limit}:{offset}"));
            let items = vec![
                Self::route_item(service, "flat-0", "Flat 0"),
                Self::route_item(service, "flat-1", "Flat 1"),
                Self::route_item(service, "flat-2", "Flat 2"),
            ];
            Ok(items
                .into_iter()
                .skip(offset as usize)
                .take(limit as usize)
                .collect())
        }

        fn roots(&self, service: &str) -> Result<Vec<ItemRef>, AgentError> {
            self.calls.borrow_mut().push("roots".into());
            Ok(vec![Self::route_item(service, "root-only", "Root Only")])
        }

        fn children(&self, service: &str, parent: &str) -> Result<Vec<ItemRef>, AgentError> {
            self.calls.borrow_mut().push(format!("children:{parent}"));
            Ok(vec![Self::route_item(service, "child-only", "Child Only")])
        }

        fn count(&self, _service: &str) -> Result<u64, AgentError> {
            Ok(123)
        }
    }

    fn fixture() -> RetrievalExecutor<FakeArchive> {
        RetrievalExecutor::new(FakeArchive {
            account: "me".into(),
            items: vec![
                FakeArchive::item(
                    "mail",
                    "m1",
                    "Spotify invoice March",
                    Some("Your Spotify receipt, total 9.99"),
                ),
                FakeArchive::item("mail", "m2", "Dinner plans", Some("see you at 8")),
                FakeArchive::item("onedrive", "f1", "spotify-logo.png", None),
            ],
        })
    }

    #[cfg(feature = "retrieval")]
    fn upsert_store_body(
        store: &isyncyou_store::Store,
        root: &std::path::Path,
        mut item: isyncyou_store::Item,
        rel: &str,
        body: &[u8],
    ) {
        item.local_path = Some(rel.into());
        store.upsert_item(&item).unwrap();
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        isyncyou_core::envelope::write_body_atomic(&path, body).unwrap();
    }

    #[test]
    fn search_returns_source_tagged_hits_across_names_and_bodies() {
        let ex = fixture();
        let out = ex.search(&[], "spotify", None).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        // m1 (name + body) and f1 (name) match; deduped; each carries source ids.
        assert_eq!(v["total_matches"], 2);
        let ids: Vec<&str> = v["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["id"].as_str().unwrap())
            .collect();
        assert!(ids.contains(&"m1") && ids.contains(&"f1"));
        for r in v["results"].as_array().unwrap() {
            assert!(r["service"].is_string() && r["id"].is_string() && r["path"].is_string());
            assert!(r["source"]["service"].is_string() && r["source"]["id"].is_string());
            assert!(r["snippet"].is_string());
        }
    }

    #[test]
    fn progressive_search_streams_staged_deduped_results() {
        let ex = fixture();
        let mut events: Vec<StreamEvent> = Vec::new();
        let out = ex
            .search_staged(&[], "spotify", None, &mut |e| events.push(e))
            .unwrap();

        // Stage boundaries, in order: names running→done, then bodies running→done.
        let stages: Vec<(String, String, usize)> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::SearchStage {
                    stage,
                    status,
                    hits,
                } => Some((stage.clone(), status.clone(), *hits)),
                _ => None,
            })
            .collect();
        assert_eq!(
            stages,
            vec![
                ("names".into(), "running".into(), 0),
                ("names".into(), "done".into(), 2), // m1 + f1 matched by name
                ("bodies".into(), "running".into(), 2),
                ("bodies".into(), "done".into(), 2), // m1's body dup deduped → no growth
            ]
        );

        // Partial results: stage 1 carries the two name hits; stage 2 is empty (deduped).
        let partials: Vec<(String, usize)> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::PartialResult { stage, items } => {
                    Some((stage.clone(), items.as_array().unwrap().len()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(partials, vec![("names".into(), 2), ("bodies".into(), 0)]);

        // Final JSON equals the non-staged search: deduped total, source-tagged, + hint.
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["total_matches"], 2);
        assert!(v["deep_search_hint"].is_string());
        for r in v["results"].as_array().unwrap() {
            for field in ["service", "id", "name", "item_type", "path", "snippet"] {
                assert!(
                    !r[field].is_null(),
                    "staged final result must carry {field}: {r}"
                );
            }
        }
    }

    #[test]
    fn progressive_search_stage2_adds_body_only_hits() {
        // "receipt" appears only in m1's body, in no name — stage 2 (full-text) must add it.
        let ex = fixture();
        let mut events: Vec<StreamEvent> = Vec::new();
        let out = ex
            .search_staged(&[], "receipt", None, &mut |e| events.push(e))
            .unwrap();
        let done: Vec<(String, usize)> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::SearchStage {
                    stage,
                    status,
                    hits,
                } if status == "done" => Some((stage.clone(), *hits)),
                _ => None,
            })
            .collect();
        assert_eq!(done, vec![("names".into(), 0), ("bodies".into(), 1)]);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["results"][0]["id"], "m1");
        assert!(
            v["results"][0]["snippet"]
                .as_str()
                .unwrap()
                .contains("receipt"),
            "body-only search hit should carry a readable snippet"
        );
    }

    #[test]
    fn non_streaming_search_body_only_hit_carries_snippet() {
        let ex = fixture();
        let out = ex.search(&[], "receipt", None).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["results"][0]["id"], "m1");
        assert!(v["results"][0]["snippet"]
            .as_str()
            .unwrap()
            .contains("receipt"));
    }

    #[test]
    fn search_keeps_unreadable_body_hit_with_empty_snippet() {
        let ex = fixture();
        let out = ex.search(&["onedrive".into()], "spotify", None).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["results"].as_array().unwrap().len(), 1);
        assert_eq!(v["results"][0]["id"], "f1");
        assert_eq!(v["results"][0]["snippet"], "");
    }

    #[test]
    fn deep_search_surfaces_keyword_less_candidate_and_excludes_matched() {
        // m1's body literally contains the query (found by plain search → excluded from the
        // deep pass); m2 is about the same topic but never says "distrokid" (the keyword-less
        // match the deep read must surface); m3 is noise.
        let ex = RetrievalExecutor::new(FakeArchive {
            account: "me".into(),
            items: vec![
                FakeArchive::item(
                    "mail",
                    "m1",
                    "Invoice March",
                    Some("Your DistroKid renewal, 22.99"),
                ),
                FakeArchive::item(
                    "mail",
                    "m2",
                    "Music payout",
                    Some("Your streaming distributor paid out 41.00"),
                ),
                FakeArchive::item("mail", "m3", "Lunch", Some("see you at noon")),
            ],
        });
        let mut events: Vec<StreamEvent> = Vec::new();
        let out = ex
            .deep_search(&["mail".into()], "distrokid", None, Some(10), &mut |e| {
                events.push(e)
            })
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let ids: Vec<&str> = v["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["id"].as_str().unwrap())
            .collect();
        assert!(
            ids.contains(&"m2"),
            "keyword-less candidate must be surfaced"
        );
        assert!(
            !ids.contains(&"m1"),
            "keyword-matched item is excluded from the deep pass"
        );
        let m2 = v["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["id"] == "m2")
            .unwrap();
        assert!(
            m2["snippet"].as_str().unwrap().contains("distributor"),
            "the candidate body is surfaced for the model to judge"
        );
        assert!(events.iter().any(|e| matches!(
            e,
            StreamEvent::SearchStage { stage, status, .. } if stage == "deep" && status == "done"
        )));
    }

    #[test]
    fn deep_search_budget_and_cursor_paginate() {
        let items: Vec<_> = (0..5)
            .map(|i| {
                FakeArchive::item(
                    "mail",
                    &format!("d{i}"),
                    &format!("Note {i}"),
                    Some(&format!("body {i}")),
                )
            })
            .collect();
        let ex = RetrievalExecutor::new(FakeArchive {
            account: "me".into(),
            items,
        });
        // Query matches nothing → all 5 are unmatched candidates; budget of 2 per pass.
        let out = ex
            .deep_search(&["mail".into()], "zzzznomatch", None, Some(2), &mut |_| {})
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["read"], 2);
        assert_eq!(v["candidates_total"], 5);
        assert_eq!(v["budget_reached"], true);
        assert_eq!(v["next_cursor"], 2);
        // Resume from the cursor to search deeper.
        let out2 = ex
            .deep_search(
                &["mail".into()],
                "zzzznomatch",
                Some(2),
                Some(2),
                &mut |_| {},
            )
            .unwrap();
        let v2: serde_json::Value = serde_json::from_str(&out2).unwrap();
        assert_eq!(v2["cursor"], 2);
        assert_eq!(v2["read"], 2);
        assert_eq!(v2["next_cursor"], 4);
    }

    #[test]
    fn search_respects_service_filter_and_limit() {
        let ex = fixture();
        let mail_only: serde_json::Value =
            serde_json::from_str(&ex.search(&["mail".into()], "spotify", None).unwrap()).unwrap();
        assert_eq!(mail_only["results"].as_array().unwrap().len(), 1); // only m1
        let limited: serde_json::Value =
            serde_json::from_str(&ex.search(&[], "spotify", Some(1)).unwrap()).unwrap();
        assert_eq!(limited["returned"], 1);
        assert_eq!(limited["total_matches"], 2);
    }

    #[test]
    fn read_respects_byte_budget_and_flags_truncation() {
        let ex = fixture();
        let out = ex.read("mail", "m1", Some(10)).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["truncated"], true);
        assert_eq!(v["bytes_returned"], 10);
        assert!(v["bytes_total"].as_u64().unwrap() > 10);
        assert_eq!(v["content"].as_str().unwrap().len(), 10);
        // Without a budget it returns the whole body, untruncated.
        let full: serde_json::Value =
            serde_json::from_str(&ex.read("mail", "m1", None).unwrap()).unwrap();
        assert_eq!(full["truncated"], false);
    }

    #[test]
    fn read_counts_model_text_and_truncates_on_utf8_boundary() {
        let ex = RetrievalExecutor::new(FakeArchive {
            account: "me".into(),
            items: vec![FakeArchive::item("onedrive", "u1", "Utf8", Some("ééx"))],
        });
        let out = ex.read("onedrive", "u1", Some(3)).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["bytes_total"], "ééx".len());
        assert_eq!(v["bytes_returned"], "é".len());
        assert_eq!(v["truncated"], true);
        assert_eq!(v["content"], "é");
    }

    #[test]
    fn read_includes_source_object() {
        let ex = fixture();
        let out = ex.read("mail", "m1", None).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["source"]["service"], "mail");
        assert_eq!(v["source"]["id"], "m1");
        assert_eq!(v["source"]["path"], "mail/m1.bin");
        assert_eq!(v["name"], "Spotify invoice March");
        assert_eq!(v["content_kind"], "mail-text");
    }

    #[test]
    fn read_missing_body_returns_controlled_error() {
        let ex = fixture();
        let err = ex.read("onedrive", "f1", None).unwrap_err();
        assert!(err.to_string().contains("no body onedrive/f1"));
    }

    #[cfg(feature = "retrieval")]
    #[test]
    fn read_mail_returns_model_text_not_raw_eml_headers() {
        let eml = concat!(
            "Subject: Quarterly Report\r\n",
            "From: Ada <ada@example.com>\r\n",
            "Content-Type: text/plain; charset=utf-8\r\n",
            "\r\n",
            "Hello from the extracted body.\r\n"
        );
        let ex = RetrievalExecutor::new(FakeArchive {
            account: "me".into(),
            items: vec![FakeArchive::item("mail", "eml1", "Quarterly", Some(eml))],
        });
        let out = ex.read("mail", "eml1", None).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let content = v["content"].as_str().unwrap();
        assert!(content.contains("Quarterly Report"));
        assert!(content.contains("Hello from the extracted body."));
        assert!(!content.contains("Content-Type:"));
        assert_eq!(v["bytes_total"], content.len());
        assert_eq!(v["bytes_returned"], content.len());
        assert_eq!(v["truncated"], false);
    }

    #[test]
    fn list_reports_items_and_service_total() {
        let ex = fixture();
        let v: serde_json::Value =
            serde_json::from_str(&ex.list("mail", None, None, None).unwrap()).unwrap();
        assert_eq!(v["service_total"], 2);
        assert_eq!(v["results"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn list_pages_flat_service_items_with_count() {
        let calls = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let ex = RetrievalExecutor::new(RoutingArchive::new(calls.clone()));
        let v: serde_json::Value =
            serde_json::from_str(&ex.list("onedrive", None, Some(1), Some(1)).unwrap()).unwrap();
        assert_eq!(*calls.borrow(), vec!["list_page:1:1"]);
        assert_eq!(v["service"], "onedrive");
        assert_eq!(v["parent"], serde_json::Value::Null);
        assert_eq!(v["limit"], 1);
        assert_eq!(v["offset"], 1);
        assert_eq!(v["service_total"], 123);
        assert_eq!(v["returned"], 1);
        assert_eq!(v["results"][0]["id"], "flat-1");
        assert_eq!(v["results"][0]["path"], "onedrive/flat-1.bin");
        assert_eq!(v["results"][0]["source"]["id"], "flat-1");
        assert_eq!(v["results"][0]["source"]["path"], "onedrive/flat-1.bin");
    }

    #[test]
    fn list_root_uses_roots_not_whole_service() {
        let calls = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let ex = RetrievalExecutor::new(RoutingArchive::new(calls.clone()));
        let v: serde_json::Value = serde_json::from_str(
            &ex.list("onedrive", Some("root"), Some(10), Some(0))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(*calls.borrow(), vec!["roots"]);
        assert_eq!(v["parent"], "root");
        assert_eq!(v["results"][0]["id"], "root-only");
    }

    #[test]
    fn list_parent_uses_children() {
        let calls = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let ex = RetrievalExecutor::new(RoutingArchive::new(calls.clone()));
        let v: serde_json::Value = serde_json::from_str(
            &ex.list("onedrive", Some("folder-1"), Some(10), Some(0))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(*calls.borrow(), vec!["children:folder-1"]);
        assert_eq!(v["parent"], "folder-1");
        assert_eq!(v["results"][0]["id"], "child-only");
    }

    #[test]
    fn list_limit_and_offset_are_applied() {
        let ex = RetrievalExecutor::new(FakeArchive {
            account: "me".into(),
            items: vec![
                FakeArchive::item("mail", "m0", "Mail 0", Some("body 0")),
                FakeArchive::item("mail", "m1", "Mail 1", Some("body 1")),
                FakeArchive::item("mail", "m2", "Mail 2", Some("body 2")),
            ],
        });
        let v: serde_json::Value =
            serde_json::from_str(&ex.list("mail", None, Some(1), Some(2)).unwrap()).unwrap();
        assert_eq!(v["limit"], 1);
        assert_eq!(v["offset"], 2);
        assert_eq!(v["service_total"], 3);
        assert_eq!(v["returned"], 1);
        assert_eq!(v["results"][0]["id"], "m2");
    }

    #[test]
    fn deep_search_still_scans_candidates_after_list_refactor() {
        let items: Vec<_> = (0..(DEFAULT_LIST_LIMIT + 5))
            .map(|i| {
                FakeArchive::item(
                    "mail",
                    &format!("bulk-{i}"),
                    &format!("Bulk {i}"),
                    Some(&format!("unmatched body {i}")),
                )
            })
            .collect();
        let ex = RetrievalExecutor::new(FakeArchive {
            account: "me".into(),
            items,
        });
        let out = ex
            .deep_search(&["mail".into()], "zzzznomatch", None, Some(40), &mut |_| {})
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["candidates_total"], DEFAULT_LIST_LIMIT + 5);
        assert_eq!(v["read"], MAX_DEEP_READS);
        assert_eq!(v["budget_reached"], true);
    }

    #[test]
    fn export_raw_passthrough_carries_source_ids() {
        let ex = fixture();
        let v: serde_json::Value = serde_json::from_str(&ex.export("mail", "m1").unwrap()).unwrap();
        assert_eq!(v["format"], "raw"); // mail → raw without the connectors feature
        assert_eq!(v["service"], "mail");
        assert_eq!(v["id"], "m1");
        assert_eq!(v["source"]["service"], "mail");
        assert_eq!(v["source"]["id"], "m1");
        assert_eq!(v["source"]["path"], "mail/m1.bin");
        assert!(v["content"].as_str().unwrap().contains("Spotify"));
    }

    #[cfg(feature = "retrieval")]
    #[test]
    fn export_calendar_returns_ics_with_source() {
        let event = serde_json::json!({
            "id": "cal-1",
            "iCalUId": "uid-1",
            "subject": "Q2 review",
            "bodyPreview": "Agenda line",
            "location": { "displayName": "Room 1" },
            "start": { "dateTime": "2026-03-01T09:00:00.0000000", "timeZone": "UTC" },
            "end": { "dateTime": "2026-03-01T10:00:00.0000000", "timeZone": "UTC" },
            "lastModifiedDateTime": "2026-02-20T08:00:00Z"
        });
        let body = event.to_string();
        let ex = RetrievalExecutor::new(FakeArchive {
            account: "me".into(),
            items: vec![FakeArchive::item(
                "calendar",
                "cal-1",
                "Q2 review",
                Some(&body),
            )],
        });
        let v: serde_json::Value =
            serde_json::from_str(&ex.export("calendar", "cal-1").unwrap()).unwrap();
        assert_eq!(v["format"], "ics");
        assert_eq!(v["source"]["service"], "calendar");
        assert_eq!(v["source"]["id"], "cal-1");
        let content = v["content"].as_str().unwrap();
        assert!(content.starts_with("BEGIN:VCALENDAR\r\nVERSION:2.0"));
        assert!(content.contains("BEGIN:VEVENT"));
        assert!(content.contains("SUMMARY:Q2 review"));
        assert!(content.contains("DTSTART:20260301T090000"));
    }

    #[cfg(feature = "retrieval")]
    #[test]
    fn export_contact_returns_vcard_with_source() {
        let contact = serde_json::json!({
            "displayName": "Ada Lovelace",
            "givenName": "Ada",
            "surname": "Lovelace",
            "emailAddresses": [{ "address": "ada@example.com", "name": "Ada" }],
            "mobilePhone": "+1 555 0100",
            "companyName": "Analytical Engines",
            "jobTitle": "Mathematician"
        });
        let body = contact.to_string();
        let ex = RetrievalExecutor::new(FakeArchive {
            account: "me".into(),
            items: vec![FakeArchive::item(
                "contacts",
                "contact-1",
                "Ada Lovelace",
                Some(&body),
            )],
        });
        let v: serde_json::Value =
            serde_json::from_str(&ex.export("contacts", "contact-1").unwrap()).unwrap();
        assert_eq!(v["format"], "vcard");
        assert_eq!(v["source"]["service"], "contacts");
        assert_eq!(v["source"]["id"], "contact-1");
        let content = v["content"].as_str().unwrap();
        assert!(content.starts_with("BEGIN:VCARD\r\nVERSION:3.0"));
        assert!(content.contains("FN:Ada Lovelace"));
        assert!(content.contains("EMAIL:ada@example.com"));
    }

    #[cfg(feature = "retrieval")]
    #[test]
    fn export_invalid_structured_body_is_tool_error() {
        let ex = RetrievalExecutor::new(FakeArchive {
            account: "me".into(),
            items: vec![FakeArchive::item(
                "calendar",
                "bad-cal",
                "Broken",
                Some("not-json"),
            )],
        });
        let err = ex.export("calendar", "bad-cal").unwrap_err();
        assert!(err.to_string().contains("export parse"));
    }

    #[cfg(feature = "retrieval")]
    #[test]
    fn store_archive_executor_covers_search_read_list_export_shapes() {
        let _guard = crate::archive::BodyKeyTestGuard::new();
        isyncyou_core::envelope::set_body_key(618_090, [90u8; 32]);

        let dir = tempfile::tempdir().unwrap();
        let store = isyncyou_store::Store::open(dir.path().join(".isyncyou-store.db")).unwrap();

        upsert_store_body(
            &store,
            dir.path(),
            isyncyou_store::Item::new("me", "mail", "m1", "Visible mail", "message"),
            "mail/aa/m1.eml",
            concat!(
                "Subject: Store runtime mail\r\n",
                "Content-Type: text/plain; charset=utf-8\r\n",
                "\r\n",
                "Hello archived mail text.\r\n"
            )
            .as_bytes(),
        );
        upsert_store_body(
            &store,
            dir.path(),
            isyncyou_store::Item::new("me", "mail", "m2", "Body only mail", "message"),
            "mail/aa/m2.eml",
            b"needle618 appears only in the indexed body",
        );
        store
            .index_body(
                "me",
                "mail",
                "m2",
                "needle618 appears only in the indexed body",
            )
            .unwrap();

        let folder = isyncyou_store::Item::new("me", "onedrive", "folder", "Folder", "folder");
        store.upsert_item(&folder).unwrap();
        let mut child = isyncyou_store::Item::new("me", "onedrive", "child", "Child.txt", "file");
        child.parent_remote_id = Some("folder".into());
        store.upsert_item(&child).unwrap();

        let event = serde_json::json!({
            "id": "cal-1",
            "iCalUId": "uid-1",
            "subject": "Store event",
            "start": { "dateTime": "2026-03-01T09:00:00.0000000", "timeZone": "UTC" },
            "end": { "dateTime": "2026-03-01T10:00:00.0000000", "timeZone": "UTC" }
        })
        .to_string();
        upsert_store_body(
            &store,
            dir.path(),
            isyncyou_store::Item::new("me", "calendar", "cal-1", "Store event", "event"),
            "calendar/cal-1.json",
            event.as_bytes(),
        );

        let contact = serde_json::json!({
            "displayName": "Ada Lovelace",
            "givenName": "Ada",
            "surname": "Lovelace",
            "emailAddresses": [{ "address": "ada@example.com" }]
        })
        .to_string();
        upsert_store_body(
            &store,
            dir.path(),
            isyncyou_store::Item::new("me", "contacts", "contact-1", "Ada Lovelace", "contact"),
            "contacts/contact-1.json",
            contact.as_bytes(),
        );
        drop(store);

        let ex = RetrievalExecutor::new(crate::archive::StoreArchive::new("me", dir.path()));

        let search = ToolAction::Search {
            account: "me".into(),
            services: vec!["mail".into()],
            query: "needle618".into(),
            limit: Some(10),
        };
        let search_out: serde_json::Value =
            serde_json::from_str(&ex.execute_read(&search).unwrap()).unwrap();
        assert_eq!(search_out["results"][0]["id"], "m2");
        assert_eq!(search_out["results"][0]["source"]["service"], "mail");
        assert!(search_out["results"][0]["snippet"]
            .as_str()
            .unwrap()
            .contains("needle618"));

        let read = ToolAction::Read {
            account: "me".into(),
            service: "mail".into(),
            id: "m1".into(),
            max_bytes: None,
        };
        let read_out: serde_json::Value =
            serde_json::from_str(&ex.execute_read(&read).unwrap()).unwrap();
        assert_eq!(read_out["content_kind"], "mail-text");
        assert_eq!(read_out["source"]["path"], "mail/aa/m1.eml");
        assert!(read_out["content"]
            .as_str()
            .unwrap()
            .contains("Hello archived mail text."));
        assert!(!read_out["content"]
            .as_str()
            .unwrap()
            .contains("Content-Type:"));

        let list_root = ToolAction::List {
            account: "me".into(),
            service: "onedrive".into(),
            parent: Some("root".into()),
            limit: Some(10),
            offset: Some(0),
        };
        let root_out: serde_json::Value =
            serde_json::from_str(&ex.execute_read(&list_root).unwrap()).unwrap();
        assert_eq!(root_out["results"][0]["id"], "folder");
        assert_eq!(root_out["results"][0]["source"]["id"], "folder");

        let list_child = ToolAction::List {
            account: "me".into(),
            service: "onedrive".into(),
            parent: Some("folder".into()),
            limit: Some(10),
            offset: Some(0),
        };
        let child_out: serde_json::Value =
            serde_json::from_str(&ex.execute_read(&list_child).unwrap()).unwrap();
        assert_eq!(child_out["results"][0]["id"], "child");

        let calendar = ToolAction::Export {
            account: "me".into(),
            service: "calendar".into(),
            id: "cal-1".into(),
        };
        let calendar_out: serde_json::Value =
            serde_json::from_str(&ex.execute_read(&calendar).unwrap()).unwrap();
        assert_eq!(calendar_out["format"], "ics");
        assert!(calendar_out["content"]
            .as_str()
            .unwrap()
            .contains("BEGIN:VCALENDAR"));

        let contacts = ToolAction::Export {
            account: "me".into(),
            service: "contacts".into(),
            id: "contact-1".into(),
        };
        let contacts_out: serde_json::Value =
            serde_json::from_str(&ex.execute_read(&contacts).unwrap()).unwrap();
        assert_eq!(contacts_out["format"], "vcard");
        assert!(contacts_out["content"]
            .as_str()
            .unwrap()
            .contains("EMAIL:ada@example.com"));
    }

    #[test]
    fn destructive_action_is_refused_by_the_read_executor() {
        let ex = fixture();
        let backup = ToolAction::Backup {
            account: "me".into(),
            services: vec![],
        };
        let err = ex.execute_read(&backup).unwrap_err();
        assert!(err.to_string().contains("destructive"));
    }

    #[test]
    fn execute_read_dispatches_search_via_tooaction() {
        let ex = fixture();
        let action = ToolAction::Search {
            account: "me".into(),
            services: vec![],
            query: "spotify".into(),
            limit: None,
        };
        let out = ex.execute_read(&action).unwrap();
        assert!(out.contains("total_matches"));
    }

    #[test]
    fn execute_read_rejects_account_mismatch() {
        let ex = fixture();
        let action = ToolAction::Search {
            account: "other".into(),
            services: vec![],
            query: "spotify".into(),
            limit: None,
        };
        let err = ex.execute_read(&action).unwrap_err();
        assert!(err.to_string().contains("account mismatch"));
    }
}
