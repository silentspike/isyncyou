//! Live OneNote **write** layer (#568 A5): the live client's page verbs — create
//! (in a section), delete, and a best-effort content append — behind [`PageWriter`]
//! so the engine wiring + the daemon handler are unit-tested deterministically
//! without a network; `GraphClient` is the real impl.
//!
//! Like `calendar_live`/`contacts_live`/`task_live`, this is the *interactive* path:
//! the user creates/deletes a page or appends text in the live client and the change
//! is pushed straight to Microsoft 365. The write token is the full restore-scope
//! token (`Notes.ReadWrite`, from #558). Graph's OneNote write is **command-based**:
//! create posts page HTML, append is a `PATCH .../content` update-command — there is
//! no full page rewrite, so the append is honestly best-effort.

use isyncyou_core::Config;
use serde_json::json;

/// The live OneNote write operations, object-safe so the daemon can hold a
/// `&dyn PageWriter` and tests can swap in a fake.
pub trait PageWriter {
    /// Create a page **in a section** from POST-ready HTML; returns the new cloud id.
    fn create(&self, section_id: &str, html: &[u8]) -> Result<String, String>;
    /// Delete a page.
    fn delete(&self, page_id: &str) -> Result<(), String>;
    /// Append a plain-text paragraph to a page's body (best-effort; Graph's content
    /// write is a command-based PATCH, not a full rewrite). The text is HTML-escaped.
    fn append(&self, page_id: &str, text: &str) -> Result<(), String>;
}

/// Escape the four HTML-significant characters so appended user text can't inject
/// markup into the page body.
fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

// Inherent GraphClient methods share names with the trait (`create`/`delete`), so
// each delegation is fully qualified to call the inherent (HTTP) method, never recurse.
impl PageWriter for isyncyou_graph::GraphClient {
    fn create(&self, section_id: &str, html: &[u8]) -> Result<String, String> {
        let v = isyncyou_graph::GraphClient::create_onenote_page_in_section(self, section_id, html)
            .map_err(|e| e.to_string())?;
        v.get("id")
            .and_then(|i| i.as_str())
            .map(String::from)
            .ok_or_else(|| "created page response has no id".to_string())
    }
    fn delete(&self, page_id: &str) -> Result<(), String> {
        isyncyou_graph::GraphClient::delete_onenote_page(self, page_id).map_err(|e| e.to_string())
    }
    fn append(&self, page_id: &str, text: &str) -> Result<(), String> {
        let commands = json!([{
            "target": "body",
            "action": "append",
            "content": format!("<p>{}</p>", escape_html(text)),
        }]);
        isyncyou_graph::GraphClient::append_onenote_page_content(self, page_id, &commands)
            .map_err(|e| e.to_string())
    }
}

/// Resolve the full write token (restore scopes incl. `Notes.ReadWrite`) and build a
/// ready `GraphClient` for the live-OneNote write ops. The token is silently
/// refreshed from the cached `login --write`; a missing cache is an error. This is
/// the daemon's entry point into the layer.
pub fn page_writer(cfg: &Config, account: &str) -> Result<isyncyou_graph::GraphClient, String> {
    let token = crate::auth::resolve_cached_restore_token(cfg, account)?;
    Ok(isyncyou_graph::GraphClient::new(token))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[derive(Default)]
    struct FakePages {
        log: RefCell<Vec<String>>,
    }
    impl PageWriter for FakePages {
        fn create(&self, section_id: &str, html: &[u8]) -> Result<String, String> {
            self.log
                .borrow_mut()
                .push(format!("create section={section_id} bytes={}", html.len()));
            Ok("page-new".into())
        }
        fn delete(&self, page_id: &str) -> Result<(), String> {
            self.log.borrow_mut().push(format!("delete id={page_id}"));
            Ok(())
        }
        fn append(&self, page_id: &str, text: &str) -> Result<(), String> {
            self.log
                .borrow_mut()
                .push(format!("append id={page_id} text={text}"));
            Ok(())
        }
    }

    #[test]
    fn page_writer_is_object_safe_and_ops_carry_args() {
        let f = FakePages::default();
        let w: &dyn PageWriter = &f; // compiles only if the trait is object-safe
        assert_eq!(w.create("S1", b"<html></html>").unwrap(), "page-new");
        w.delete("P1").unwrap();
        w.append("P1", "more notes").unwrap();
        let log = f.log.borrow();
        assert_eq!(log[0], "create section=S1 bytes=13");
        assert_eq!(log[1], "delete id=P1");
        assert_eq!(log[2], "append id=P1 text=more notes");
    }

    #[test]
    fn append_escapes_html_in_the_user_text() {
        assert_eq!(escape_html("a<b>&c"), "a&lt;b&gt;&amp;c");
    }
}
