//! The read-class tool executor, backed by an [`ArchiveSource`]. It implements
//! [`crate::ToolExecutor`] for `Search`/`Read`/`List`/`Export`, returning JSON whose
//! every result carries `{service, id, path}` source citations (REQ-AGENT-009) and
//! honours a `max_bytes` read budget. The logic is generic over `ArchiveSource`, so it
//! is tested with an in-memory fake (no store).

use crate::archive::{ArchiveSource, ItemRef};
use crate::tool::{ToolAction, ToolClass};
use crate::turn::ToolExecutor;
use crate::AgentError;

/// Default read budget when the model does not set `max_bytes` (64 KiB).
pub const DEFAULT_READ_BUDGET: u64 = 64 * 1024;
/// Default search result cap when the model does not set `limit`.
pub const DEFAULT_SEARCH_LIMIT: u32 = 20;

/// Executes read-class actions against an [`ArchiveSource`].
pub struct RetrievalExecutor<A: ArchiveSource> {
    source: A,
}

impl<A: ArchiveSource> RetrievalExecutor<A> {
    pub fn new(source: A) -> Self {
        Self { source }
    }

    fn source_ref(it: &ItemRef) -> serde_json::Value {
        serde_json::json!({
            "service": it.service,
            "id": it.id,
            "name": it.name,
            "item_type": it.item_type,
            "path": it.path,
        })
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
        let results: Vec<serde_json::Value> = hits.iter().take(cap).map(Self::source_ref).collect();
        Ok(serde_json::json!({
            "query": query,
            "returned": results.len(),
            "total_matches": total,
            "results": results,
        })
        .to_string())
    }

    fn read(&self, service: &str, id: &str, max_bytes: Option<u64>) -> Result<String, AgentError> {
        let item = self
            .source
            .get(service, id)?
            .ok_or_else(|| AgentError::ToolArgs(format!("no item {service}/{id}")))?;
        let bytes = self.source.read_body(service, id)?;
        let budget = max_bytes.unwrap_or(DEFAULT_READ_BUDGET) as usize;
        let truncated = bytes.len() > budget;
        let slice = if truncated {
            &bytes[..budget]
        } else {
            &bytes[..]
        };
        Ok(serde_json::json!({
            "service": item.service,
            "id": item.id,
            "name": item.name,
            "path": item.path,
            "bytes_total": bytes.len(),
            "bytes_returned": slice.len(),
            "truncated": truncated,
            "content": String::from_utf8_lossy(slice),
        })
        .to_string())
    }

    fn list(&self, service: &str, parent: Option<&str>) -> Result<String, AgentError> {
        let items = self.source.list(service, parent)?;
        let count = self.source.count(service)?;
        let results: Vec<serde_json::Value> = items.iter().map(Self::source_ref).collect();
        Ok(serde_json::json!({
            "service": service,
            "parent": parent,
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
        Ok(serde_json::json!({
            "service": item.service,
            "id": item.id,
            "path": item.path,
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
        match action {
            ToolAction::Search {
                services,
                query,
                limit,
                ..
            } => self.search(services, query, *limit),
            ToolAction::Read {
                service,
                id,
                max_bytes,
                ..
            } => self.read(service, id, *max_bytes),
            ToolAction::List {
                service, parent, ..
            } => self.list(service, parent.as_deref()),
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
}

#[cfg(test)]
mod tests {
    use super::*;

    /// In-memory archive for testing the executor logic without a store.
    struct FakeArchive {
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
        fn list(&self, service: &str, _parent: Option<&str>) -> Result<Vec<ItemRef>, AgentError> {
            Ok(self
                .items
                .iter()
                .filter(|(i, _)| i.service == service)
                .map(|(i, _)| i.clone())
                .collect())
        }
        fn count(&self, service: &str) -> Result<u64, AgentError> {
            Ok(self
                .items
                .iter()
                .filter(|(i, _)| i.service == service)
                .count() as u64)
        }
    }

    fn fixture() -> RetrievalExecutor<FakeArchive> {
        RetrievalExecutor::new(FakeArchive {
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
        }
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
    fn list_reports_items_and_service_total() {
        let ex = fixture();
        let v: serde_json::Value = serde_json::from_str(&ex.list("mail", None).unwrap()).unwrap();
        assert_eq!(v["service_total"], 2);
        assert_eq!(v["results"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn export_raw_passthrough_carries_source_ids() {
        let ex = fixture();
        let v: serde_json::Value = serde_json::from_str(&ex.export("mail", "m1").unwrap()).unwrap();
        assert_eq!(v["format"], "raw"); // mail → raw without the connectors feature
        assert_eq!(v["service"], "mail");
        assert_eq!(v["id"], "m1");
        assert!(v["content"].as_str().unwrap().contains("Spotify"));
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
}
