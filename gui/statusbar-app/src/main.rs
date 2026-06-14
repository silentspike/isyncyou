//! SNI system-tray app + on-screen status flyout (plan §13/§24/§25).
//!
//! iSyncYou lives in the system tray as a StatusNotifierItem (via `ksni`,
//! pure-DBus) — tray-first: no window opens on startup. Left-click the icon unfolds
//! a **frameless status panel right at the icon** (Nextcloud/Dropbox-style; the
//! Plasma host gives the icon coordinates via `Activate`), which dismisses itself on
//! focus loss. The panel presents the headless `isyncyou-statusbar` renderer (so the
//! on-screen pixels equal the verified pixels) with the live daemon status and a
//! button into the full web UI; the right-click menu mirrors those actions and a
//! live status label. The tray runs on its own thread, refreshes the label from the
//! daemon API, and reaches the winit loop through an [`EventLoopProxy`].
//!
//! Window identity (WM_CLASS / Wayland app_id = [`APP_ID`]) matches the installed
//! `.desktop` so the panel/task switcher show "iSyncYou".
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
    /// Show the status panel as a frameless flyout near the tray icon. `x`/`y` are
    /// the icon's screen coordinates from the SNI `Activate` (0,0 = auto-place).
    ShowPopup {
        x: i32,
        y: i32,
    },
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

/// The app's stable identity: the X11 WM_CLASS / Wayland app_id so the panel,
/// task switcher and the `.desktop` (StartupWMClass) all show "iSyncYou" with the
/// right icon instead of the executable/crate name.
const APP_ID: &str = "org.silentspike.iSyncYou";

struct App {
    view: StatusView,
    window: Option<Rc<Window>>,
    surface: Option<softbuffer::Surface<Rc<Window>, Rc<Window>>>,
    cursor: (f64, f64),
    deadline: Option<Instant>,
    /// Daemon API/web-UI address for the window's "Open in browser" button.
    api: String,
    /// Whether the flyout has gained focus since it opened. Closing on focus-loss is
    /// gated on this so the panel doesn't vanish instantly when it opens unfocused
    /// (e.g. under focus-stealing prevention).
    popup_was_focused: bool,
}

impl App {
    /// Place the flyout near the tray icon at screen `(x, y)` (from SNI Activate),
    /// flipped above/below and clamped so it stays fully on the primary monitor.
    /// `(0, 0)` (menu entry, no coordinates) auto-places it top-right under the panel.
    fn popup_position(&self, el: &ActiveEventLoop, x: i32, y: i32) -> (i32, i32) {
        let (sw, sh) = el
            .primary_monitor()
            .map(|m| (m.size().width as i32, m.size().height as i32))
            .unwrap_or((1920, 1080));
        let (w, h) = (WIDTH as i32, HEIGHT as i32);
        if x == 0 && y == 0 {
            return (sw - w - 8, 40);
        }
        let px = (x - w / 2).clamp(8, (sw - w - 8).max(8));
        // click in the lower half (bottom panel) → panel opens upward, else downward
        let py = if y > sh / 2 {
            (y - h - 8).max(8)
        } else {
            (y + 8).min((sh - h - 8).max(8))
        };
        (px, py)
    }

    /// Create the frameless status flyout near the tray icon, or focus the existing
    /// one. Borderless + positioned at the icon so it reads as the app "unfolding"
    /// from the tray (it closes again on focus loss; see [`Self`]'s window_event).
    fn ensure_window(&mut self, el: &ActiveEventLoop, x: i32, y: i32) {
        if let Some(w) = &self.window {
            w.focus_window();
            return;
        }
        let (px, py) = self.popup_position(el, x, y);
        #[allow(unused_mut)]
        let mut attrs = Window::default_attributes()
            .with_title("iSyncYou")
            .with_decorations(false)
            .with_resizable(false)
            .with_position(winit::dpi::PhysicalPosition::new(px, py))
            .with_inner_size(winit::dpi::LogicalSize::new(WIDTH, HEIGHT));
        // Set the WM_CLASS (X11) / app_id (Wayland) so the window is identified as
        // "iSyncYou" and matched to its .desktop. Linux-only (the exts don't exist
        // elsewhere); applying both backends is harmless — only the active one is used.
        #[cfg(target_os = "linux")]
        {
            use winit::platform::wayland::WindowAttributesExtWayland;
            use winit::platform::x11::WindowAttributesExtX11;
            attrs = WindowAttributesExtX11::with_name(attrs, APP_ID, "iSyncYou");
            attrs = WindowAttributesExtWayland::with_name(attrs, APP_ID, "iSyncYou");
        }
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
        self.popup_was_focused = false;
        window.focus_window();
        window.request_redraw();
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, _el: &ActiveEventLoop) {
        // Tray-first: do NOT open a window on startup. iSyncYou lives in the system
        // tray (StatusNotifierItem); the optional status window is created on demand
        // (tray menu "Status window…"). Left-click opens the web UI in the browser.
    }

    fn user_event(&mut self, el: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::ShowPopup { x, y } => self.ensure_window(el, x, y),
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
            // flyout behavior: dismiss the panel when it loses focus (click elsewhere),
            // but only once it has actually been focused — otherwise a panel that opens
            // unfocused would close immediately.
            WindowEvent::Focused(true) => self.popup_was_focused = true,
            WindowEvent::Focused(false) if self.popup_was_focused => {
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
                    // OpenBrowser is launched here (apply() is render-state only); other
                    // actions just update the view and trigger a redraw.
                    if matches!(action, isyncyou_statusbar::Action::OpenBrowser) {
                        open_web_ui(&self.api);
                    } else if apply(&mut self.view, action) {
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
/// Open the iSyncYou web UI (served by the daemon at `http://<api>/`) in the user's
/// browser. This is the entry point to the full feature surface — mail/calendar/
/// contacts restore, search, all services. Best-effort: a missing `xdg-open` just
/// logs, never crashes the tray.
fn open_web_ui(api: &str) {
    let url = format!("http://{api}/");
    match std::process::Command::new("xdg-open").arg(&url).spawn() {
        Ok(_) => eprintln!("tray: opening web UI {url}"),
        Err(e) => eprintln!("tray: could not open {url}: {e}"),
    }
}

struct StatusTray {
    proxy: EventLoopProxy<UserEvent>,
    state_label: String,
    /// Daemon API/web-UI address (`host:port`) for the browser link.
    api: String,
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
    fn activate(&mut self, x: i32, y: i32) {
        // Left-click: unfold the status flyout right at the tray icon (x, y are the
        // icon's screen coordinates).
        let _ = self.proxy.send_event(UserEvent::ShowPopup { x, y });
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
                label: "Show sync status".into(),
                icon_name: "folder-sync".into(),
                activate: Box::new(|t: &mut StatusTray| {
                    let _ = t.proxy.send_event(UserEvent::ShowPopup { x: 0, y: 0 });
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Open in browser (mail restore, search, …)".into(),
                icon_name: "internet-web-browser".into(),
                activate: Box::new(|t: &mut StatusTray| open_web_ui(&t.api)),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Quit".into(),
                icon_name: "application-exit".into(),
                activate: Box::new(|t: &mut StatusTray| {
                    let _ = t.proxy.send_event(UserEvent::Quit);
                }),
                ..Default::default()
            }
            .into(),
        ]
    }
}

/// Derive the tray's status label from the live daemon API: `Synced` / `Syncing
/// (N)` / `Paused`, or `Offline` if the daemon can't be reached. Blocking (std
/// TCP, short timeout) — call it off the async executor via `spawn_blocking`.
fn compute_label(api: &str) -> String {
    let state = match http_get_json(api, "/api/v1/sync/state") {
        Ok(s) => s,
        Err(_) => return "Offline".into(),
    };
    if state["paused"].as_bool().unwrap_or(false) {
        return "Paused".into();
    }
    let hyd = http_get_json(api, "/api/v1/hydrations").unwrap_or(serde_json::json!({}));
    let n = hyd["active"].as_array().map(|a| a.len()).unwrap_or(0);
    if n > 0 {
        format!("Syncing ({n})")
    } else {
        "Synced".into()
    }
}

/// Run the SNI tray on a Tokio runtime (ksni is async); keep it alive for the
/// process lifetime and refresh the live status every few seconds. Logs and returns
/// if no StatusNotifierWatcher is available.
fn run_tray(proxy: EventLoopProxy<UserEvent>, api: String) {
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
            state_label: "…".into(),
            api: api.clone(),
        };
        match tray.spawn().await {
            Ok(handle) => {
                eprintln!("tray: StatusNotifierItem registered");
                // Reflect the live daemon status in the tray label/tooltip.
                loop {
                    let api2 = api.clone();
                    let label = tokio::task::spawn_blocking(move || compute_label(&api2))
                        .await
                        .unwrap_or_else(|_| "Offline".into());
                    handle
                        .update(|t: &mut StatusTray| t.state_label = label.clone())
                        .await;
                    tokio::time::sleep(Duration::from_secs(3)).await;
                }
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
    let api = args
        .iter()
        .position(|a| a == "--api")
        .and_then(|j| args.get(j + 1))
        .map(String::as_str)
        .unwrap_or("127.0.0.1:8765")
        .to_string();
    let deadline = std::env::var("ISYNCYOU_STATUSBAR_EXIT_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(|ms| Instant::now() + Duration::from_millis(ms));
    let event_loop = EventLoop::<UserEvent>::with_user_event()
        .build()
        .expect("event loop");
    event_loop.set_control_flow(ControlFlow::Wait);
    let proxy = event_loop.create_proxy();
    let tray_api = api.clone();
    std::thread::spawn(move || run_tray(proxy, tray_api));
    // The optional status window shows live daemon data when reachable, else a
    // representative sample (it is a preview; the tray label is the live status).
    let mut app = App {
        view: live_view(&api).unwrap_or_else(|_| sample_view()),
        window: None,
        surface: None,
        cursor: (0.0, 0.0),
        deadline,
        api: api.clone(),
        popup_was_focused: false,
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
