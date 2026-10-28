//! Headless mail capture using the shared JMAP connector, not another client.
use isyncyou_connectors::jmap::{JmapClient, JmapError};
use isyncyou_store::{ProviderChange, ProviderObject, ProviderScope, Store};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::path::Path;

type Result<T> = std::result::Result<T, Box<dyn Error>>;
const PAGE: usize = 16;
const MAX_ENVELOPE: usize = 64 * 1024 * 1024;

trait MailSource {
    fn get(&self, kind: &str, ids: Option<&[String]>) -> Result<Value>;
    fn query(&self, position: usize, limit: usize) -> Result<Value>;
    fn changes(&self, since: &str) -> std::result::Result<Value, JmapError>;
    fn download(&self, blob: &str) -> Result<Vec<u8>>;
    fn max_get(&self) -> usize;
}
impl MailSource for JmapClient {
    fn get(&self, kind: &str, ids: Option<&[String]>) -> Result<Value> {
        Ok(JmapClient::get(self, kind, ids, None)?)
    }
    fn query(&self, position: usize, limit: usize) -> Result<Value> {
        Ok(self.query_emails(position, limit, None)?)
    }
    fn changes(&self, since: &str) -> std::result::Result<Value, JmapError> {
        JmapClient::changes(self, "Email", since, 1000)
    }
    fn download(&self, blob: &str) -> Result<Vec<u8>> {
        Ok(JmapClient::download(
            self,
            blob,
            "message.eml",
            "message/rfc822",
        )?)
    }
    fn max_get(&self) -> usize {
        self.max_objects_in_get()
    }
}

fn invalid() -> Box<dyn Error> {
    "incomplete or inconsistent JMAP capture".into()
}
fn field<'a>(v: &'a Value, name: &str) -> Result<&'a str> {
    v.get(name)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(invalid)
}
fn ids(v: &Value, name: &str) -> Result<Vec<String>> {
    v.get(name)
        .and_then(Value::as_array)
        .ok_or_else(invalid)?
        .iter()
        .map(|x| {
            x.as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .ok_or_else(invalid)
        })
        .collect()
}

pub struct Budget<'a> {
    pub path: &'a Path,
    pub remaining: u64,
    pub reserve: u64,
}
impl Budget<'_> {
    fn admit(&mut self, bytes: usize) -> Result<()> {
        let required = (bytes as u64).saturating_mul(4);
        if required > self.remaining
            || fs2::available_space(self.path)? < required.saturating_add(self.reserve)
        {
            return Err("export storage capacity unavailable".into());
        }
        self.remaining -= required;
        Ok(())
    }
}

fn capture_objects(
    client: &impl MailSource,
    requested: &[String],
    expected_state: &str,
    budget: &mut Budget<'_>,
) -> Result<Vec<ProviderChange>> {
    if requested.is_empty() {
        return Ok(Vec::new());
    }
    let reply = client.get("Email", Some(requested))?;
    if field(&reply, "state")? != expected_state || !ids(&reply, "notFound")?.is_empty() {
        return Err(invalid());
    }
    let list = reply
        .get("list")
        .and_then(Value::as_array)
        .ok_or_else(invalid)?;
    if list.len() != requested.len() {
        return Err(invalid());
    }
    let expected: HashSet<&str> = requested.iter().map(String::as_str).collect();
    let mut seen = HashSet::new();
    let mut changes = Vec::new();
    for object in list {
        let id = field(object, "id")?;
        let blob = field(object, "blobId")?;
        if !expected.contains(id) || !seen.insert(id.to_owned()) {
            return Err(invalid());
        }
        // Preserve every native metadata field plus the full, unmodified MIME.
        let metadata = serde_json::to_vec(object)?;
        if metadata.len() > 1024 * 1024 {
            return Err(invalid());
        }
        let mime = client.download(blob)?;
        if metadata.len().saturating_add(mime.len()).saturating_add(16) > MAX_ENVELOPE {
            return Err(invalid());
        }
        budget.admit(metadata.len() + mime.len() + 16)?;
        let mut raw = b"ISYNC-MIME-1\n".to_vec();
        raw.extend_from_slice(&(metadata.len() as u32).to_be_bytes());
        raw.extend_from_slice(&metadata);
        raw.extend_from_slice(&mime);
        changes.push(ProviderChange::Upsert(ProviderObject {
            remote_id: id.to_owned(),
            etag: Some(blob.to_owned()),
            raw,
        }));
    }
    Ok(changes)
}

fn initial(
    client: &impl MailSource,
    store: &Store,
    scope: &ProviderScope,
    budget: &mut Budget<'_>,
) -> Result<()> {
    let initial = client.get("Email", Some(&[]))?;
    let email_state = field(&initial, "state")?.to_owned();
    let marker = format!("initial:{email_state}");
    let previous = store.provider_cursor(scope)?;
    if !store.begin_provider_reconcile(scope, previous.as_deref(), &marker)? {
        return Err(invalid());
    }
    let mut position = 0usize;
    let mut query_state: Option<String> = None;
    let mut seen = HashSet::new();
    loop {
        let page = client.query(position, PAGE)?;
        let page_state = field(&page, "queryState")?;
        if query_state.as_deref().is_some_and(|x| x != page_state) {
            return Err(invalid());
        }
        query_state.get_or_insert_with(|| page_state.to_owned());
        let page_ids = ids(&page, "ids")?;
        let total = page
            .get("total")
            .and_then(Value::as_u64)
            .ok_or_else(invalid)? as usize;
        if total > 200_000
            || page_ids.iter().any(|id| !seen.insert(id.clone()))
            || position + page_ids.len() > total
        {
            return Err(invalid());
        }
        // One MIME at a time; the cursor remains incomplete until final validation.
        for id in &page_ids {
            let objects = capture_objects(client, std::slice::from_ref(id), &email_state, budget)?;
            store.apply_provider_page(scope, &objects, &marker)?;
        }
        if page_ids.is_empty() {
            store.apply_provider_page(scope, &[], &marker)?;
        }
        position += page_ids.len();
        if position == total {
            break;
        }
        if page_ids.is_empty() {
            return Err(invalid());
        }
    }
    if field(&client.get("Email", Some(&[]))?, "state")? != email_state {
        return Err(invalid());
    }
    // Reconciliation happens only after a stable complete enumeration.
    let mut after = String::new();
    loop {
        let objects = store.provider_objects_page(scope, &after, 16)?;
        if objects.is_empty() {
            break;
        }
        after = objects.last().ok_or_else(invalid)?.remote_id.clone();
        let removed: Vec<_> = objects
            .into_iter()
            .filter(|x| !seen.contains(&x.remote_id))
            .map(|x| ProviderChange::Destroyed(x.remote_id))
            .collect();
        if !removed.is_empty() {
            store.apply_provider_page(scope, &removed, &marker)?;
        }
    }
    if !store.mark_provider_complete(scope, &marker, &email_state)? {
        return Err(invalid());
    }
    Ok(())
}

fn delta(
    client: &impl MailSource,
    store: &Store,
    scope: &ProviderScope,
    original: &str,
    budget: &mut Budget<'_>,
) -> Result<()> {
    let mut cursor = original.to_owned();
    let mut changed = HashMap::<String, bool>::new();
    for _ in 0..10000 {
        let page = match client.changes(&cursor) {
            Ok(page) => page,
            Err(JmapError::Method(ref kind)) if kind == "cannotCalculateChanges" => {
                return initial(client, store, scope, budget)
            }
            Err(error) => return Err(error.into()),
        };
        if field(&page, "oldState")? != cursor {
            return Err(invalid());
        }
        for id in ids(&page, "created")?
            .into_iter()
            .chain(ids(&page, "updated")?)
        {
            changed.insert(id, true);
        }
        for id in ids(&page, "destroyed")? {
            changed.insert(id, false);
        }
        if changed.len() > 200_000 {
            return Err(invalid());
        }
        let next = field(&page, "newState")?.to_owned();
        let more = page
            .get("hasMoreChanges")
            .and_then(Value::as_bool)
            .ok_or_else(invalid)?;
        if more && next == cursor {
            return Err(invalid());
        }
        cursor = next;
        if !more {
            let live: Vec<String> = changed
                .iter()
                .filter_map(|(id, live)| live.then_some(id.clone()))
                .collect();
            for chunk in live.chunks(1) {
                let page = capture_objects(client, chunk, &cursor, budget)?;
                // Until every blob is durable, preserve the original cursor.
                store.apply_provider_page(scope, &page, original)?;
            }
            let removed: Vec<_> = changed
                .into_iter()
                .filter_map(|(id, live)| (!live).then_some(ProviderChange::Destroyed(id)))
                .collect();
            if field(&client.get("Email", Some(&[]))?, "state")? != cursor {
                return Err(invalid());
            }
            store.apply_provider_page(scope, &removed, &cursor)?;
            return Ok(());
        }
    }
    Err(invalid())
}

fn sync_source(
    client: &impl MailSource,
    store: &Store,
    scope: &ProviderScope,
    budget: &mut Budget<'_>,
) -> Result<()> {
    match store.provider_cursor(scope)? {
        None => initial(client, store, scope, budget),
        Some(cursor) if cursor.starts_with("initial:") => initial(client, store, scope, budget),
        Some(cursor) => delta(client, store, scope, &cursor, budget),
    }
}

pub fn sync(
    client: &JmapClient,
    store: &Store,
    scope: &ProviderScope,
    budget: &mut Budget<'_>,
) -> Result<()> {
    sync_source(client, store, scope, budget)
}

pub fn sync_mailboxes(client: &JmapClient, store: &Store, scope: &ProviderScope) -> Result<()> {
    let page = JmapClient::get(client, "Mailbox", None, None)?;
    let state = field(&page, "state")?;
    let list = page
        .get("list")
        .and_then(Value::as_array)
        .ok_or_else(invalid)?;
    if list.len() >= client.max_get() || !ids(&page, "notFound")?.is_empty() {
        return Err(invalid());
    }
    let mut seen = HashSet::new();
    let mut objects = Vec::new();
    for item in list {
        let id = field(item, "id")?;
        if !seen.insert(id.to_owned()) {
            return Err(invalid());
        }
        objects.push(ProviderObject {
            remote_id: id.into(),
            etag: None,
            raw: serde_json::to_vec(item)?,
        });
    }
    store.reconcile_provider_snapshot(scope, &objects, state)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Fixture {
        fail_download: bool,
    }
    impl MailSource for Fixture {
        fn get(&self, _kind: &str, ids: Option<&[String]>) -> Result<Value> {
            let list = if ids.unwrap_or(&[]).is_empty() {
                vec![]
            } else {
                vec![
                    json!({"id":"one","blobId":"blob1","mailboxIds":{"inbox":true},"keywords":{"$seen":true}}),
                ]
            };
            Ok(json!({"state":"state1", "list":list, "notFound":[]}))
        }
        fn query(&self, position: usize, _limit: usize) -> Result<Value> {
            Ok(
                json!({"queryState":"q1", "total":1, "ids":if position == 0 { vec!["one"] } else { vec![] }}),
            )
        }
        fn changes(&self, _since: &str) -> std::result::Result<Value, JmapError> {
            Err(JmapError::InvalidResponse)
        }
        fn download(&self, _blob: &str) -> Result<Vec<u8>> {
            if self.fail_download {
                Err("simulated failure".into())
            } else {
                Ok(b"Subject: original\r\n\r\nbody".to_vec())
            }
        }
        fn max_get(&self) -> usize {
            16
        }
    }

    fn scope() -> ProviderScope {
        ProviderScope {
            account_id: "a".into(),
            origin: "https://example.test/".into(),
            service: "mail".into(),
            collection: "a".into(),
        }
    }

    #[test]
    fn complete_initial_capture_preserves_mime_and_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_in_memory().unwrap();
        let mut budget = Budget {
            path: dir.path(),
            remaining: 1024 * 1024,
            reserve: 0,
        };
        sync_source(
            &Fixture {
                fail_download: false,
            },
            &store,
            &scope(),
            &mut budget,
        )
        .unwrap();
        let object = store.provider_object(&scope(), "one").unwrap().unwrap();
        assert!(object.raw.starts_with(b"ISYNC-MIME-1\n"));
        assert!(object.raw.ends_with(b"Subject: original\r\n\r\nbody"));
        assert_eq!(
            store.provider_cursor(&scope()).unwrap().as_deref(),
            Some("state1")
        );
        let snapshot = store
            .provider_export_snapshot("a", "https://example.test/")
            .unwrap();
        assert!(snapshot.collections[0].complete);
    }

    #[test]
    fn failed_mime_keeps_initial_cursor_incomplete() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_in_memory().unwrap();
        let mut budget = Budget {
            path: dir.path(),
            remaining: 1024 * 1024,
            reserve: 0,
        };
        assert!(sync_source(
            &Fixture {
                fail_download: true
            },
            &store,
            &scope(),
            &mut budget
        )
        .is_err());
        assert_eq!(
            store.provider_cursor(&scope()).unwrap().as_deref(),
            Some("initial:state1")
        );
        assert!(
            !store
                .provider_export_snapshot("a", "https://example.test/")
                .unwrap()
                .collections[0]
                .complete
        );
    }
}
