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
        account: "backupslave@outlook.com".into(),
        state: SyncState::Syncing,
        transfers: vec![
            Transfer {
                name: "IMG_2024.jpg".into(),
                up: false,
                percent: 71,
            },
            Transfer {
                name: "Beleg.pdf".into(),
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
            state_label: "Synchronisiert".into(),
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

fn main() {
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
