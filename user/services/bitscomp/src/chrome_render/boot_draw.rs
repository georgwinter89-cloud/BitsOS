//! DS-5: the boot scene's picture, the kernel's boot screen (crates/console/src/bootdraw.rs: the design's board
//! "Start", Absicht dunkel, cheap level) drawn again by bitscomp with the same integer maths, layout and colours, so
//! the handover shows no jump. The scene is the 1920x1080 design at 4/3, centred on screens of at least 2560x1440.
//! `draw` paints every part that meets the frame's rectangle, so a row drawn again equals the whole picture there.

use super::boot_art::{Face, Frame, Rect, Ring, Word, blend, ring};
use crate::text::{Typeface, Weight};

pub const SCENE_W: i32 = 2560;
pub const SCENE_H: i32 = 1440;

// Absicht dunkel (the design's tokens), as the kernel's boot screen.
pub const SUNKEN: u32 = 0x000c_120f;
const LINE: u32 = 0x002b_3731;
const LINE_STRONG: u32 = 0x006b_7a71;
const INK: u32 = 0x00e8_eee9;
const MUTED: u32 = 0x00a3_b0a8;
const ACCENT: u32 = 0x007f_cfa0;
const RAISED: u32 = 0x001b_2520;
const BUSY: u32 = 0x00e3_bd5f;
const WARN: u32 = 0x00ff_7b6b;
const VIGNETTE: u32 = 0x0004_0806;

/// Design pixels to screen pixels (x 4/3, rounded).
pub const fn px(v: i32) -> i32 {
    (v * 4 + 1) / 3
}

const LABEL_SPACING: i32 = 300;
const ROWS_LEFT: i32 = px(96);
pub const ROW_W: i32 = px(520);
const PROGRESS_LEFT: i32 = px(1364);
const PROGRESS_RIGHT: i32 = px(1824);
pub const RING_CENTRE: (i32, i32) = (1280, px(380));
const RING_X: i32 = RING_CENTRE.0 - 347;
const RING_Y: i32 = RING_CENTRE.1 - 347;
const TITLE_TOP: i32 = 664;
const STATUS_TOP: i32 = TITLE_TOP + 84 + 10;
const LIST_TOP: i32 = 820;
const LIST_PITCH: i32 = 24;
/// The hint at the bottom right: the kernel's says the text console; in bitscomp a key opens the desktop.
pub const HINT: &str = "BELIEBIGE TASTE: ARBEITSFLÄCHE";

pub fn row_top(i: usize) -> i32 {
    px(LIST_TOP + LIST_PITCH * i as i32)
}

/// The rows that fit above the scene's bottom (the board has room for ten).
pub fn rows_shown() -> usize {
    (0..).take_while(|i| row_top(i + 1) <= SCENE_H).count()
}

/// A row's state, as `/proc/bitsos/boot` names them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Waiting,
    Running,
    Done,
    Failed,
    Skipped,
}

impl State {
    pub fn parse(s: &str) -> Option<State> {
        Some(match s {
            "waiting" => State::Waiting,
            "running" => State::Running,
            "done" => State::Done,
            "failed" => State::Failed,
            "skipped" => State::Skipped,
            _ => return None,
        })
    }

    pub fn is_final(self) -> bool {
        matches!(self, State::Done | State::Failed | State::Skipped)
    }
}

/// What one row shows, cut to fit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub state: State,
    pub label: String,
    pub detail: String,
    pub time: String,
}

/// Everything the picture shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct View {
    pub rows: Vec<Row>,
    pub status: String,
    pub status_warn: bool,
    pub progress: (usize, usize),
    pub header: String,
}

/// The fonts, the title and the ring, made once per scene.
pub struct Art {
    pub mono: Face,
    label: Face,
    text: Face,
    title: Word,
    ring: Ring,
}

impl Art {
    /// The ring (the dearest piece, ~40 ms on the Gaming-PC) is made on a thread of its own beside the fonts.
    pub fn new() -> Art {
        std::thread::scope(|s| {
            let ring = std::thread::Builder::new().name("bitscomp-boot".into()).spawn_scoped(s, Ring::new);
            let mono = Face::new(Typeface::JetBrainsMono, Weight::Regular, 13.0);
            let label = Face::new(Typeface::JetBrainsMono, Weight::Medium, 11.0);
            let text = Face::new(Typeface::Figtree, Weight::Regular, 20.0);
            let title = Word::new(Typeface::Bricolage, Weight::SemiBold, 80.0, "BitsOS", -0.01);
            // No thread (or a failed one): made here instead.
            let ring = ring.ok().and_then(|h| h.join().ok()).unwrap_or_else(Ring::new);
            Art { mono, label, text, title, ring }
        })
    }

    /// Room for a row's detail next to its time (the kernel's `View::of` reserves "00,00 s"; a time of 100 s or
    /// more is wider and takes its own width, so it never runs into the detail; below 100 s this is the kernel's cut).
    pub fn detail_max(&self, time: &str) -> i32 {
        ROW_W - px(156) - self.mono.width("00,00 s", 0).max(self.mono.width(time, 0)) - px(12)
    }
}

/// A radial gradient: alpha by t² over an ellipse (centre and radii in half pixels), 1024 steps.
struct Radial {
    cx2: i64,
    cy2: i64,
    kx: u64,
    ky: u64,
    lut: [u8; 1025],
}

impl Radial {
    fn new(cx2: i64, cy2: i64, rx2: i64, ry2: i64, stops: &[(u32, u32)]) -> Radial {
        let rx = (rx2 * rx2) as u64;
        let ry = (ry2 * ry2) as u64;
        let mut lut = [0u8; 1025];
        for (i, v) in lut.iter_mut().enumerate() {
            let t = ((i as u64 * 1_000_000 / 1024).isqrt()) as u32;
            let a = alpha_at(stops, t);
            *v = ((a * 255 + 500) / 1000) as u8;
        }
        Radial { cx2, cy2, kx: (1024u64 << 32) / rx.max(1), ky: (1024u64 << 32) / ry.max(1), lut }
    }

    fn at(&self, x2: i64, y2: i64) -> u8 {
        let (dx, dy) = ((x2 - self.cx2).unsigned_abs(), (y2 - self.cy2).unsigned_abs());
        let i = (dx * dx).saturating_mul(self.kx).saturating_add((dy * dy).saturating_mul(self.ky)) >> 32;
        self.lut[(i as usize).min(1024)]
    }
}

fn alpha_at(stops: &[(u32, u32)], t: u32) -> u32 {
    let mut prev = stops[0];
    if t <= prev.0 {
        return prev.1;
    }
    for &s in &stops[1..] {
        if t <= s.0 {
            let span = (s.0 - prev.0).max(1);
            let k = t - prev.0;
            return (prev.1 * (span - k) + s.1 * k) / span;
        }
        prev = s;
    }
    prev.1
}

/// The boot scene on one screen size: where it lies and its background gradients.
pub struct Scene {
    pub ox: i32,
    pub oy: i32,
    pub w: i32,
    pub h: i32,
    glow: Radial,
    vignette: Radial,
    disc: Radial,
    halo: Radial,
}

impl Scene {
    /// None when the screen is smaller than the scene (the kernel shows none there either).
    pub fn new(w: i32, h: i32) -> Option<Scene> {
        if w < SCENE_W || h < SCENE_H {
            return None;
        }
        Some(Scene {
            ox: (w - SCENE_W) / 2,
            oy: (h - SCENE_H) / 2,
            w,
            h,
            glow: Radial::new(2560, 1173, 1600, 1333, &[(0, 160), (550, 40), (1000, 0)]),
            vignette: Radial::new(2560, 1440, 3620, 2036, &[(450, 0), (1000, 800)]),
            disc: Radial::new(2 * RING_CENTRE.0 as i64, 2 * RING_CENTRE.1 as i64, 683, 683, &[(0, 250), (700, 30), (1000, 0)]),
            halo: Radial::new(2560, 2 * (px(TITLE_TOP) + px(84) / 2) as i64, 640, 200, &[(0, 110), (1000, 0)]),
        })
    }

    pub fn full(&self) -> Rect {
        Rect::new(0, 0, self.w, self.h)
    }

    /// The background at scene pixel (x, y): dot grid, glow, vignette, the ring's disc, the title's halo and the
    /// baked scanlines (one row in four).
    fn bg(&self, x: i32, y: i32) -> u32 {
        let mut c = SUNKEN;
        let (gx, gy) = (x.rem_euclid(32), y.rem_euclid(32));
        if (14..=17).contains(&gx) && (14..=17).contains(&gy) {
            let (dx, dy) = (2 * gx + 1 - 32, 2 * gy + 1 - 32);
            match dx * dx + dy * dy {
                2 => c = blend(LINE, c, 178),
                10 => c = blend(LINE, c, 68),
                _ => {}
            }
        }
        let (x2, y2) = (2 * x as i64 + 1, 2 * y as i64 + 1);
        c = blend(ACCENT, c, self.glow.at(x2, y2));
        c = blend(VIGNETTE, c, self.vignette.at(x2, y2));
        c = blend(ACCENT, c, self.disc.at(x2, y2));
        c = blend(ACCENT, c, self.halo.at(x2, y2));
        if y.rem_euclid(4) == 0 { blend(INK, c, 9) } else { c }
    }

    fn rect(&self, x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rect::new(self.ox + x, self.oy + y, w, h)
    }

    fn fill_bg(&self, f: &mut Frame, r: Rect) {
        let r = r.cut(f.at);
        for y in r.y..r.y + r.h {
            for x in r.x..r.x + r.w {
                let c = self.bg(x - self.ox, y - self.oy);
                if let Some(p) = f.get_mut(x, y) {
                    *p = c;
                }
            }
        }
    }

    /// Every part of the picture inside the frame's rectangle (in this order, as the kernel's `draw_band`).
    pub fn draw(&self, f: &mut Frame, v: &View, art: &Art) {
        let band = f.at.cut(self.full());
        if band.is_empty() {
            return;
        }
        let all = self.rect(0, 0, SCENE_W, SCENE_H).cut(band);
        if self.ox > 0 || self.oy > 0 {
            for p in f.px.iter_mut() {
                *p = SUNKEN;
            }
        }
        self.fill_bg(f, all);
        ring(f, &art.ring, self.ox + RING_X, self.oy + RING_Y, all);
        let (tw, top) = (art.title.w as i32, px(TITLE_TOP));
        let base = top + (px(84) - (art.title.ascent - art.title.descent)) / 2;
        art.title.draw(f, self.ox + RING_CENTRE.0 - tw / 2, self.oy + base, INK, all);
        self.dot(f, px(52), px(47), all);
        self.label(f, art, px(68), px(40), "BITSOS V2 · START", MUTED, false, all);
        self.label(f, art, px(1824), px(1020), HINT, LINE_STRONG, true, all);
        self.header(f, v, art, band);
        for (i, r) in v.rows.iter().enumerate().take(rows_shown()) {
            self.row(f, art, i, r, band);
        }
        self.status(f, v, art, band);
        self.progress(f, v, art, band);
    }

    /// The whole picture, drawn in bands on up to 8 threads (each band equals the whole picture there).
    pub fn picture(&self, v: &View, art: &Art) -> Vec<u32> {
        let n = std::thread::available_parallelism().map_or(1, |n| n.get()).clamp(1, 8) as i32;
        let step = (self.h + n - 1) / n;
        let bands: Vec<Rect> = (0..n).map(|i| Rect::new(0, i * step, self.w, step.min(self.h - i * step))).filter(|r| !r.is_empty()).collect();
        let draw = |r: Rect| {
            let mut f = Frame::new(r);
            self.draw(&mut f, v, art);
            f.px
        };
        let parts: Vec<Vec<u32>> = std::thread::scope(|s| {
            let handles: Vec<_> = bands[1..]
                .iter()
                .map(|&r| std::thread::Builder::new().name("bitscomp-boot".into()).spawn_scoped(s, move || draw(r)))
                .collect();
            let mut parts = vec![draw(bands[0])];
            for (h, &r) in handles.into_iter().zip(&bands[1..]) {
                parts.push(h.ok().and_then(|h| h.join().ok()).unwrap_or_else(|| draw(r)));
            }
            parts
        });
        parts.concat()
    }

    /// The screen rectangles that differ between `old` and `new` (drawn again with `draw`).
    pub fn changes(&self, old: &View, new: &View) -> Vec<Rect> {
        let mut out = Vec::new();
        let n = new.rows.len().max(old.rows.len()).min(rows_shown());
        for i in 0..n {
            if old.rows.get(i) != new.rows.get(i) {
                out.push(self.row_rect(i));
            }
        }
        if (&old.status, old.status_warn) != (&new.status, new.status_warn) {
            out.push(self.status_rect());
        }
        if old.progress != new.progress {
            out.push(self.progress_rect());
        }
        if old.header != new.header {
            out.push(self.header_rect());
        }
        out
    }

    fn row_rect(&self, i: usize) -> Rect {
        let top = row_top(i);
        self.rect(ROWS_LEFT, top, ROW_W, row_top(i + 1) - top)
    }

    fn status_rect(&self) -> Rect {
        self.rect(ROWS_LEFT, px(STATUS_TOP), SCENE_W - 2 * ROWS_LEFT, px(28))
    }

    fn progress_rect(&self) -> Rect {
        let top = px(LIST_TOP + 16);
        let bar_top = px(LIST_TOP + 16 + 14 + 10);
        self.rect(PROGRESS_LEFT - 8, top, PROGRESS_RIGHT - PROGRESS_LEFT + 16, bar_top + px(6) + 8 - top)
    }

    fn header_rect(&self) -> Rect {
        self.rect(px(1200), px(36), px(1872) - px(1200), px(22))
    }

    #[allow(clippy::too_many_arguments)]
    fn label(&self, f: &mut Frame, art: &Art, x: i32, top: i32, s: &str, color: u32, right: bool, clip: Rect) {
        let x = if right { x - art.label.width(s, LABEL_SPACING) } else { x };
        let base = art.label.baseline(top, px(14));
        art.label.text(f, self.ox + x, self.oy + base, s, color, LABEL_SPACING, clip);
    }

    fn header(&self, f: &mut Frame, v: &View, art: &Art, band: Rect) {
        let r = self.header_rect().cut(band);
        if r.is_empty() {
            return;
        }
        self.fill_bg(f, r);
        self.label(f, art, px(1872), px(40), &v.header, MUTED, true, r);
    }

    fn row(&self, f: &mut Frame, art: &Art, i: usize, row: &Row, band: Rect) {
        let top = row_top(i);
        let r = self.row_rect(i).cut(band);
        if r.is_empty() {
            return;
        }
        self.fill_bg(f, r);
        let (mark, mc) = match row.state {
            State::Done => ("ok", ACCENT),
            State::Running => ("··", BUSY),
            State::Failed => ("!!", WARN),
            State::Skipped => ("––", LINE_STRONG),
            State::Waiting => ("··", LINE_STRONG),
        };
        let (lc, dc) = match row.state {
            State::Waiting => (LINE_STRONG, LINE_STRONG),
            State::Failed => (MUTED, WARN),
            State::Skipped => (MUTED, MUTED),
            _ => (MUTED, INK),
        };
        let mono = &art.mono;
        let base = self.oy + mono.baseline(top, px(20));
        let x = self.ox + ROWS_LEFT;
        mono.text(f, x, base, mark, mc, 0, r);
        mono.text(f, x + px(36), base, &row.label, lc, 0, r);
        mono.text(f, x + px(156), base, &row.detail, dc, 0, r);
        if row.state != State::Waiting {
            let tx = x + ROW_W - mono.width(&row.time, 0);
            mono.text(f, tx, base, &row.time, MUTED, 0, r);
        }
    }

    fn status(&self, f: &mut Frame, v: &View, art: &Art, band: Rect) {
        let (top, line) = (px(STATUS_TOP), px(28));
        let r = self.status_rect().cut(band);
        if r.is_empty() {
            return;
        }
        self.fill_bg(f, r);
        let w = art.text.width(&v.status, 0);
        let base = self.oy + art.text.baseline(top, line);
        art.text.text(f, self.ox + RING_CENTRE.0 - w / 2, base, &v.status, if v.status_warn { WARN } else { MUTED }, 0, r);
    }

    fn progress(&self, f: &mut Frame, v: &View, art: &Art, band: Rect) {
        let top = px(LIST_TOP + 16);
        let bar_top = px(LIST_TOP + 16 + 14 + 10);
        let r = self.progress_rect().cut(band);
        if r.is_empty() {
            return;
        }
        self.fill_bg(f, r);
        self.label(f, art, PROGRESS_LEFT, top, "SCHRITTE", MUTED, false, r);
        let (done, all) = v.progress;
        self.label(f, art, PROGRESS_RIGHT, top, &format!("{done} / {all}"), ACCENT, true, r);
        let w = PROGRESS_RIGHT - PROGRESS_LEFT;
        let (x, h) = (self.ox + PROGRESS_LEFT, px(6));
        let y = self.oy + bar_top;
        rrect(f, x, y, w, h, RAISED, 255, r);
        let fill = if all == 0 { 0 } else { w * done.min(all) as i32 / all as i32 };
        if fill > 0 {
            rrect(f, x - 3, y - 3, fill + 6, h + 6, ACCENT, 46, r);
            rrect(f, x, y, fill, h, ACCENT, 255, r);
        }
    }

    /// The live dot of the header (static at the cheap level) with its soft glow.
    fn dot(&self, f: &mut Frame, cx: i32, cy: i32, clip: Rect) {
        let (cx16, cy16) = ((self.ox + cx) * 16, (self.oy + cy) * 16);
        disc(f, cx16, cy16, 12 * 16, 14 * 16, ACCENT, 60, clip);
        disc(f, cx16, cy16, 85, 16, ACCENT, 255, clip);
    }
}

/// A disc of radius `r16` (1/16 pixels) with an edge `soft16` wide, `color` at up to `alpha`.
#[allow(clippy::too_many_arguments)]
fn disc(f: &mut Frame, cx16: i32, cy16: i32, r16: i32, soft16: i32, color: u32, alpha: u32, clip: Rect) {
    let reach = (r16 + soft16) / 16 + 1;
    let (cx, cy) = (cx16 / 16, cy16 / 16);
    for y in cy - reach..=cy + reach {
        for x in cx - reach..=cx + reach {
            let (dx, dy) = ((x * 16 + 8 - cx16) as i64, (y * 16 + 8 - cy16) as i64);
            let d = (dx * dx + dy * dy).isqrt() as i32;
            let cov = ((r16 + soft16 / 2 - d) * 256 / soft16.max(1)).clamp(0, 256) as u32;
            f.put(clip, x, y, color, (cov * alpha / 256) as u8);
        }
    }
}

/// A rounded bar (radius h/2) at up to `alpha`.
#[allow(clippy::too_many_arguments)]
fn rrect(f: &mut Frame, x: i32, y: i32, w: i32, h: i32, color: u32, alpha: u32, clip: Rect) {
    let r16 = h * 8;
    for yy in y..y + h {
        for xx in x..x + w {
            let (px16, py16) = (xx * 16 + 8, yy * 16 + 8);
            let lx = px16.clamp(x * 16 + r16, ((x + w) * 16 - r16).max(x * 16 + r16));
            let (dx, dy) = ((px16 - lx) as i64, (py16 - (y * 16 + r16)) as i64);
            let d = (dx * dx + dy * dy).isqrt() as i32;
            let cov = ((r16 + 8 - d) * 16).clamp(0, 256) as u32;
            f.put(clip, xx, yy, color, (cov * alpha / 256) as u8);
        }
    }
}
