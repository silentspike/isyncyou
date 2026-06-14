//! On-screen status-bar window + SNI system-tray icon (plan §13/§24/§25).
//!
//! The window presents the headless `isyncyou-statusbar` renderer (so on-screen
//! pixels equal the verified pixels), and a StatusNotifierItem tray icon (via
//! `ksni`, pure-DBus) lives in the panel: left-click / "Open" focuses the window,
//! "Quit" exits. The tray runs on its own thread; menu actions reach the winit
//! loop through an [`EventLoopProxy`].
//!
//! Headless smoke-run: set `ISYNCYOU_STATUSBAR_EXIT_MS=<n>` to auto-exit after
//! `n` ms (used to screenshot the window/tray under Xvfb / on a live session).

use isyncyou_statusbar::{apply, hit_test, render, StatusView, SyncState, Transfer, HEIGHT, WIDTH};
use std::num::NonZeroU32;
use std::rc::Rc;
use std::time::{Duration, Instant};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::window::{Window, WindowId};

/// Messages the tray thread sends to the winit loop.
#[derive(Debug, Clone)]
enum UserEvent {
    FocusWindow,
    Quit,
}

/// A representative view to render on screen.
fn sample_view() -> StatusView {
    StatusView {
        account: "testuser@example.com".into(),
        state: SyncState::Syncing,
        transfers: vec![
            Transfer {
                name: "IMG_2024.jpg".into(),
                up: false,
                percent: 71,
            },
            Transfer {
                name: "invoice.pdf".into(),
                up: true,
                percent: 88,
            },
        ],
        down_mbps: 12.2,
        up_mbps: 3.2,
        queue: 14,
    }
}

struct App {
    view: StatusView,
    window: Option<Rc<Window>>,
    surface: Option<softbuffer::Surface<Rc<Window>, Rc<Window>>>,
    cursor: (f64, f64),
    deadline: Option<Instant>,
}

impl App {
    /// Create the window + surface if absent, else focus the existing one.
    fn ensure_window(&mut self, el: &ActiveEventLoop) {
        if let Some(w) = &self.window {
            w.focus_window();
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("iSyncYou")
            .with_inner_size(winit::dpi::LogicalSize::new(WIDTH, HEIGHT));
        let window = Rc::new(el.create_window(attrs).expect("create window"));
        let context = softbuffer::Context::new(window.clone()).expect("softbuffer context");
        let mut surface =
            softbuffer::Surface::new(&context, window.clone()).expect("softbuffer surface");
        surface
            .resize(
                NonZeroU32::new(WIDTH).unwrap(),
                NonZeroU32::new(HEIGHT).unwrap(),
            )
            .unwrap();
        self.surface = Some(surface);
        self.window = Some(window.clone());
        window.request_redraw();
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        self.ensure_window(el);
    }

    fn user_event(&mut self, el: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::FocusWindow => self.ensure_window(el),
            UserEvent::Quit => el.exit(),
        }
    }

    fn window_event(&mut self, _el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            // closing the window only hides it — the tray icon keeps the app alive
            WindowEvent::CloseRequested => {
                self.window = None;
                self.surface = None;
            }
            WindowEvent::CursorMoved { position, .. } => self.cursor = (position.x, position.y),
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } => {
                if let Some(action) = hit_test(self.cursor.0 as f32, self.cursor.1 as f32) {
                    if apply(&mut self.view, action) {
                        if let Some(w) = &self.window {
                            w.request_redraw();
                        }
                    }
                }
            }
            WindowEvent::RedrawRequested => {
                if let Some(surface) = self.surface.as_mut() {
                    let pm = render(&self.view);
                    let mut buf = surface.buffer_mut().expect("surface buffer");
                    for (px, rgba) in buf.iter_mut().zip(pm.data().chunks_exact(4)) {
                        *px = (rgba[0] as u32) << 16 | (rgba[1] as u32) << 8 | rgba[2] as u32;
                    }
                    buf.present().expect("present");
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        if let Some(d) = self.deadline {
            if Instant::now() >= d {
                el.exit();
            } else {
                el.set_control_flow(ControlFlow::WaitUntil(d));
            }
        }
    }
}

/// The StatusNotifierItem tray icon: shows the sync status + a menu, and forwards
/// actions to the window via the [`EventLoopProxy`].
struct StatusTray {
    proxy: EventLoopProxy<UserEvent>,
    state_label: String,
}

impl ksni::Tray for StatusTray {
    fn id(&self) -> String {
        "isyncyou".into()
    }
    fn title(&self) -> String {
        "iSyncYou".into()
    }
    fn icon_name(&self) -> String {
        "folder-sync".into()
    }
    fn status(&self) -> ksni::Status {
        ksni::Status::Active
    }
    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: "iSyncYou".into(),
            description: self.state_label.clone(),
            icon_name: "folder-sync".into(),
            icon_pixmap: Vec::new(),
        }
    }
    fn activate(&mut self, _x: i32, _y: i32) {
        let _ = self.proxy.send_event(UserEvent::FocusWindow);
    }
    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::{MenuItem, StandardItem};
        vec![
            StandardItem {
                label: format!("iSyncYou — {}", self.state_label),
                enabled: false,
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Open".into(),
                activate: Box::new(|t: &mut StatusTray| {
                    let _ = t.proxy.send_event(UserEvent::FocusWindow);
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Quit".into(),
                activate: Box::new(|t: &mut StatusTray| {
                    let _ = t.proxy.send_event(UserEvent::Quit);
                }),
                ..Default::default()
            }
            .into(),
        ]
    }
}

/// Run the SNI tray on a Tokio runtime (ksni is async); keep it alive for the
/// process lifetime. Logs and returns if no StatusNotifierWatcher is available.
fn run_tray(proxy: EventLoopProxy<UserEvent>) {
    use ksni::TrayMethods;
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("tray: cannot start runtime: {e}");
            return;
        }
    };
    rt.block_on(async move {
        let tray = StatusTray {
            proxy,
            state_label: "Synced".into(),
        };
        match tray.spawn().await {
            Ok(_handle) => {
                eprintln!("tray: StatusNotifierItem registered");
                std::future::pending::<()>().await;
            }
            Err(e) => eprintln!("tray: no StatusNotifierWatcher / SNI host: {e}"),
        }
    });
}

/// Minimal HTTP/1.1 GET over loopback TCP (the local API is loopback-only and
/// plaintext, so no client library is needed). Returns the parsed JSON body.
fn http_get_json(addr: &str, path: &str) -> Result<serde_json::Value, String> {
    use std::io::{Read, Write};
    let mut sock =
        std::net::TcpStream::connect(addr).map_err(|e| format!("connect {addr}: {e}"))?;
    sock.set_read_timeout(Some(Duration::from_secs(10))).ok();
    write!(
        sock,
        "GET {path} HTTP/1.0\r\nHost: {addr}\r\nConnection: close\r\n\r\n"
    )
    .map_err(|e| e.to_string())?;
    // Read the head byte-wise up to the terminator, then exactly Content-Length
    // body bytes — the server may keep the connection open, so reading to EOF
    // would just hit the timeout.
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        match sock.read(&mut byte) {
            Ok(0) => break,
            Ok(_) => head.push(byte[0]),
            Err(e) => return Err(format!("read header: {e}")),
        }
        if head.len() > 64 * 1024 {
            return Err("oversized HTTP header".into());
        }
    }
    let head_text = String::from_utf8_lossy(&head);
    let len: usize = head_text
        .lines()
        .find_map(|l| {
            l.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .map(str::to_owned)
        })
        .and_then(|v| v.trim().parse().ok())
        .ok_or("response without Content-Length")?;
    let mut body = vec![0u8; len];
    sock.read_exact(&mut body)
        .map_err(|e| format!("read body: {e}"))?;
    serde_json::from_slice(&body).map_err(|e| format!("parse {path}: {e}"))
}

/// Build a [`StatusView`] from the **live daemon** via its local API: the first
/// account's username plus the real scheduled-sync state. Errors (daemon down,
/// no account) are surfaced — a snapshot must prove live data, not invent it.
fn live_view(api: &str) -> Result<StatusView, String> {
    let accounts = http_get_json(api, "/api/v1/accounts")?;
    let acc = accounts["accounts"]
        .as_array()
        .and_then(|a| a.first())
        .ok_or("daemon reports no accounts")?;
    let username = acc["username"].as_str().unwrap_or("?").to_string();
    let state = http_get_json(api, "/api/v1/sync/state")?;
    let paused = state["paused"].as_bool().unwrap_or(false);

    // In-flight FUSE placeholder downloads (best-effort; absent on a daemon
    // without a mount). Active hydrations show as downloads + flip state to
    // Syncing so the bar reflects on-demand fetches.
    let hyd = http_get_json(api, "/api/v1/hydrations").unwrap_or(serde_json::json!({}));
    let active: Vec<String> = hyd["active"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|n| n.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let sync_state = if paused {
        SyncState::Paused
    } else if active.is_empty() {
        SyncState::Synced
    } else {
        SyncState::Syncing
    };
    let transfers = active
        .iter()
        .take(3)
        .map(|name| Transfer {
            name: name.clone(),
            up: false,
            percent: 0,
        })
        .collect();
    Ok(StatusView {
        account: username,
        state: sync_state,
        transfers,
        down_mbps: 0.0,
        up_mbps: 0.0,
        queue: active.len() as u32,
    })
}

/// `--snapshot <out.png> [--api <host:port>]`: render the **live** daemon status
/// headlessly through the same renderer that draws the window (the verified
/// pixels ARE the screen pixels) and exit — no display server needed. Used by
/// the staging E2E to verify the native UI against real daemon data.
fn run_snapshot(out: &str, api: &str) -> Result<(), String> {
    let view = live_view(api)?;
    let png = isyncyou_statusbar::render_png(&view);
    std::fs::write(out, png).map_err(|e| format!("write {out}: {e}"))?;
    eprintln!(
        "snapshot: {out} (account {}, state {:?})",
        view.account, view.state
    );
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if let Some(i) = args.iter().position(|a| a == "--snapshot") {
        let out = args
            .get(i + 1)
            .map(String::as_str)
            .unwrap_or("statusbar.png");
        let api = args
            .iter()
            .position(|a| a == "--api")
            .and_then(|j| args.get(j + 1))
            .map(String::as_str)
            .unwrap_or("127.0.0.1:8765");
        if let Err(e) = run_snapshot(out, api) {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
        return;
    }
    let deadline = std::env::var("ISYNCYOU_STATUSBAR_EXIT_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(|ms| Instant::now() + Duration::from_millis(ms));
    let event_loop = EventLoop::<UserEvent>::with_user_event()
        .build()
        .expect("event loop");
    event_loop.set_control_flow(ControlFlow::Wait);
    let proxy = event_loop.create_proxy();
    std::thread::spawn(move || run_tray(proxy));
    let mut app = App {
        view: sample_view(),
        window: None,
        surface: None,
        cursor: (0.0, 0.0),
        deadline,
    };
    event_loop.run_app(&mut app).expect("run app");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One-shot mock API: serves canned JSON bodies for the two endpoints the
    /// snapshot mode queries, then a temp PNG is rendered from the result.
    fn serve_api(paused: bool) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        std::thread::spawn(move || {
            // live_view queries /accounts, /sync/state and /hydrations
            for _ in 0..3 {
                let (mut sock, _) = listener.accept().unwrap();
                use std::io::{Read, Write};
                let mut head = Vec::new();
                let mut b = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") && sock.read(&mut b).unwrap_or(0) > 0 {
                    head.push(b[0]);
                }
                let head = String::from_utf8_lossy(&head).to_string();
                let body = if head.contains("/api/v1/accounts") {
                    r#"{"accounts":[{"id":"a","username":"live@example.com"}]}"#.to_string()
                } else if head.contains("/api/v1/hydrations") {
                    r#"{"count":0,"active":[]}"#.to_string()
                } else {
                    format!(r#"{{"enabled":true,"paused":{paused}}}"#)
                };
                write!(
                    sock,
                    "HTTP/1.0 200 OK\r\nContent-Length: {}\r\n\r\n{}",
                    body.len(),
                    body
                )
                .unwrap();
            }
        });
        addr
    }

    #[test]
    fn live_view_reads_account_and_sync_state_from_the_daemon_api() {
        let addr = serve_api(true);
        let v = live_view(&addr).unwrap();
        assert_eq!(v.account, "live@example.com");
        assert!(matches!(v.state, SyncState::Paused));
    }

    #[test]
    fn snapshot_writes_a_png_rendered_from_live_data() {
        let addr = serve_api(false);
        let out = std::env::temp_dir().join(format!("isy-snap-{}.png", std::process::id()));
        run_snapshot(out.to_str().unwrap(), &addr).unwrap();
        let bytes = std::fs::read(&out).unwrap();
        assert!(bytes.starts_with(b"\x89PNG\r\n\x1a\n"), "not a PNG");
        assert!(bytes.len() > 10_000, "implausibly small render");
        let _ = std::fs::remove_file(&out);
    }

    #[test]
    fn live_view_surfaces_a_dead_daemon_instead_of_inventing_data() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        drop(listener);
        assert!(live_view(&addr).is_err());
    }
}
