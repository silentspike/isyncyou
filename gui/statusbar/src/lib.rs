//! `isyncyou-statusbar` — the native mini status-bar renderer.
//!
//! Own renderer (tiny-skia shapes + cosmic-text text), no GUI framework. The
//! same code renders on-screen (via winit/softbuffer, added with the tray in
//! #56) and **headless** into a pixel buffer for AI/CI verification — so the
//! verified pixels equal the on-screen pixels by construction.
//!
//! [`render`] returns a [`tiny_skia::Pixmap`]; [`render_png`] encodes it.

use cosmic_text::{Attrs, Buffer, Color as CtColor, FontSystem, Metrics, Shaping, SwashCache};
use tiny_skia::{
    Color, FillRule, GradientStop, LinearGradient, Paint, PathBuilder, Pixmap, Point, Rect,
    SpreadMode, Stroke, Transform,
};

pub const WIDTH: u32 = 380;
pub const HEIGHT: u32 = 560;

/// Overall sync state shown in the status pill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncState {
    Synced,
    Syncing,
    Throttled { wait_secs: u32 },
    Paused,
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
    buf.set_size(fs, Some(WIDTH as f32), Some(size * 2.0));
    buf.set_text(fs, s, Attrs::new(), Shaping::Advanced);
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
    let mut fs = FontSystem::new();
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
            "Synchronisiert".to_string(),
        ),
        SyncState::Syncing => (
            col(0x10, 0x24, 0x40),
            col(0x1d, 0x4e, 0xd8),
            col(0x38, 0xbd, 0xf8),
            col(0x8e, 0xc6, 0xff),
            "Synchronisiert\u{2026}".to_string(),
        ),
        SyncState::Throttled { wait_secs } => (
            col(0x44, 0x22, 0x06),
            col(0xa1, 0x62, 0x07),
            col(0xf5, 0x9e, 0x0b),
            col(0xfb, 0xbf, 0x24),
            format!("Gedrosselt {wait_secs}s"),
        ),
        SyncState::Paused => (
            col(0x2a, 0x2a, 0x2a),
            col(0x55, 0x55, 0x55),
            col(0x99, 0x99, 0x99),
            col(0xcc, 0xcc, 0xcc),
            "Pausiert".to_string(),
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
        "Restore, Mail & Suche \u{2192} im Browser",
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
            account: "jan@outlook.com".into(),
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

    #[test]
    fn render_png_has_png_signature() {
        let png = render_png(&sample());
        assert_eq!(&png[1..4], b"PNG", "expected PNG signature");
    }
}
