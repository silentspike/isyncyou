//! Live OneDrive **listing** layer (Mode 1 online, #647): a folder's children
//! read straight from Graph, fully paged, with **no store write** — the browse
//! primitive for online-mode folders. Behind [`OneDriveLister`] so the daemon
//! handler (S-OM.2) can hold a `&dyn OneDriveLister` and tests can swap in a
//! fake; `GraphClient` is the real impl.
//!
//! Unlike the sync/offline modes, an online listing never touches the store — it
//! is a pure pass-through to Graph. The read-capable token is resolved from the
//! cached login (mobile-friendly: the read token, else the read-capable
//! write/restore token, #89).

use isyncyou_core::Config;
use serde_json::Value;

/// Live per-folder OneDrive listing, object-safe so the daemon can hold a
/// `&dyn OneDriveLister` and tests can swap in a fake.
pub trait OneDriveLister {
    /// List a folder's children live from Graph (an empty id = the drive root),
    /// fully paged. No store row is written.
    fn list_children(&self, folder_id: &str) -> Result<Vec<Value>, String>;
}

// The inherent GraphClient method shares this name, so the delegation is fully
// qualified to call the inherent (HTTP) method, never recurse.
impl OneDriveLister for isyncyou_graph::GraphClient {
    fn list_children(&self, folder_id: &str) -> Result<Vec<Value>, String> {
        isyncyou_graph::GraphClient::list_children(self, folder_id).map_err(|e| e.to_string())
    }
}

/// Resolve a read-capable token (mobile-friendly: the cached read token, else the
/// read-capable write/restore token — #89) and build a ready `GraphClient` for
/// live online-mode browsing. The token is silently refreshed from the cached
/// login; a missing cache is an error. The daemon's entry point into the layer.
pub fn onedrive_lister(cfg: &Config, account: &str) -> Result<isyncyou_graph::GraphClient, String> {
    let token = crate::auth::resolve_cache_refresh_token(cfg, account)?;
    Ok(isyncyou_graph::GraphClient::new(token))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::cell::RefCell;

    #[derive(Default)]
    struct FakeDrive {
        log: RefCell<Vec<String>>,
    }
    impl OneDriveLister for FakeDrive {
        fn list_children(&self, folder_id: &str) -> Result<Vec<Value>, String> {
            self.log
                .borrow_mut()
                .push(format!("list folder={folder_id}"));
            Ok(vec![json!({ "id": "c1", "name": "a.txt" })])
        }
    }

    #[test]
    fn onedrive_lister_is_object_safe_and_returns_children() {
        let f = FakeDrive::default();
        let l: &dyn OneDriveLister = &f; // compiles only if the trait is object-safe
        let kids = l.list_children("ROOT").unwrap();
        assert_eq!(kids.len(), 1);
        assert_eq!(kids[0]["name"], "a.txt");
        assert_eq!(f.log.borrow()[0], "list folder=ROOT");
    }

    /// Minimal std-only one-shot HTTP/1.1 server so the *real* `GraphClient`
    /// listing path can be exercised offline (the engine crate has no dev-deps /
    /// mock-http lib). Drains the request head, serves one 200 body, closes.
    fn serve_once(body: &str) -> (String, std::thread::JoinHandle<()>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let handle = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            // Drain the request head (a GET — no body) so the client's write completes.
            let mut buf = Vec::new();
            let mut byte = [0u8; 1];
            while !buf.ends_with(b"\r\n\r\n") {
                if sock.read(&mut byte).unwrap_or(0) == 0 {
                    break;
                }
                buf.push(byte[0]);
            }
            sock.write_all(resp.as_bytes()).unwrap();
        });
        (format!("http://{addr}"), handle)
    }

    #[test]
    fn list_children_writes_no_store_row() {
        // AC3 (#647): a live online listing writes NOTHING to the store. Run the
        // real GraphClient path against a mock Graph endpoint with an in-memory
        // store in scope; assert the store stays empty before AND after.
        use isyncyou_store::Store;
        let store = Store::open_in_memory().unwrap();
        assert_eq!(store.count_by_service("me", "onedrive").unwrap(), 0);

        let (base, h) = serve_once(r#"{"value":[{"id":"x","name":"f.txt"}]}"#);
        let client = isyncyou_graph::GraphClient::new("tok").with_base_url(&base);
        let kids = OneDriveLister::list_children(&client, "FOLDER").unwrap();
        h.join().unwrap();

        assert_eq!(
            kids.len(),
            1,
            "the live listing returns the folder's children"
        );
        assert_eq!(kids[0]["name"], "f.txt");
        // The whole point of Mode 1: no store row is written.
        assert_eq!(store.count_by_service("me", "onedrive").unwrap(), 0);
    }
}
