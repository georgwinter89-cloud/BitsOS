//! DS-4: the boot screen's picture (the design's board "Start", Absicht dunkel, at the cheap effect level: no
//! spin, no pulse, no blur at run time) drawn from the step model into a RAM frame. Integer only, the same code
//! on the host (tests compare QEMU's screendump with it) and in the kernel. The scene is the 1920x1080 design at
//! 4/3, centred on screens of at least 2560x1440 (smaller screens keep the text console).
//!
//! `View::of` prepares every string outside the frame lock; `draw_band` and `draw_changes` only blend.

use crate::bootart::{self, LABEL, MONO, TEXT, TITLE};
use crate::bootsteps::{KEYS, State, Steps, seconds};
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use display::{RamFrame, Rect};

pub const SCENE_W: u32 = 2560;
pub const SCENE_H: u32 = 1440;

// Absicht dunkel (the design's tokens).
pub const SUNKEN: u32 = 0x000c_120f;
const LINE: u32 = 0x002b_3731;
const LINE_STRONG: u32 = 0x006b_7a71;
const INK: u32 = 0x00e8_eee9;
const MUTED: u32 = 0x00a3_b0a8;
pub const ACCENT: u32 = 0x007f_cfa0;
const RAISED: u32 = 0x001b_2520;
const BUSY: u32 = 0x00e3_bd5f;
const WARN: u32 = 0x00ff_7b6b;
const VIGNETTE: u32 = 0x0004_0806;

/// Design pixels to screen pixels (x 4/3, rounded).
pub const fn px(v: i32) -> i32 {
    (v * 4 + 1) / 3
}

/// Letter spacing of the design's `.lbl` (0.08 em of 11 px, at 4/3) in 1/256 pixels.
const LABEL_SPACING: i32 = 300;

const ROWS_LEFT: i32 = px(96);
const ROW_W: i32 = px(520);
const PROGRESS_LEFT: i32 = px(1364);
const PROGRESS_RIGHT: i32 = px(1824);
/// The ring's centre (the board's 520 px ring at top 120: centre 380 design px); the sprite is 694 px square.
pub const RING_CENTRE: (i32, i32) = (1280, px(380));
const RING_X: i32 = RING_CENTRE.0 - 347;
const RING_Y: i32 = RING_CENTRE.1 - 347;
/// The title block (title 84 px line, 10 px gap, greeting 28 px line) at the board's top 664.
const TITLE_TOP: i32 = 664;
const STATUS_TOP: i32 = TITLE_TOP + 84 + 10;
/// The step list at the board's top 820 with 20 px lines. The board's gap is 8 (pitch 28) for its 5 mock lines;
/// 10 real steps at 28 would end at 1100, below the 1080 board, so the gap is 4 (pitch 24, ends at 1056).
const LIST_TOP: i32 = 820;
const LIST_PITCH: i32 = 24;
/// Rows of the first full picture per frame-lock hold: the lock is released between bands, so the KernelRt
/// input thread and VT writes wait at most one band (B4), not the whole 2560x1440 picture.
pub const BAND_ROWS: u32 = 96;

/// Top of step row `i` in scene pixels.
pub fn row_top(i: usize) -> i32 {
    px(LIST_TOP + LIST_PITCH * i as i32)
}

/// The part of `a` inside `b` (empty when they do not meet).
fn cut(a: Rect, b: Rect) -> Rect {
    let (x0, y0) = (a.x.max(b.x), a.y.max(b.y));
    let (x1, y1) = ((a.x + a.w).min(b.x + b.w), (a.y + a.h).min(b.y + b.h));
    if x1 <= x0 || y1 <= y0 { Rect::new(x0, y0, 0, 0) } else { Rect::new(x0, y0, x1 - x0, y1 - y0) }
}

/// What one row shows, cut to fit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub state: State,
    pub label: &'static str,
    pub detail: String,
    pub time: String,
}

/// Everything the picture shows (only `draw_*` touch the frame).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct View {
    pub rows: Vec<Row>,
    pub status: String,
    pub status_warn: bool,
    pub progress: (usize, usize),
    pub header: String,
}

impl View {
    pub fn of(s: &Steps) -> View {
        let detail_max = ROW_W - px(156) - MONO.width("00,00 s", 0) - px(12);
        let rows = KEYS
            .iter()
            .map(|k| {
                let st = s.get(*k);
                let d = if st.state == State::Waiting { "wartet" } else { st.detail.as_str() };
                let (cut, more) = MONO.fit(d, detail_max, 0);
                let detail = if more { format!("{cut}…") } else { cut.to_string() };
                let time = if st.state == State::Waiting { String::new() } else { seconds(st.t_ms) };
                Row { state: st.state, label: k.label(), detail, time }
            })
            .collect();
        let status = s.status();
        let status_warn = KEYS.iter().any(|k| s.get(*k).state == State::Failed) && !KEYS.iter().any(|k| s.get(*k).state == State::Running);
        let header = if s.generation > 0 { format!("GENERATION {}", s.generation) } else { String::from("BITSOS V2") };
        View { rows, status, status_warn, progress: s.progress(), header }
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
    /// `stops`: (t, alpha) in thousandths, ascending t; beyond the last stop its alpha stays.
    fn new(cx2: i64, cy2: i64, rx2: i64, ry2: i64, stops: &[(u32, u32)]) -> Radial {
        let rx = (rx2 * rx2) as u64;
        let ry = (ry2 * ry2) as u64;
        let mut lut = [0u8; 1025];
        for (i, v) in lut.iter_mut().enumerate() {
            // t in thousandths: sqrt(i / 1024).
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

/// The boot screen on one frame size: where the scene lies and its background gradients.
pub struct Scene {
    pub ox: i32,
    pub oy: i32,
    glow: Radial,
    vignette: Radial,
    disc: Radial,
    halo: Radial,
}

impl Scene {
    /// None when the frame is smaller than the scene.
    pub fn new(w: u32, h: u32) -> Option<Scene> {
        if w < SCENE_W || h < SCENE_H || !bootart::ready() {
            return None;
        }
        Some(Scene {
            ox: ((w - SCENE_W) / 2) as i32,
            oy: ((h - SCENE_H) / 2) as i32,
            // The big green glow (1200x1000 at 360,-60), accent 16 % -> 4 % at 55 % -> 0.
            glow: Radial::new(2560, 1173, 1600, 1333, &[(0, 160), (550, 40), (1000, 0)]),
            // Vignette: farthest-corner ellipse, clear to 45 %, rgba(4,8,6,.8) at the corners.
            vignette: Radial::new(2560, 1440, 3620, 2036, &[(450, 0), (1000, 800)]),
            // The ring's own fill (r 256), accent 25 % -> 3 % at 70 % -> 0.
            disc: Radial::new(2 * RING_CENTRE.0 as i64, 2 * RING_CENTRE.1 as i64, 683, 683, &[(0, 250), (700, 30), (1000, 0)]),
            // The title's glow (the design's text-shadow, at the cheap level a soft ellipse behind the word).
            halo: Radial::new(2560, 2 * (px(TITLE_TOP) + px(84) / 2) as i64, 640, 200, &[(0, 110), (1000, 0)]),
        })
    }

    /// The background at scene pixel (x, y).
    fn bg(&self, x: i32, y: i32) -> u32 {
        let mut c = SUNKEN;
        // Dot grid: a dot every 32 pixels (24 at 4/3), 1.33 px solid fading out at 1.73 px, at 70 %.
        let (gx, gy) = (x.rem_euclid(32), y.rem_euclid(32));
        if (14..=17).contains(&gx) && (14..=17).contains(&gy) {
            let (dx, dy) = (2 * gx + 1 - 32, 2 * gy + 1 - 32);
            match dx * dx + dy * dy {
                2 => c = crate::font::blend(LINE, c, 178),
                10 => c = crate::font::blend(LINE, c, 68),
                _ => {}
            }
        }
        let (x2, y2) = (2 * x as i64 + 1, 2 * y as i64 + 1);
        c = crate::font::blend(ACCENT, c, self.glow.at(x2, y2));
        c = crate::font::blend(VIGNETTE, c, self.vignette.at(x2, y2));
        c = crate::font::blend(ACCENT, c, self.disc.at(x2, y2));
        c = crate::font::blend(ACCENT, c, self.halo.at(x2, y2));
        // The board's scanlines (1 px of ink at 2.8 % every 3 px; at 4/3 one row in 4 at 1.33 px worth, 9/255),
        // baked into this own image: no run-time cost, never over a program's pixels.
        if y.rem_euclid(4) == 0 { crate::font::blend(INK, c, 9) } else { c }
    }

    /// A scene rectangle in frame coordinates.
    fn frame_rect(&self, x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rect::new((self.ox + x) as u32, (self.oy + y) as u32, w.max(0) as u32, h.max(0) as u32)
    }

    fn fill_bg(&self, f: &mut RamFrame, r: Rect) {
        let stride = f.width as usize;
        let px = f.pixels_mut();
        for y in r.y..r.y + r.h {
            let line = y as usize * stride;
            for x in r.x..r.x + r.w {
                if let Some(p) = px.get_mut(line + x as usize) {
                    *p = self.bg(x as i32 - self.ox, y as i32 - self.oy);
                }
            }
        }
    }

    /// The whole picture (host tests and ktests; the thread draws it in `bands`).
    pub fn draw_all(&self, f: &mut RamFrame, v: &View) {
        let full = f.full();
        self.draw_band(f, v, full);
    }

    /// The bands of `BAND_ROWS` rows that make up a `w`x`h` frame, top to bottom.
    pub fn bands(w: u32, h: u32) -> impl Iterator<Item = Rect> {
        (0..h).step_by(BAND_ROWS as usize).map(move |y| Rect::new(0, y, w, BAND_ROWS.min(h - y)))
    }

    /// The whole picture inside `band` (frame coordinates): the bands of a frame together give `draw_all`.
    pub fn draw_band(&self, f: &mut RamFrame, v: &View, band: Rect) {
        let band = cut(band, f.full());
        if band.is_empty() {
            return;
        }
        let all = cut(self.frame_rect(0, 0, SCENE_W as i32, SCENE_H as i32), band);
        if self.ox > 0 || self.oy > 0 {
            f.fill(band, SUNKEN);
        }
        self.fill_bg(f, all);
        bootart::ring(f, self.ox + RING_X, self.oy + RING_Y, all);
        let (tw, top) = (TITLE.w as i32, px(TITLE_TOP));
        let base = top + (px(84) - (TITLE.ascent - TITLE.descent)) / 2;
        bootart::word(f, &TITLE, self.ox + RING_CENTRE.0 - tw / 2, self.oy + base, INK, all);
        // Header: the live dot and the screen's name (the design's top-left label).
        self.dot(f, px(52), px(47), all);
        self.label(f, px(68), px(40), "BITSOS V2 · START", MUTED, false, all);
        self.label(f, px(1824), px(1020), "BELIEBIGE TASTE: TEXTKONSOLE", LINE_STRONG, true, all);
        self.header(f, v, band);
        for (i, r) in v.rows.iter().enumerate() {
            self.row(f, i, r, band);
        }
        self.status(f, v, band);
        self.progress(f, v, band);
        f.mark(band);
    }

    /// Only what changed from `old` to `new`.
    pub fn draw_changes(&self, f: &mut RamFrame, old: &View, new: &View) {
        let full = f.full();
        for (i, r) in new.rows.iter().enumerate() {
            if old.rows.get(i) != Some(r) {
                self.row(f, i, r, full);
            }
        }
        if (&old.status, old.status_warn) != (&new.status, new.status_warn) {
            self.status(f, new, full);
        }
        if old.progress != new.progress {
            self.progress(f, new, full);
        }
        if old.header != new.header {
            self.header(f, new, full);
        }
    }

    fn label(&self, f: &mut RamFrame, x: i32, top: i32, s: &str, color: u32, right: bool, clip: Rect) {
        let x = if right { x - LABEL.width(s, LABEL_SPACING) } else { x };
        let base = LABEL.baseline(top, px(14));
        bootart::text(f, &LABEL, self.ox + x, self.oy + base, s, color, LABEL_SPACING, clip);
    }

    fn header(&self, f: &mut RamFrame, v: &View, band: Rect) {
        let r = cut(self.frame_rect(px(1200), px(36), px(1872) - px(1200), px(22)), band);
        if r.is_empty() {
            return;
        }
        self.fill_bg(f, r);
        self.label(f, px(1872), px(40), &v.header, MUTED, true, r);
        f.mark(r);
    }

    fn row(&self, f: &mut RamFrame, i: usize, row: &Row, band: Rect) {
        let top = row_top(i);
        let r = cut(self.frame_rect(ROWS_LEFT, top, ROW_W, row_top(i + 1) - top), band);
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
        let waiting = row.state == State::Waiting;
        let (lc, dc) = match row.state {
            State::Waiting => (LINE_STRONG, LINE_STRONG),
            State::Failed => (MUTED, WARN),
            State::Skipped => (MUTED, MUTED),
            _ => (MUTED, INK),
        };
        let base = self.oy + MONO.baseline(top, px(20));
        let x = self.ox + ROWS_LEFT;
        bootart::text(f, &MONO, x, base, mark, mc, 0, r);
        bootart::text(f, &MONO, x + px(36), base, row.label, lc, 0, r);
        bootart::text(f, &MONO, x + px(156), base, &row.detail, dc, 0, r);
        if !waiting {
            let tx = x + ROW_W - MONO.width(&row.time, 0);
            bootart::text(f, &MONO, tx, base, &row.time, MUTED, 0, r);
        }
        f.mark(r);
    }

    fn status(&self, f: &mut RamFrame, v: &View, band: Rect) {
        let (top, line) = (px(STATUS_TOP), px(28));
        let r = cut(self.frame_rect(ROWS_LEFT, top, SCENE_W as i32 - 2 * ROWS_LEFT, line), band);
        if r.is_empty() {
            return;
        }
        self.fill_bg(f, r);
        let w = TEXT.width(&v.status, 0);
        let base = self.oy + TEXT.baseline(top, line);
        bootart::text(f, &TEXT, self.ox + RING_CENTRE.0 - w / 2, base, &v.status, if v.status_warn { WARN } else { MUTED }, 0, r);
        f.mark(r);
    }

    fn progress(&self, f: &mut RamFrame, v: &View, band: Rect) {
        // The board: 16 px below the list's top, label 14 px line, 10 px gap, the 6 px bar.
        let top = px(LIST_TOP + 16);
        let bar_top = px(LIST_TOP + 16 + 14 + 10);
        let r = cut(self.frame_rect(PROGRESS_LEFT - 8, top, PROGRESS_RIGHT - PROGRESS_LEFT + 16, bar_top + px(6) + 8 - top), band);
        if r.is_empty() {
            return;
        }
        self.fill_bg(f, r);
        self.label(f, PROGRESS_LEFT, top, "SCHRITTE", MUTED, false, r);
        let (done, all) = v.progress;
        let n = format_count(done, all);
        self.label(f, PROGRESS_RIGHT, top, n.as_str(), ACCENT, true, r);
        let w = PROGRESS_RIGHT - PROGRESS_LEFT;
        let (x, h) = (self.ox + PROGRESS_LEFT, px(6));
        let y = self.oy + bar_top;
        rrect(f, x, y, w, h, RAISED, 255, r);
        let fill = if all == 0 { 0 } else { w * done as i32 / all as i32 };
        if fill > 0 {
            // The design's glow under the bar (box-shadow 14 px), at the cheap level one faint wider bar.
            rrect(f, x - 3, y - 3, fill + 6, h + 6, ACCENT, 46, r);
            rrect(f, x, y, fill, h, ACCENT, 255, r);
        }
        f.mark(r);
    }

    /// The live dot of the header (static at the cheap level) with its soft glow.
    fn dot(&self, f: &mut RamFrame, cx: i32, cy: i32, clip: Rect) {
        let (cx16, cy16) = ((self.ox + cx) * 16, (self.oy + cy) * 16);
        disc(f, cx16, cy16, 12 * 16, 14 * 16, ACCENT, 60, clip);
        disc(f, cx16, cy16, 85, 16, ACCENT, 255, clip);
    }
}

/// "7 / 10" without allocating under the frame lock.
struct Count {
    buf: [u8; 24],
    len: usize,
}
impl Count {
    fn as_str(&self) -> &str {
        core::str::from_utf8(&self.buf[..self.len]).unwrap_or("")
    }
}
fn format_count(done: usize, all: usize) -> Count {
    use core::fmt::Write;
    struct W<'a>(&'a mut Count);
    impl core::fmt::Write for W<'_> {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            let b = s.as_bytes();
            let end = (self.0.len + b.len()).min(self.0.buf.len());
            self.0.buf[self.0.len..end].copy_from_slice(&b[..end - self.0.len]);
            self.0.len = end;
            Ok(())
        }
    }
    let mut c = Count { buf: [0; 24], len: 0 };
    let _ = write!(W(&mut c), "{done} / {all}");
    c
}

/// A disc of radius `r16` (1/16 pixels) with an edge `soft16` wide, `color` at up to `alpha`.
fn disc(f: &mut RamFrame, cx16: i32, cy16: i32, r16: i32, soft16: i32, color: u32, alpha: u32, clip: Rect) {
    let reach = (r16 + soft16) / 16 + 1;
    let stride = f.width as usize;
    let (cx, cy) = (cx16 / 16, cy16 / 16);
    let px = f.pixels_mut();
    for y in cy - reach..=cy + reach {
        for x in cx - reach..=cx + reach {
            if x < clip.x as i32 || y < clip.y as i32 || x >= (clip.x + clip.w) as i32 || y >= (clip.y + clip.h) as i32 {
                continue;
            }
            let (dx, dy) = ((x * 16 + 8 - cx16) as i64, (y * 16 + 8 - cy16) as i64);
            let d = (dx * dx + dy * dy).isqrt() as i32;
            let cov = ((r16 + soft16 / 2 - d) * 256 / soft16.max(1)).clamp(0, 256) as u32;
            let a = (cov * alpha / 256) as u8;
            if let Some(p) = px.get_mut(y as usize * stride + x as usize) {
                *p = crate::font::blend(color, *p, a);
            }
        }
    }
}

/// A rounded bar (radius h/2) at up to `alpha`.
fn rrect(f: &mut RamFrame, x: i32, y: i32, w: i32, h: i32, color: u32, alpha: u32, clip: Rect) {
    let r16 = h * 8;
    let stride = f.width as usize;
    let px = f.pixels_mut();
    for yy in y..y + h {
        for xx in x..x + w {
            if xx < clip.x as i32 || yy < clip.y as i32 || xx >= (clip.x + clip.w) as i32 || yy >= (clip.y + clip.h) as i32 {
                continue;
            }
            // Distance to the bar's centre line, ends rounded.
            let (px16, py16) = (xx * 16 + 8, yy * 16 + 8);
            let lx = px16.clamp(x * 16 + r16, ((x + w) * 16 - r16).max(x * 16 + r16));
            let (dx, dy) = ((px16 - lx) as i64, (py16 - (y * 16 + r16)) as i64);
            let d = (dx * dx + dy * dy).isqrt() as i32;
            let cov = ((r16 + 8 - d) * 16).clamp(0, 256) as u32;
            let a = (cov * alpha / 256) as u8;
            if let Some(p) = px.get_mut(yy as usize * stride + xx as usize) {
                *p = crate::font::blend(color, *p, a);
            }
        }
    }
}
