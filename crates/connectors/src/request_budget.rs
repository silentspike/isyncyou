//! Shared conservative origin admission for the self-hosted protocols.
use std::collections::HashMap;
use std::io::{self, Read};
use std::ops::Deref;
use std::sync::{Arc, Condvar, Mutex, OnceLock, Weak};
use reqwest::blocking::{RequestBuilder, Response};
use url::Url;

struct Counts { requests: usize, attachments: usize, request_limit: usize, attachment_limit: usize, streams: usize }
impl Default for Counts { fn default() -> Self { Self { requests:0, attachments:0, request_limit:4, attachment_limit:2, streams:0 } } }
#[derive(Default)]
pub(crate) struct Gate { counts: Mutex<Counts>, changed: Condvar }
pub(crate) struct Permit { gate: Arc<Gate>, attachment: bool, stream: bool, counts_request: bool }
impl Gate {
    pub(crate) fn acquire(self: &Arc<Self>, attachment: bool) -> Permit {
        let mut c=self.counts.lock().unwrap_or_else(|e|e.into_inner());
        while c.requests>=c.request_limit || (attachment && c.attachments>=c.attachment_limit) { c=self.changed.wait(c).unwrap_or_else(|e|e.into_inner()); }
        c.requests+=1; if attachment { c.attachments+=1; }
        Permit { gate:self.clone(), attachment, stream:false, counts_request:true }
    }
    // Keep one request slot available for synchronous delta callbacks.
    pub(crate) fn acquire_stream(self: &Arc<Self>) -> Option<Permit> {
        let mut c=self.counts.lock().unwrap_or_else(|e|e.into_inner());
        loop {
            if c.request_limit < 2 { return None; }
            if c.requests < c.request_limit - 1 { break; }
            c=self.changed.wait(c).unwrap_or_else(|e|e.into_inner());
        }
        c.requests+=1; c.streams+=1;
        Some(Permit { gate:self.clone(), attachment:false, stream:true, counts_request:true })
    }
    // This independent identity slot prevents duplicate streams for one account.
    pub(crate) fn acquire_stream_identity(self: &Arc<Self>) -> Permit {
        let mut c=self.counts.lock().unwrap_or_else(|e|e.into_inner());
        while c.streams>=1 { c=self.changed.wait(c).unwrap_or_else(|e|e.into_inner()); }
        c.streams+=1; Permit { gate:self.clone(), attachment:false, stream:true, counts_request:false }
    }
}
impl Drop for Permit { fn drop(&mut self) { let mut c=self.gate.counts.lock().unwrap_or_else(|e|e.into_inner()); if self.stream {c.streams-=1;} if self.counts_request {c.requests-=1;} if self.attachment {c.attachments-=1;} self.gate.changed.notify_all(); } }
pub(crate) fn account_gate(key: &str, request_limit: usize, attachment_limit: usize) -> Arc<Gate> {
    static GATES: OnceLock<Mutex<HashMap<String,Weak<Gate>>>>=OnceLock::new();
    let mut gates=GATES.get_or_init(||Mutex::new(HashMap::new())).lock().unwrap_or_else(|e|e.into_inner());
    gates.retain(|_,v|v.strong_count()>0);
    let gate=gates.get(key).and_then(Weak::upgrade).unwrap_or_else(||{let g=Arc::new(Gate::default());gates.insert(key.to_owned(),Arc::downgrade(&g));g});
    {let mut c=gate.counts.lock().unwrap_or_else(|e|e.into_inner());c.request_limit=c.request_limit.min(request_limit.max(1));c.attachment_limit=c.attachment_limit.min(attachment_limit.max(1));gate.changed.notify_all();}
    gate
}
pub(crate) fn origin_gate(origin: &Url) -> Arc<Gate> { account_gate(&origin.origin().ascii_serialization(),4,2) }
pub struct BudgetResponse { inner: Response, _permit: Permit }
impl Deref for BudgetResponse { type Target=Response; fn deref(&self)->&Response {&self.inner} }
impl Read for BudgetResponse { fn read(&mut self, b:&mut [u8])->io::Result<usize>{self.inner.read(b)} }
#[derive(Debug)]
pub enum BudgetSendError { Uncloneable, Transport }
pub trait BudgetedSend {
    fn budgeted_send(self, origin:&Url)->Result<BudgetResponse,reqwest::Error>;
    fn budgeted_send_auto(self)->Result<BudgetResponse,BudgetSendError>;
}
impl BudgetedSend for RequestBuilder {
    fn budgeted_send(self, origin:&Url)->Result<BudgetResponse,reqwest::Error> { let permit=origin_gate(origin).acquire(false);let inner=self.send()?;Ok(BudgetResponse{inner,_permit:permit}) }
    fn budgeted_send_auto(self)->Result<BudgetResponse,BudgetSendError> {
        let request=self.try_clone().ok_or(BudgetSendError::Uncloneable)?.build().map_err(|_|BudgetSendError::Uncloneable)?;
        let origin=request.url().clone();
        self.budgeted_send(&origin).map_err(|_|BudgetSendError::Transport)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn mixed_requests_share_origin_capacity_and_hold_it_through_response_drop() {
        use std::io::{Read,Write};
        let listener=std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let origin=Url::parse(&format!("http://{}/",listener.local_addr().unwrap())).unwrap();
        let gate=origin_gate(&origin);
        let p1=gate.acquire(false); let p2=gate.acquire(true); let p3=gate.acquire(false);
        let server=std::thread::spawn(move|| {let (mut stream,_)=listener.accept().unwrap();let mut buf=[0;2048];let _=stream.read(&mut buf).unwrap();stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nConnection: close\r\n\r\nx").unwrap();});
        let http=reqwest::blocking::Client::builder().no_proxy().build().unwrap();
        let response=http.get(origin.clone()).budgeted_send(&origin).unwrap();server.join().unwrap();
        let (tx,rx)=mpsc::channel();let other=origin.clone();
        let worker=std::thread::spawn(move||{let g=origin_gate(&other);let permit=g.acquire(false);tx.send(()).unwrap();drop(permit)});
        assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());
        drop(response);rx.recv_timeout(Duration::from_secs(3)).unwrap();worker.join().unwrap();drop((p1,p2,p3));
    }
    #[test]
    fn attachment_limit_and_stream_callback_do_not_deadlock() {
        let gate=account_gate("test-budget-attachment",4,2);let a=gate.acquire(true);let b=gate.acquire(true);
        let (tx,rx)=mpsc::channel();let g=gate.clone();let worker=std::thread::spawn(move||{let permit=g.acquire(true);tx.send(()).unwrap();drop(permit)});
        assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());drop(a);rx.recv_timeout(Duration::from_secs(3)).unwrap();worker.join().unwrap();drop(b);
        let single=account_gate("test-budget-single",1,1);assert!(single.acquire_stream().is_none());
        let two=account_gate("test-budget-two",2,1);let stream=two.acquire_stream().unwrap();let request=two.acquire(false);drop(request);drop(stream);
    }
    #[test]
    fn different_origins_are_isolated_and_server_limits_only_tighten() {
        let a=account_gate("test-budget-a",1,1);let b=account_gate("test-budget-b",4,2);
        assert!(!Arc::ptr_eq(&a,&b));let permit=a.acquire(false);let (tx,rx)=mpsc::channel();
        let g=account_gate("test-budget-a",4,2);let worker=std::thread::spawn(move||{let p=g.acquire(false);tx.send(()).unwrap();drop(p)});
        assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());drop(permit);rx.recv_timeout(Duration::from_secs(3)).unwrap();worker.join().unwrap();
    }
}
