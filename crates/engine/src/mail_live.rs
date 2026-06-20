//! Live mail **write** layer (#561): the live client's verbs — send, reply,
//! forward, move, mark read, flag, categorize, draft — abstracted behind
//! [`MailWriter`] so the engine wiring + the daemon handler are unit-tested
//! deterministically without a network; `GraphClient` is the real impl.
//!
//! Unlike `mail_restore` (a crash-safe, ledger-backed re-creation of archived
//! mail), this is the *interactive* path: the user composes/acts in the live
//! client and the change is pushed straight to Microsoft 365. The write token is
//! the full restore-scope token (`Mail.ReadWrite` + `Mail.Send`, from #558),
//! resolved from the cached `login --write`.

use isyncyou_core::Config;
use serde_json::{json, Value};

/// The live-mail write operations, object-safe so the daemon can hold a
/// `&dyn MailWriter` and tests can swap in a fake.
pub trait MailWriter {
    /// Compose and send a new message; `save_to_sent` is implied true.
    #[allow(clippy::too_many_arguments)] // a compose genuinely has many fields
    fn send_new(
        &self,
        subject: &str,
        body_html: &str,
        to: &[String],
        cc: &[String],
        bcc: &[String],
        importance: Option<&str>,
        request_read_receipt: bool,
    ) -> Result<(), String>;
    /// Reply to the sender (`all = false`) or all recipients (`all = true`).
    fn reply(&self, message_id: &str, comment: &str, all: bool) -> Result<(), String>;
    /// Forward a message to new recipients with an optional comment.
    fn forward(&self, message_id: &str, comment: &str, to: &[String]) -> Result<(), String>;
    /// Move a message to another folder; returns its new id in the destination.
    fn move_to(&self, message_id: &str, destination_id: &str) -> Result<String, String>;
    /// Mark a message read/unread.
    fn set_read(&self, message_id: &str, is_read: bool) -> Result<(), String>;
    /// Set/clear a follow-up flag (`notFlagged` / `flagged` / `complete`).
    fn set_flag(&self, message_id: &str, flag_status: &str) -> Result<(), String>;
    /// Replace a message's categories.
    fn set_categories(&self, message_id: &str, categories: &[String]) -> Result<(), String>;
    /// Create a draft; returns the new draft's id.
    fn create_draft(&self, subject: &str, body_html: &str, to: &[String])
        -> Result<String, String>;
    /// Send an existing draft by id.
    fn send_draft(&self, message_id: &str) -> Result<(), String>;
}

/// Build the Graph `message` resource from the live client's simple inputs
/// (mirrors the reference `server.py` shape). Pure + unit-tested for shape; `cc`
/// and `bcc` are omitted entirely when empty (Graph treats absent and `[]` the
/// same, and omitting keeps the payload minimal). `importance` (`low`/`high`) and
/// a read-receipt request are added only when set (#563 compose).
pub fn build_message(
    subject: &str,
    body_html: &str,
    to: &[String],
    cc: &[String],
    bcc: &[String],
    importance: Option<&str>,
    request_read_receipt: bool,
) -> Value {
    let recips = |addrs: &[String]| -> Vec<Value> {
        addrs
            .iter()
            .map(|a| json!({ "emailAddress": { "address": a } }))
            .collect()
    };
    let mut m = json!({
        "subject": subject,
        "body": { "contentType": "HTML", "content": body_html },
        "toRecipients": recips(to),
    });
    if !cc.is_empty() {
        m["ccRecipients"] = Value::Array(recips(cc));
    }
    if !bcc.is_empty() {
        m["bccRecipients"] = Value::Array(recips(bcc));
    }
    if let Some(imp) = importance.filter(|i| !i.is_empty()) {
        m["importance"] = json!(imp);
    }
    if request_read_receipt {
        m["isReadReceiptRequested"] = json!(true);
    }
    m
}

// Inherent GraphClient methods share names with several trait methods, so every
// delegation is fully qualified to call the inherent (HTTP) method, never recurse.
impl MailWriter for isyncyou_graph::GraphClient {
    #[allow(clippy::too_many_arguments)]
    fn send_new(
        &self,
        subject: &str,
        body_html: &str,
        to: &[String],
        cc: &[String],
        bcc: &[String],
        importance: Option<&str>,
        request_read_receipt: bool,
    ) -> Result<(), String> {
        let msg = build_message(
            subject,
            body_html,
            to,
            cc,
            bcc,
            importance,
            request_read_receipt,
        );
        isyncyou_graph::GraphClient::send_mail(self, &msg, true).map_err(|e| e.to_string())
    }
    fn reply(&self, message_id: &str, comment: &str, all: bool) -> Result<(), String> {
        if all {
            isyncyou_graph::GraphClient::reply_all(self, message_id, comment)
        } else {
            isyncyou_graph::GraphClient::reply(self, message_id, comment)
        }
        .map_err(|e| e.to_string())
    }
    fn forward(&self, message_id: &str, comment: &str, to: &[String]) -> Result<(), String> {
        let to_refs: Vec<&str> = to.iter().map(String::as_str).collect();
        isyncyou_graph::GraphClient::forward(self, message_id, comment, &to_refs)
            .map_err(|e| e.to_string())
    }
    fn move_to(&self, message_id: &str, destination_id: &str) -> Result<String, String> {
        let v = isyncyou_graph::GraphClient::move_message(self, message_id, destination_id)
            .map_err(|e| e.to_string())?;
        Ok(v.get("id")
            .and_then(Value::as_str)
            .map(String::from)
            .unwrap_or_default())
    }
    fn set_read(&self, message_id: &str, is_read: bool) -> Result<(), String> {
        isyncyou_graph::GraphClient::set_read(self, message_id, is_read)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
    fn set_flag(&self, message_id: &str, flag_status: &str) -> Result<(), String> {
        isyncyou_graph::GraphClient::set_flag(self, message_id, flag_status)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
    fn set_categories(&self, message_id: &str, categories: &[String]) -> Result<(), String> {
        isyncyou_graph::GraphClient::set_categories(self, message_id, categories)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
    fn create_draft(
        &self,
        subject: &str,
        body_html: &str,
        to: &[String],
    ) -> Result<String, String> {
        let msg = build_message(subject, body_html, to, &[], &[], None, false);
        let v = isyncyou_graph::GraphClient::create_draft(self, &msg).map_err(|e| e.to_string())?;
        v.get("id")
            .and_then(Value::as_str)
            .map(String::from)
            .ok_or_else(|| "created draft response has no id".to_string())
    }
    fn send_draft(&self, message_id: &str) -> Result<(), String> {
        isyncyou_graph::GraphClient::send_draft(self, message_id).map_err(|e| e.to_string())
    }
}

/// Resolve the full write token (restore scopes: `Mail.ReadWrite` + `Mail.Send`)
/// and build a ready `GraphClient` for the live-mail write ops. The token is
/// silently refreshed from the cached `login --write`; a missing cache is an error
/// (the user must log in once). This is the daemon's entry point into the layer.
pub fn mail_writer(cfg: &Config, account: &str) -> Result<isyncyou_graph::GraphClient, String> {
    let token = crate::auth::resolve_cached_restore_token(cfg, account)?;
    Ok(isyncyou_graph::GraphClient::new(token))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn build_message_shapes_recipients_and_omits_empty_cc_bcc() {
        let m = build_message(
            "Hello",
            "<p>hi</p>",
            &["a@b.com".into()],
            &[],
            &[],
            None,
            false,
        );
        assert_eq!(m["subject"], "Hello");
        assert_eq!(m["body"]["contentType"], "HTML");
        assert_eq!(m["body"]["content"], "<p>hi</p>");
        assert_eq!(m["toRecipients"][0]["emailAddress"]["address"], "a@b.com");
        assert!(m.get("ccRecipients").is_none(), "empty cc must be omitted");
        assert!(
            m.get("bccRecipients").is_none(),
            "empty bcc must be omitted"
        );
        // no importance / read-receipt unless requested
        assert!(m.get("importance").is_none());
        assert!(m.get("isReadReceiptRequested").is_none());

        let m2 = build_message(
            "Hi",
            "x",
            &["t@x.com".into()],
            &["c@x.com".into()],
            &["b1@x.com".into(), "b2@x.com".into()],
            Some("high"),
            true,
        );
        assert_eq!(m2["ccRecipients"][0]["emailAddress"]["address"], "c@x.com");
        assert_eq!(m2["bccRecipients"].as_array().unwrap().len(), 2);
        assert_eq!(
            m2["bccRecipients"][1]["emailAddress"]["address"],
            "b2@x.com"
        );
        assert_eq!(m2["importance"], "high");
        assert_eq!(m2["isReadReceiptRequested"], true);
    }

    /// Records every op so the trait wiring + id passing is verifiable with no network.
    #[derive(Default)]
    struct FakeWriter {
        calls: RefCell<Vec<String>>,
    }
    impl FakeWriter {
        fn log(&self, s: String) {
            self.calls.borrow_mut().push(s);
        }
    }
    impl MailWriter for FakeWriter {
        #[allow(clippy::too_many_arguments)]
        fn send_new(
            &self,
            subject: &str,
            _body: &str,
            to: &[String],
            _cc: &[String],
            _bcc: &[String],
            importance: Option<&str>,
            request_read_receipt: bool,
        ) -> Result<(), String> {
            self.log(format!(
                "send_new subject={subject} to={} imp={} rr={request_read_receipt}",
                to.join(","),
                importance.unwrap_or("-"),
            ));
            Ok(())
        }
        fn reply(&self, id: &str, comment: &str, all: bool) -> Result<(), String> {
            self.log(format!("reply id={id} all={all} comment={comment}"));
            Ok(())
        }
        fn forward(&self, id: &str, _comment: &str, to: &[String]) -> Result<(), String> {
            self.log(format!("forward id={id} to={}", to.join(",")));
            Ok(())
        }
        fn move_to(&self, id: &str, dest: &str) -> Result<String, String> {
            self.log(format!("move_to id={id} dest={dest}"));
            Ok(format!("{id}-in-{dest}"))
        }
        fn set_read(&self, id: &str, is_read: bool) -> Result<(), String> {
            self.log(format!("set_read id={id} is_read={is_read}"));
            Ok(())
        }
        fn set_flag(&self, id: &str, status: &str) -> Result<(), String> {
            self.log(format!("set_flag id={id} status={status}"));
            Ok(())
        }
        fn set_categories(&self, id: &str, cats: &[String]) -> Result<(), String> {
            self.log(format!("set_categories id={id} cats={}", cats.join(",")));
            Ok(())
        }
        fn create_draft(
            &self,
            subject: &str,
            _body: &str,
            to: &[String],
        ) -> Result<String, String> {
            self.log(format!(
                "create_draft subject={subject} to={}",
                to.join(",")
            ));
            Ok("draft-1".into())
        }
        fn send_draft(&self, id: &str) -> Result<(), String> {
            self.log(format!("send_draft id={id}"));
            Ok(())
        }
    }

    #[test]
    fn trait_is_object_safe_and_ops_carry_ids() {
        let f = FakeWriter::default();
        let w: &dyn MailWriter = &f; // object-safety check
        w.send_new(
            "Hi",
            "<p>x</p>",
            &["a@b.com".into()],
            &[],
            &[],
            Some("high"),
            true,
        )
        .unwrap();
        w.reply("m1", "thanks", false).unwrap();
        w.reply("m1", "all thanks", true).unwrap();
        w.forward("m2", "fyi", &["x@y.com".into()]).unwrap();
        assert_eq!(w.move_to("m3", "Archive").unwrap(), "m3-in-Archive");
        w.set_read("m4", true).unwrap();
        w.set_flag("m5", "flagged").unwrap();
        w.set_categories("m6", &["Red".into()]).unwrap();
        assert_eq!(
            w.create_draft("Draft", "<p>d</p>", &["a@b.com".into()])
                .unwrap(),
            "draft-1"
        );
        w.send_draft("m7").unwrap();

        let calls = f.calls.borrow();
        assert_eq!(calls[0], "send_new subject=Hi to=a@b.com imp=high rr=true");
        assert_eq!(calls[1], "reply id=m1 all=false comment=thanks");
        assert_eq!(calls[2], "reply id=m1 all=true comment=all thanks");
        assert_eq!(calls[3], "forward id=m2 to=x@y.com");
        assert_eq!(calls[4], "move_to id=m3 dest=Archive");
        assert_eq!(calls[5], "set_read id=m4 is_read=true");
        assert_eq!(calls[6], "set_flag id=m5 status=flagged");
        assert_eq!(calls[7], "set_categories id=m6 cats=Red");
        assert_eq!(calls[8], "create_draft subject=Draft to=a@b.com");
        assert_eq!(calls[9], "send_draft id=m7");
    }
}
