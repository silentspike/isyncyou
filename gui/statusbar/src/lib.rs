//! `isyncyou-statusbar` — the native mini status-bar renderer.
//!
//! Own renderer (tiny-skia shapes + cosmic-text text), no GUI framework. The
//! same code renders on-screen (via winit/softbuffer, added with the tray in
//! #56) and **headless** into a pixel buffer for AI/CI verification — so the
//! verified pixels equal the on-screen pixels by construction.
//!
//! [`render`] returns a [`tiny_skia::Pixmap`]; [`render_png`] encodes it.
//!
//! Text uses a **bundled** font (JetBrains Mono, SIL OFL-1.1 — see
//! `assets/JetBrainsMono-OFL.txt`) loaded into a [`FontSystem`] with no
//! system-font scan, so glyphs render identically and deterministically on any
//! machine — including a headless CI box with no fonts installed. JetBrains Mono
//! covers every glyph the UI uses (Latin + German umlauts + arrows ↓↑→ + ⚠/…).

use cosmic_text::{
    Attrs, Buffer, Color as CtColor, Family, FontSystem, Metrics, Shaping, SwashCache,
};
use tiny_skia::{
    Color, FillRule, GradientStop, LinearGradient, Paint, PathBuilder, Pixmap, Point, Rect,
    SpreadMode, Stroke, Transform,
};

pub const WIDTH: u32 = 380;
pub const HEIGHT: u32 = 560;

/// The bundled UI font (JetBrains Mono Regular, SIL OFL-1.1). Embedded so text
/// never depends on system fonts.
const FONT_DATA: &[u8] = include_bytes!("../assets/JetBrainsMono-Regular.ttf");
/// Family name of [`FONT_DATA`].
const FONT_FAMILY: &str = "JetBrains Mono";

/// A [`FontSystem`] containing **only** the bundled font (no system scan), so
/// rendering is deterministic and works on a font-less headless host.
pub fn bundled_font_system() -> FontSystem {
    let mut db = cosmic_text::fontdb::Database::new();
    db.load_font_data(FONT_DATA.to_vec());
    FontSystem::new_with_locale_and_db("en-US".to_string(), db)
}

/// Overall sync state shown in the status pill. `Synced` is the at-rest/idle
/// state (nothing pending). `Throttled` and `Error` carry a reason that the
/// status bar shows prominently so the user never blames the tool or their line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncState {
    Synced,
    Syncing,
    Throttled { wait_secs: u32 },
    Paused,
    Error { reason: String },
}

/// One active transfer row.
#[derive(Debug, Clone)]
pub struct Transfer {
    pub name: String,
    pub up: bool,
    pub percent: u8,
}

/// The view-model the status bar renders.
#[derive(Debug, Clone)]
pub struct StatusView {
    pub account: String,
    pub state: SyncState,
    pub transfers: Vec<Transfer>,
    pub down_mbps: f32,
    pub up_mbps: f32,
    pub queue: u32,
}

/// What a click on a status-bar control means. The window layer turns this into
/// an engine call / a browser launch; the model below stays GUI-framework-free
/// and headless-testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Toggle the engine between paused and running.
    TogglePause,
    /// Open the full web UI in the user's browser.
    OpenBrowser,
}

/// On-screen rectangles of the two controls, as `(x, y, w, h)`. Shared by
/// [`render_with`] (which draws them) and [`hit_test`] (which maps a click back
/// to an [`Action`]) so the drawn buttons and the hit boxes never drift apart.
const PAUSE_BTN: (f32, f32, f32, f32) = (16.0, 470.0, 150.0, 34.0);
const BROWSER_BTN: (f32, f32, f32, f32) = (WIDTH as f32 - 16.0 - 174.0, 470.0, 174.0, 34.0);

fn in_rect(x: f32, y: f32, r: (f32, f32, f32, f32)) -> bool {
    x >= r.0 && x < r.0 + r.2 && y >= r.1 && y < r.1 + r.3
}

/// Map a click at `(x, y)` (in render pixels) to the control it hit, if any.
pub fn hit_test(x: f32, y: f32) -> Option<Action> {
    if in_rect(x, y, PAUSE_BTN) {
        Some(Action::TogglePause)
    } else if in_rect(x, y, BROWSER_BTN) {
        Some(Action::OpenBrowser)
    } else {
        None
    }
}

/// Apply an [`Action`] to the view model (the headless event-dispatch half).
/// `TogglePause` flips paused↔running; `OpenBrowser` doesn't change the view (the
/// caller launches the browser). Returns whether the view changed.
pub fn apply(view: &mut StatusView, action: Action) -> bool {
    match action {
        Action::TogglePause => {
            view.state = match view.state {
                SyncState::Paused => SyncState::Synced,
                _ => SyncState::Paused,
            };
            true
        }
        Action::OpenBrowser => false,
    }
}

fn col(r: u8, g: u8, b: u8) -> Color {
    Color::from_rgba8(r, g, b, 255)
}

fn rrect(x: f32, y: f32, w: f32, h: f32, r: f32) -> tiny_skia::Path {
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.quad_to(x + w, y, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.quad_to(x + w, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.quad_to(x, y + h, x, y + h - r);
    pb.line_to(x, y + r);
    pb.quad_to(x, y, x + r, y);
    pb.close();
    pb.finish().unwrap()
}

fn fill_rrect(pm: &mut Pixmap, x: f32, y: f32, w: f32, h: f32, r: f32, c: Color) {
    let mut p = Paint::default();
    p.set_color(c);
    p.anti_alias = true;
    pm.fill_path(
        &rrect(x, y, w, h, r),
        &p,
        FillRule::Winding,
        Transform::identity(),
        None,
    );
}

fn fill_rect(pm: &mut Pixmap, x: f32, y: f32, w: f32, h: f32, c: Color) {
    let mut p = Paint::default();
    p.set_color(c);
    if let Some(rect) = Rect::from_xywh(x, y, w, h) {
        pm.fill_rect(rect, &p, Transform::identity(), None);
    }
}

fn grad_rect(pm: &mut Pixmap, x: f32, y: f32, w: f32, h: f32, c0: Color, c1: Color) {
    if let Some(shader) = LinearGradient::new(
        Point::from_xy(x, y),
        Point::from_xy(x + w, y),
        vec![GradientStop::new(0.0, c0), GradientStop::new(1.0, c1)],
        SpreadMode::Pad,
        Transform::identity(),
    ) {
        let p = Paint {
            shader,
            anti_alias: true,
            ..Default::default()
        };
        if let Some(rect) = Rect::from_xywh(x, y, w, h) {
            pm.fill_rect(rect, &p, Transform::identity(), None);
        }
    }
}

fn fill_circle(pm: &mut Pixmap, cx: f32, cy: f32, r: f32, c: Color) {
    let mut p = Paint::default();
    p.set_color(c);
    p.anti_alias = true;
    let mut pb = PathBuilder::new();
    pb.push_circle(cx, cy, r);
    pm.fill_path(
        &pb.finish().unwrap(),
        &p,
        FillRule::Winding,
        Transform::identity(),
        None,
    );
}

fn blend_px(data: &mut [u8], pw: u32, ph: u32, x: i32, y: i32, c: CtColor) {
    if x < 0 || y < 0 || x as u32 >= pw || y as u32 >= ph {
        return;
    }
    let a = c.a() as f32 / 255.0;
    if a <= 0.0 {
        return;
    }
    let i = ((y as u32 * pw + x as u32) * 4) as usize;
    data[i] = (c.r() as f32 * a + data[i] as f32 * (1.0 - a)) as u8;
    data[i + 1] = (c.g() as f32 * a + data[i + 1] as f32 * (1.0 - a)) as u8;
    data[i + 2] = (c.b() as f32 * a + data[i + 2] as f32 * (1.0 - a)) as u8;
    data[i + 3] = (c.a() as f32 + data[i + 3] as f32 * (1.0 - a)) as u8;
}

#[allow(clippy::too_many_arguments)]
fn text(
    pm: &mut Pixmap,
    fs: &mut FontSystem,
    sc: &mut SwashCache,
    x: f32,
    y: f32,
    size: f32,
    c: Color,
    s: &str,
) {
    let mut buf = Buffer::new(fs, Metrics::new(size, size * 1.3));
    buf.set_size(Some(WIDTH as f32), Some(size * 2.0));
    buf.set_text(
        s,
        &Attrs::new().family(Family::Name(FONT_FAMILY)),
        Shaping::Advanced,
        None,
    );
    buf.shape_until_scroll(fs, false);
    let (pw, ph) = (pm.width(), pm.height());
    let tc = CtColor::rgb(
        (c.red() * 255.0) as u8,
        (c.green() * 255.0) as u8,
        (c.blue() * 255.0) as u8,
    );
    let data = pm.data_mut();
    let (ox, oy) = (x as i32, y as i32);
    buf.draw(fs, sc, tc, |gx, gy, gw, gh, color| {
        for dy in 0..gh as i32 {
            for dx in 0..gw as i32 {
                blend_px(data, pw, ph, ox + gx + dx, oy + gy + dy, color);
            }
        }
    });
}

/// Render the status bar to a pixel buffer (the headless == on-screen image).
pub fn render(view: &StatusView) -> Pixmap {
    let mut fs = bundled_font_system();
    let mut sc = SwashCache::new();
    render_with(view, &mut fs, &mut sc)
}

/// Render reusing a font system (avoids re-scanning fonts each frame).
pub fn render_with(view: &StatusView, fs: &mut FontSystem, sc: &mut SwashCache) -> Pixmap {
    let mut pm = Pixmap::new(WIDTH, HEIGHT).unwrap();
    pm.fill(col(0x0c, 0x12, 0x1d));
    let pad = 16.0;

    // header
    fill_rrect(&mut pm, pad, 14.0, 30.0, 30.0, 8.0, col(0x1d, 0x4e, 0xd8));
    text(
        &mut pm,
        fs,
        sc,
        pad + 40.0,
        13.0,
        17.0,
        col(0xff, 0xff, 0xff),
        "OneDrive",
    );
    text(
        &mut pm,
        fs,
        sc,
        pad + 40.0,
        35.0,
        11.5,
        col(0x8e, 0x9d, 0xb3),
        &view.account,
    );

    // status pill
    let pill_w = 140.0;
    let pill_x = WIDTH as f32 - pad - pill_w;
    let (pill_bg, pill_brd, pill_dot, pill_fg, label) = match &view.state {
        SyncState::Synced => (
            col(0x06, 0x2c, 0x1d),
            col(0x15, 0x7f, 0x4e),
            col(0x22, 0xc5, 0x5e),
            col(0x4a, 0xde, 0x80),
            "Synced".to_string(),
        ),
        SyncState::Syncing => (
            col(0x10, 0x24, 0x40),
            col(0x1d, 0x4e, 0xd8),
            col(0x38, 0xbd, 0xf8),
            col(0x8e, 0xc6, 0xff),
            "Syncing\u{2026}".to_string(),
        ),
        SyncState::Throttled { wait_secs } => (
            col(0x44, 0x22, 0x06),
            col(0xa1, 0x62, 0x07),
            col(0xf5, 0x9e, 0x0b),
            col(0xfb, 0xbf, 0x24),
            format!("Throttled {wait_secs}s"),
        ),
        SyncState::Paused => (
            col(0x2a, 0x2a, 0x2a),
            col(0x55, 0x55, 0x55),
            col(0x99, 0x99, 0x99),
            col(0xcc, 0xcc, 0xcc),
            "Paused".to_string(),
        ),
        SyncState::Error { .. } => (
            col(0x45, 0x0a, 0x0a),
            col(0xb9, 0x1c, 0x1c),
            col(0xef, 0x44, 0x44),
            col(0xfc, 0xa5, 0xa5),
            "Error".to_string(),
        ),
    };
    fill_rrect(&mut pm, pill_x, 16.0, pill_w, 28.0, 14.0, pill_bg);
    let mut sp = Paint::default();
    sp.set_color(pill_brd);
    sp.anti_alias = true;
    let st = Stroke {
        width: 1.0,
        ..Default::default()
    };
    pm.stroke_path(
        &rrect(pill_x, 16.0, pill_w, 28.0, 14.0),
        &sp,
        &st,
        Transform::identity(),
        None,
    );
    fill_circle(&mut pm, pill_x + 16.0, 30.0, 4.0, pill_dot);
    text(&mut pm, fs, sc, pill_x + 26.0, 22.0, 12.0, pill_fg, &label);

    // prominent reason banner — only for states the user must understand at a
    // glance, so they never suspect the tool or their connection (plan §13/§25).
    let banner: Option<(Color, Color, String)> = match &view.state {
        // the wait time is already on the pill ("Throttled Ns"); the banner's job
        // is reassurance — keep it short enough to never clip the strip.
        SyncState::Throttled { .. } => Some((
            col(0x44, 0x22, 0x06),
            col(0xfb, 0xbf, 0x24),
            "\u{26a0} Throttled by Microsoft (429) \u{2014} not your connection".to_string(),
        )),
        SyncState::Error { reason } => Some((
            col(0x45, 0x0a, 0x0a),
            col(0xfc, 0xa5, 0xa5),
            format!("\u{26a0} {reason}"),
        )),
        _ => None,
    };
    if let Some((bg, fg, msg)) = banner {
        let bw = WIDTH as f32 - 2.0 * pad;
        fill_rrect(&mut pm, pad, 48.0, bw, 17.0, 6.0, bg);
        text(&mut pm, fs, sc, pad + 9.0, 51.0, 11.0, fg, &msg);
    }

    // transfer rows
    for (i, t) in view.transfers.iter().take(3).enumerate() {
        let yy = 70.0 + i as f32 * 56.0;
        let rw = WIDTH as f32 - 2.0 * pad;
        fill_rrect(&mut pm, pad, yy, rw, 48.0, 10.0, col(0x16, 0x20, 0x2e));
        let (arr, ac) = if t.up {
            ("\u{2191}", col(0x38, 0xbd, 0xf8))
        } else {
            ("\u{2193}", col(0x34, 0xd3, 0x99))
        };
        text(&mut pm, fs, sc, pad + 12.0, yy + 13.0, 16.0, ac, arr);
        text(
            &mut pm,
            fs,
            sc,
            pad + 34.0,
            yy + 9.0,
            12.5,
            col(0xe6, 0xed, 0xf5),
            &t.name,
        );
        // progress bar
        let bx = pad + 34.0;
        let bw = rw - 48.0 - 40.0;
        let by = yy + 30.0;
        fill_rrect(&mut pm, bx, by, bw, 7.0, 3.5, col(0x29, 0x37, 0x4b));
        let pw = (bw * (t.percent.min(100) as f32 / 100.0)).max(1.0);
        grad_rect(
            &mut pm,
            bx,
            by,
            pw,
            7.0,
            col(0x3b, 0x82, 0xf6),
            col(0x06, 0xb6, 0xd4),
        );
        text(
            &mut pm,
            fs,
            sc,
            WIDTH as f32 - pad - 36.0,
            yy + 9.0,
            12.0,
            col(0xcb, 0xd5, 0xe1),
            &format!("{}%", t.percent.min(100)),
        );
    }

    // aggregate footer
    let py = 70.0 + 3.0 * 56.0 + 8.0;
    text(
        &mut pm,
        fs,
        sc,
        pad,
        py,
        13.0,
        col(0x34, 0xd3, 0x99),
        &format!("\u{2193} {:.1} MB/s", view.down_mbps),
    );
    text(
        &mut pm,
        fs,
        sc,
        pad + 130.0,
        py,
        13.0,
        col(0x38, 0xbd, 0xf8),
        &format!("\u{2191} {:.1} MB/s", view.up_mbps),
    );
    text(
        &mut pm,
        fs,
        sc,
        WIDTH as f32 - pad - 120.0,
        py,
        13.0,
        col(0x8e, 0x9d, 0xb3),
        &format!("{} in Queue", view.queue),
    );

    // action buttons — Pause/Resume + open the full UI. Their rects live in
    // PAUSE_BTN / BROWSER_BTN so hit_test always matches what's drawn here.
    let pause_label = if matches!(view.state, SyncState::Paused) {
        "Resume"
    } else {
        "Pause"
    };
    fill_rrect(
        &mut pm,
        PAUSE_BTN.0,
        PAUSE_BTN.1,
        PAUSE_BTN.2,
        PAUSE_BTN.3,
        8.0,
        col(0x18, 0x23, 0x3a),
    );
    text(
        &mut pm,
        fs,
        sc,
        PAUSE_BTN.0 + 16.0,
        PAUSE_BTN.1 + 9.0,
        13.0,
        col(0xcd, 0xd6, 0xe4),
        pause_label,
    );
    fill_rrect(
        &mut pm,
        BROWSER_BTN.0,
        BROWSER_BTN.1,
        BROWSER_BTN.2,
        BROWSER_BTN.3,
        8.0,
        col(0x1d, 0x4e, 0xd8),
    );
    text(
        &mut pm,
        fs,
        sc,
        BROWSER_BTN.0 + 16.0,
        BROWSER_BTN.1 + 9.0,
        13.0,
        col(0xff, 0xff, 0xff),
        "Open in browser",
    );

    // footer hint
    fill_rect(
        &mut pm,
        pad,
        HEIGHT as f32 - 40.0,
        WIDTH as f32 - 2.0 * pad,
        1.0,
        col(0x1f, 0x29, 0x37),
    );
    text(
        &mut pm,
        fs,
        sc,
        pad,
        HEIGHT as f32 - 28.0,
        11.0,
        col(0x6b, 0x7a, 0x8e),
        "Restore, mail & search \u{2192} in browser",
    );
    pm
}

/// Render and encode to PNG (headless verification / screenshots).
pub fn render_png(view: &StatusView) -> Vec<u8> {
    render(view).encode_png().expect("pixmap PNG encode")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> StatusView {
        StatusView {
            account: "you@example.com".into(),
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

    #[test]
    fn renders_correct_dimensions() {
        let pm = render(&sample());
        assert_eq!((pm.width(), pm.height()), (WIDTH, HEIGHT));
    }

    #[test]
    fn background_pixel_is_dark() {
        let pm = render(&sample());
        let px = &pm.data()[0..4]; // top-left, RGBA
        assert!(
            px[0] < 40 && px[1] < 50 && px[2] < 60,
            "bg not dark: {px:?}"
        );
        assert_eq!(px[3], 255, "bg must be opaque");
    }

    #[test]
    fn shapes_make_it_non_blank() {
        // shape fills are deterministic regardless of available fonts: the image
        // must contain pixels different from the background.
        let pm = render(&sample());
        let bg = pm.data()[0..3].to_vec();
        let differs = pm.data().chunks_exact(4).any(|p| p[0..3] != bg[..]);
        assert!(differs, "render produced a blank image");
    }

    #[test]
    fn throttled_state_renders() {
        let mut v = sample();
        v.state = SyncState::Throttled { wait_secs: 14 };
        let pm = render(&v);
        assert_eq!((pm.width(), pm.height()), (WIDTH, HEIGHT));
    }

    /// RGB of the pixel at `(x, y)` in a rendered pixmap.
    fn px(pm: &Pixmap, x: u32, y: u32) -> [u8; 3] {
        let i = ((y * pm.width() + x) * 4) as usize;
        [pm.data()[i], pm.data()[i + 1], pm.data()[i + 2]]
    }

    #[test]
    fn error_state_renders_red() {
        let mut v = sample();
        v.state = SyncState::Error {
            reason: "Token expired — sign in again".into(),
        };
        let pm = render(&v);
        // the otherwise blue/green theme has no strong red; the error pill + banner do.
        let reddish = pm
            .data()
            .chunks_exact(4)
            .any(|p| p[0] > 120 && p[0] as u16 > p[1] as u16 * 2 && p[0] as u16 > p[2] as u16 * 2);
        assert!(
            reddish,
            "error state must paint a prominent red pill/banner"
        );
    }

    #[test]
    fn reason_banner_only_for_throttled_and_error() {
        // a point well inside the banner strip (y=48..65), clear of the pill/header
        let (bx, by) = (40, 56);
        let bg = {
            let mut v = sample();
            v.state = SyncState::Synced;
            px(&render(&v), bx, by) // no banner: background shows through
        };
        for st in [
            SyncState::Throttled { wait_secs: 9 },
            SyncState::Error {
                reason: "disk full".into(),
            },
        ] {
            let mut v = sample();
            v.state = st.clone();
            assert_ne!(
                px(&render(&v), bx, by),
                bg,
                "{st:?} must paint a reason banner"
            );
        }
    }

    #[test]
    fn apply_pauses_from_error() {
        let mut v = sample();
        v.state = SyncState::Error {
            reason: "boom".into(),
        };
        assert!(apply(&mut v, Action::TogglePause));
        assert_eq!(v.state, SyncState::Paused);
    }

    #[test]
    fn render_png_has_png_signature() {
        let png = render_png(&sample());
        assert_eq!(&png[1..4], b"PNG", "expected PNG signature");
    }

    #[test]
    fn hit_test_maps_clicks_to_controls() {
        // a point inside each button's rect → its action
        let pc = (PAUSE_BTN.0 + 5.0, PAUSE_BTN.1 + 5.0);
        assert_eq!(hit_test(pc.0, pc.1), Some(Action::TogglePause));
        let bc = (BROWSER_BTN.0 + 5.0, BROWSER_BTN.1 + 5.0);
        assert_eq!(hit_test(bc.0, bc.1), Some(Action::OpenBrowser));
        // empty space (the header) hits nothing
        assert_eq!(hit_test(5.0, 5.0), None);
        // just outside the pause button (right edge) hits nothing
        assert_eq!(
            hit_test(PAUSE_BTN.0 + PAUSE_BTN.2 + 1.0, PAUSE_BTN.1 + 5.0),
            None
        );
    }

    #[test]
    fn apply_toggles_pause_and_browser_is_inert() {
        let mut v = sample();
        v.state = SyncState::Synced;
        assert!(apply(&mut v, Action::TogglePause));
        assert_eq!(v.state, SyncState::Paused);
        assert!(apply(&mut v, Action::TogglePause));
        assert_eq!(v.state, SyncState::Synced);
        // OpenBrowser changes nothing in the model
        let before = v.state.clone();
        assert!(!apply(&mut v, Action::OpenBrowser));
        assert_eq!(v.state, before);
    }

    #[test]
    fn bundled_font_system_loads_only_the_bundled_font() {
        // Exactly one face (no system scan) => deterministic, font-less-host safe.
        let fs = bundled_font_system();
        assert_eq!(fs.db().faces().count(), 1);
        // ...and it is JetBrains Mono (the user-chosen monospace), not a system font.
        let face = fs.db().faces().next().expect("one bundled face");
        assert!(
            face.families
                .iter()
                .any(|(name, _)| name.contains("JetBrains Mono")),
            "bundled face families = {:?}",
            face.families
        );
    }

    #[test]
    fn header_text_renders_glyph_pixels() {
        // With the bundled font, the white "OneDrive" title paints glyph pixels in
        // the text band (x 56..200, y 10..48), which holds text only — no shapes.
        // A font-less render would leave this band at the dark background.
        let pm = render(&sample());
        let w = pm.width() as usize;
        let data = pm.data();
        let mut bright = 0usize;
        for y in 10..48usize {
            for x in 56..200usize {
                let i = (y * w + x) * 4;
                if data[i] > 90 && data[i + 1] > 90 && data[i + 2] > 90 {
                    bright += 1;
                }
            }
        }
        assert!(
            bright > 30,
            "expected bundled-font glyph pixels in the title band, got {bright}"
        );
    }
}
