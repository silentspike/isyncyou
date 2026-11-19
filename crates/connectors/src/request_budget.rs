//! Shared conservative origin admission for the self-hosted protocols.
use reqwest::blocking::{RequestBuilder, Response};
use std::collections::HashMap;
use std::io::{self, Read};
use std::ops::Deref;
use std::sync::{Arc, Condvar, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};
use url::Url;

struct Counts {
    requests: usize,
    attachments: usize,
    request_limit: usize,
    attachment_limit: usize,
    streams: usize,
}
impl Default for Counts {
    fn default() -> Self {
        Self {
            requests: 0,
            attachments: 0,
            request_limit: 4,
            attachment_limit: 2,
            streams: 0,
        }
    }
}
#[derive(Default)]
pub(crate) struct Gate {
    counts: Mutex<Counts>,
    changed: Condvar,
}
pub(crate) struct Permit {
    gate: Arc<Gate>,
    attachment: bool,
    stream: bool,
    counts_request: bool,
}
impl Gate {
    pub(crate) fn acquire_search(
        self: &Arc<Self>,
        attachment: bool,
    ) -> Result<Permit, BudgetSendError> {
        let Some(budget) = current_search_budget() else {
            return Ok(self.acquire(attachment));
        };
        let mut c = self.counts.lock().unwrap_or_else(|e| e.into_inner());
        while c.requests >= c.request_limit || (attachment && c.attachments >= c.attachment_limit) {
            let remaining = budget.remaining()?;
            let (next, _) = self
                .changed
                .wait_timeout(c, remaining.min(Duration::from_millis(50)))
                .unwrap_or_else(|e| e.into_inner());
            c = next;
        }
        budget.remaining()?;
        c.requests += 1;
        if attachment {
            c.attachments += 1;
        }
        Ok(Permit {
            gate: self.clone(),
            attachment,
            stream: false,
            counts_request: true,
        })
    }
    pub(crate) fn acquire(self: &Arc<Self>, attachment: bool) -> Permit {
        let mut c = self.counts.lock().unwrap_or_else(|e| e.into_inner());
        while c.requests >= c.request_limit || (attachment && c.attachments >= c.attachment_limit) {
            c = self.changed.wait(c).unwrap_or_else(|e| e.into_inner());
        }
        c.requests += 1;
        if attachment {
            c.attachments += 1;
        }
        Permit {
            gate: self.clone(),
            attachment,
            stream: false,
            counts_request: true,
        }
    }
    // Keep one request slot available for synchronous delta callbacks.
    pub(crate) fn acquire_stream(self: &Arc<Self>) -> Option<Permit> {
        let mut c = self.counts.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if c.request_limit < 2 {
                return None;
            }
            if c.requests < c.request_limit - 1 {
                break;
            }
            c = self.changed.wait(c).unwrap_or_else(|e| e.into_inner());
        }
        c.requests += 1;
        c.streams += 1;
        Some(Permit {
            gate: self.clone(),
            attachment: false,
            stream: true,
            counts_request: true,
        })
    }
    // This independent identity slot prevents duplicate streams for one account.
    pub(crate) fn acquire_stream_identity(self: &Arc<Self>) -> Permit {
        let mut c = self.counts.lock().unwrap_or_else(|e| e.into_inner());
        while c.streams >= 1 {
            c = self.changed.wait(c).unwrap_or_else(|e| e.into_inner());
        }
        c.streams += 1;
        Permit {
            gate: self.clone(),
            attachment: false,
            stream: true,
            counts_request: false,
        }
    }
}
impl Drop for Permit {
    fn drop(&mut self) {
        let mut c = self.gate.counts.lock().unwrap_or_else(|e| e.into_inner());
        if self.stream {
            c.streams -= 1;
        }
        if self.counts_request {
            c.requests -= 1;
        }
        if self.attachment {
            c.attachments -= 1;
        }
        self.gate.changed.notify_all();
    }
}
pub(crate) fn account_gate(key: &str, request_limit: usize, attachment_limit: usize) -> Arc<Gate> {
    static GATES: OnceLock<Mutex<HashMap<String, Weak<Gate>>>> = OnceLock::new();
    let mut gates = GATES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    gates.retain(|_, v| v.strong_count() > 0);
    let gate = gates.get(key).and_then(Weak::upgrade).unwrap_or_else(|| {
        let g = Arc::new(Gate::default());
        gates.insert(key.to_owned(), Arc::downgrade(&g));
        g
    });
    {
        let mut c = gate.counts.lock().unwrap_or_else(|e| e.into_inner());
        c.request_limit = c.request_limit.min(request_limit.max(1));
        c.attachment_limit = c.attachment_limit.min(attachment_limit.max(1));
        gate.changed.notify_all();
    }
    gate
}
pub(crate) fn origin_gate(origin: &Url) -> Arc<Gate> {
    account_gate(&origin.origin().ascii_serialization(), 4, 2)
}
pub struct BudgetResponse {
    inner: Response,
    _permit: Permit,
}
impl Deref for BudgetResponse {
    type Target = Response;
    fn deref(&self) -> &Response {
        &self.inner
    }
}
impl Read for BudgetResponse {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        search_read(&mut self.inner, b)
    }
}
#[derive(Debug)]
pub enum BudgetSendError {
    Uncloneable,
    Transport(reqwest::Error),
    Exhausted,
    Cancelled,
}
impl BudgetSendError {
    pub fn is_timeout(&self) -> bool {
        match self {
            Self::Exhausted => true,
            Self::Transport(e) => e.is_timeout(),
            _ => false,
        }
    }
    pub fn is_connect(&self) -> bool {
        matches!(self, Self::Transport(e) if e.is_connect())
    }
}

/// Shared, turn-scoped network budget. Values and credentials are never logged.
#[derive(Clone)]
pub struct SearchBudget {
    inner: Arc<Mutex<SearchBudgetState>>,
    deadline: Instant,
    cancelled: Arc<dyn Fn() -> bool + Send + Sync>,
}
struct SearchBudgetState {
    requests: usize,
    bytes: usize,
    body_reads: usize,
    exhausted: bool,
}
impl SearchBudget {
    pub fn new(
        deadline: Instant,
        cancelled: Arc<dyn Fn() -> bool + Send + Sync>,
        max_requests: usize,
        max_bytes: usize,
        max_body_reads: usize,
    ) -> Self {
        Self {
            inner: Arc::new(Mutex::new(SearchBudgetState {
                requests: max_requests.min(128),
                bytes: max_bytes.min(8 * 1024 * 1024),
                body_reads: max_body_reads.min(40),
                exhausted: false,
            })),
            deadline: deadline.min(Instant::now() + Duration::from_secs(10)),
            cancelled,
        }
    }
    pub fn remaining(&self) -> Result<Duration, BudgetSendError> {
        if (self.cancelled)() {
            return Err(BudgetSendError::Cancelled);
        }
        self.deadline
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
            .ok_or(BudgetSendError::Exhausted)
    }
    fn request(&self) -> Result<(), BudgetSendError> {
        self.remaining()?;
        let mut s = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if s.requests == 0 {
            s.exhausted = true;
            return Err(BudgetSendError::Exhausted);
        }
        s.requests -= 1;
        Ok(())
    }
    fn reserve_bytes(&self, count: usize) -> Result<(), BudgetSendError> {
        self.remaining()?;
        let mut s = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if count > s.bytes {
            s.exhausted = true;
            return Err(BudgetSendError::Exhausted);
        }
        s.bytes -= count;
        Ok(())
    }
    /// Reserve a bounded allowance for a local read; unused bytes can be
    /// returned once, only through the associated reservation.
    pub fn reserve_output(&self, count: usize) -> Result<SearchOutputReservation, BudgetSendError> {
        self.reserve_bytes(count)?;
        Ok(SearchOutputReservation {
            budget: self.clone(),
            reserved: count,
        })
    }
    pub fn body_read(&self) -> Result<(), BudgetSendError> {
        self.remaining()?;
        let mut s = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if s.body_reads == 0 {
            s.exhausted = true;
            return Err(BudgetSendError::Exhausted);
        }
        s.body_reads -= 1;
        Ok(())
    }
    pub fn remaining_limits(&self) -> (usize, usize, usize) {
        let s = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        (s.requests, s.bytes, s.body_reads)
    }
    pub fn was_exhausted(&self) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .exhausted
    }
}
pub struct SearchOutputReservation {
    budget: SearchBudget,
    reserved: usize,
}
impl SearchOutputReservation {
    pub fn finish(self, used: usize) -> Result<(), BudgetSendError> {
        if used > self.reserved {
            return Err(BudgetSendError::Exhausted);
        }
        let mut state = self.budget.inner.lock().unwrap_or_else(|e| e.into_inner());
        state.bytes += self.reserved - used;
        Ok(())
    }
}
thread_local! { static SEARCH_BUDGET: std::cell::RefCell<Option<SearchBudget>> = const { std::cell::RefCell::new(None) }; }
pub fn with_search_budget<T>(budget: &SearchBudget, f: impl FnOnce() -> T) -> T {
    struct Reset(Option<SearchBudget>);
    impl Drop for Reset {
        fn drop(&mut self) {
            SEARCH_BUDGET.with(|b| *b.borrow_mut() = self.0.take());
        }
    }
    let old = SEARCH_BUDGET.with(|b| b.borrow_mut().replace(budget.clone()));
    let _reset = Reset(old);
    f()
}
fn current_search_budget() -> Option<SearchBudget> {
    SEARCH_BUDGET.with(|b| b.borrow().clone())
}
pub(crate) fn search_request(request: RequestBuilder) -> Result<RequestBuilder, BudgetSendError> {
    if let Some(budget) = current_search_budget() {
        let prepared = request
            .try_clone()
            .ok_or(BudgetSendError::Uncloneable)?
            .build()
            .map_err(BudgetSendError::Transport)?;
        let body = match prepared.body() {
            Some(body) => body.as_bytes().ok_or(BudgetSendError::Uncloneable)?.len(),
            None => 0,
        };
        budget.reserve_bytes(body)?;
        budget.request()?;
        Ok(request.timeout(budget.remaining()?))
    } else {
        Ok(request)
    }
}
pub(crate) fn search_read(reader: &mut impl Read, out: &mut [u8]) -> io::Result<usize> {
    if let Some(budget) = current_search_budget() {
        budget
            .remaining()
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "search budget exhausted"))?;
        let len = {
            let mut state = budget.inner.lock().unwrap_or_else(|e| e.into_inner());
            if state.bytes == 0 && !out.is_empty() {
                state.exhausted = true;
                return Err(io::Error::other("search byte budget exhausted"));
            }
            let len = out.len().min(state.bytes);
            state.bytes -= len;
            len
        };
        let n = reader.read(&mut out[..len])?;
        let mut state = budget.inner.lock().unwrap_or_else(|e| e.into_inner());
        state.bytes += len - n;
        drop(state);
        budget
            .remaining()
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "search budget exhausted"))?;
        Ok(n)
    } else {
        reader.read(out)
    }
}
pub trait BudgetedSend {
    fn budgeted_send(self, origin: &Url) -> Result<BudgetResponse, BudgetSendError>;
    fn budgeted_send_auto(self) -> Result<BudgetResponse, BudgetSendError>;
}
impl BudgetedSend for RequestBuilder {
    fn budgeted_send(self, origin: &Url) -> Result<BudgetResponse, BudgetSendError> {
        let permit = origin_gate(origin).acquire_search(false)?;
        let inner = search_request(self)?
            .send()
            .map_err(BudgetSendError::Transport)?;
        Ok(BudgetResponse {
            inner,
            _permit: permit,
        })
    }
    fn budgeted_send_auto(self) -> Result<BudgetResponse, BudgetSendError> {
        let request = self
            .try_clone()
            .ok_or(BudgetSendError::Uncloneable)?
            .build()
            .map_err(|_| BudgetSendError::Uncloneable)?;
        let origin = request.url().clone();
        self.budgeted_send(&origin)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_budget_bounds_real_request_deadline_and_counters() {
        use std::io::Read;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0; 1024];
            let _ = stream.read(&mut buf);
            std::thread::sleep(Duration::from_millis(350));
        });
        let budget = SearchBudget::new(
            Instant::now() + Duration::from_millis(100),
            Arc::new(|| false),
            2,
            1000,
            1,
        );
        let http = reqwest::blocking::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let start = Instant::now();
        assert!(
            with_search_budget(&budget, || http.get(origin.clone()).budgeted_send(&origin))
                .is_err()
        );
        assert!(start.elapsed() < Duration::from_millis(300));
        assert_eq!(budget.remaining_limits(), (1, 1000, 1));
        server.join().unwrap();
    }

    #[test]
    fn search_budget_counts_chunks_and_honors_cancel_before_next_read() {
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let check = cancelled.clone();
        let budget = SearchBudget::new(
            Instant::now() + Duration::from_secs(2),
            Arc::new(move || check.load(std::sync::atomic::Ordering::SeqCst)),
            2,
            4,
            1,
        );
        let mut source = &b"abcdef"[..];
        let mut out = [0; 8];
        with_search_budget(&budget, || {
            assert_eq!(search_read(&mut source, &mut out).unwrap(), 4);
            assert_eq!(budget.remaining_limits(), (2, 0, 1));
            cancelled.store(true, std::sync::atomic::Ordering::SeqCst);
            assert!(search_read(&mut source, &mut out).is_err());
        });
    }

    #[test]
    fn search_budget_times_out_waiting_for_shared_origin_gate() {
        let origin = Url::parse("http://127.0.0.1:39219/").unwrap();
        let gate = origin_gate(&origin);
        let permits: Vec<_> = (0..4).map(|_| gate.acquire(false)).collect();
        let budget = SearchBudget::new(
            Instant::now() + Duration::from_millis(80),
            Arc::new(|| false),
            1,
            100,
            1,
        );
        let start = Instant::now();
        assert!(with_search_budget(&budget, || gate.acquire_search(false)).is_err());
        assert!(start.elapsed() < Duration::from_millis(300));
        assert_eq!(budget.remaining_limits(), (1, 100, 1));
        drop(permits);
    }

    #[test]
    fn search_budget_caps_cannot_be_increased_by_reservation_return() {
        let budget = SearchBudget::new(
            Instant::now() + Duration::from_secs(100),
            Arc::new(|| false),
            usize::MAX,
            usize::MAX,
            usize::MAX,
        );
        assert_eq!(budget.remaining_limits(), (128, 8 * 1024 * 1024, 40));
        let reservation = budget.reserve_output(100).unwrap();
        assert_eq!(budget.remaining_limits().1, 8 * 1024 * 1024 - 100);
        reservation.finish(60).unwrap();
        assert_eq!(budget.remaining_limits().1, 8 * 1024 * 1024 - 60);
        let reservation = budget.reserve_output(50).unwrap();
        assert!(reservation.finish(51).is_err());
        assert_eq!(budget.remaining_limits().1, 8 * 1024 * 1024 - 110);
    }

    #[test]
    fn nested_search_budget_restores_outer_context_after_panic() {
        let outer = SearchBudget::new(
            Instant::now() + Duration::from_secs(2),
            Arc::new(|| false),
            2,
            1000,
            1,
        );
        let inner = SearchBudget::new(
            Instant::now() + Duration::from_secs(2),
            Arc::new(|| false),
            1,
            1000,
            1,
        );
        let client = reqwest::blocking::Client::new();
        with_search_budget(&outer, || {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                with_search_budget(&inner, || {
                    let _ = search_request(client.get("https://example.test/")).unwrap();
                    panic!("fixture panic");
                });
            }));
            let _ = search_request(client.get("https://example.test/")).unwrap();
        });
        assert_eq!(inner.remaining_limits().0, 0);
        assert_eq!(outer.remaining_limits().0, 1);
        // No search context remains on this test thread.
        let _ = search_request(client.get("https://example.test/")).unwrap();
        assert_eq!(outer.remaining_limits().0, 1);
    }
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn mixed_requests_share_origin_capacity_and_hold_it_through_response_drop() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let gate = origin_gate(&origin);
        let p1 = gate.acquire(false);
        let p2 = gate.acquire(true);
        let p3 = gate.acquire(false);
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0; 2048];
            let _ = stream.read(&mut buf).unwrap();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nConnection: close\r\n\r\nx")
                .unwrap();
        });
        let http = reqwest::blocking::Client::builder()
            .no_proxy()
            .build()
            .unwrap();
        let response = http.get(origin.clone()).budgeted_send(&origin).unwrap();
        server.join().unwrap();
        let (tx, rx) = mpsc::channel();
        let other = origin.clone();
        let worker = std::thread::spawn(move || {
            let g = origin_gate(&other);
            let permit = g.acquire(false);
            tx.send(()).unwrap();
            drop(permit)
        });
        assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());
        drop(response);
        rx.recv_timeout(Duration::from_secs(3)).unwrap();
        worker.join().unwrap();
        drop((p1, p2, p3));
    }
    #[test]
    fn attachment_limit_and_stream_callback_do_not_deadlock() {
        let gate = account_gate("test-budget-attachment", 4, 2);
        let a = gate.acquire(true);
        let b = gate.acquire(true);
        let (tx, rx) = mpsc::channel();
        let g = gate.clone();
        let worker = std::thread::spawn(move || {
            let permit = g.acquire(true);
            tx.send(()).unwrap();
            drop(permit)
        });
        assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());
        drop(a);
        rx.recv_timeout(Duration::from_secs(3)).unwrap();
        worker.join().unwrap();
        drop(b);
        let single = account_gate("test-budget-single", 1, 1);
        assert!(single.acquire_stream().is_none());
        let two = account_gate("test-budget-two", 2, 1);
        let stream = two.acquire_stream().unwrap();
        let request = two.acquire(false);
        drop(request);
        drop(stream);
    }
    #[test]
    fn different_origins_are_isolated_and_server_limits_only_tighten() {
        let a = account_gate("test-budget-a", 1, 1);
        let b = account_gate("test-budget-b", 4, 2);
        assert!(!Arc::ptr_eq(&a, &b));
        let permit = a.acquire(false);
        let (tx, rx) = mpsc::channel();
        let g = account_gate("test-budget-a", 4, 2);
        let worker = std::thread::spawn(move || {
            let p = g.acquire(false);
            tx.send(()).unwrap();
            drop(p)
        });
        assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());
        drop(permit);
        rx.recv_timeout(Duration::from_secs(3)).unwrap();
        worker.join().unwrap();
    }
}
