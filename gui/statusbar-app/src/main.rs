//! On-screen status-bar window (plan §24/§25): presents the headless
//! `isyncyou-statusbar` renderer in a real window via winit + softbuffer, and
//! turns clicks into `hit_test` -> `apply` actions. The renderer is the *same*
//! code verified headlessly, so on-screen pixels equal the verified pixels.
//!
//! Headless smoke-run: set `ISYNCYOU_STATUSBAR_EXIT_MS=<n>` to auto-exit after
//! `n` ms (used to screenshot the window under Xvfb without it hanging).

use isyncyou_statusbar::{apply, hit_test, render, StatusView, SyncState, Transfer, HEIGHT, WIDTH};
use std::num::NonZeroU32;
use std::rc::Rc;
use std::time::{Duration, Instant};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowId};

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

impl ApplicationHandler for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.window.is_some() {
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

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => el.exit(),
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
                    // the same renderer used for headless verification
                    let pm = render(&self.view);
                    let mut buf = surface.buffer_mut().expect("surface buffer");
                    for (px, rgba) in buf.iter_mut().zip(pm.data().chunks_exact(4)) {
                        // softbuffer wants 0RGB; tiny-skia gives RGBA
                        *px = (rgba[0] as u32) << 16 | (rgba[1] as u32) << 8 | rgba[2] as u32;
                    }
                    buf.present().expect("present");
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        // auto-exit for the headless screenshot run
        if let Some(d) = self.deadline {
            if Instant::now() >= d {
                el.exit();
            } else {
                el.set_control_flow(ControlFlow::WaitUntil(d));
            }
        }
    }
}

fn main() {
    let deadline = std::env::var("ISYNCYOU_STATUSBAR_EXIT_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(|ms| Instant::now() + Duration::from_millis(ms));
    let event_loop = EventLoop::new().expect("event loop");
    event_loop.set_control_flow(ControlFlow::Wait);
    let mut app = App {
        view: sample_view(),
        window: None,
        surface: None,
        cursor: (0.0, 0.0),
        deadline,
    };
    event_loop.run_app(&mut app).expect("run app");
}
