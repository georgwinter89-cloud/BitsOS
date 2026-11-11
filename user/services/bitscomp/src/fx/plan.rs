//! WY-19: which effects a frame shows, in the element order of `render::build_elements` (topmost first). Every function
//! here returns nothing at the cheap level, so the level costs that one test. All sizes are the design's board px
//! (Main.dc.html) on the physical grid, as the cached images draw them.

use std::time::Duration;

use super::trans::{self, Look, Play};
use super::{Demo, FxElement, Key, Kind, motion};
use crate::chrome_render::buehne::Buehne;
use crate::header::px_rect;
use crate::layout::{Place, Placement, Rect};
use crate::state::App;
use crate::theme::{Motion, Rgba, Shadow, Theme, alpha};
use crate::win::Window;

/// CSS `backdrop-filter: blur(16px) saturate(1.25)` of `.glass`.
const GLASS_BLUR: f32 = 16.0;
const GLASS_SATURATE: f32 = 1.25;
/// At `theme.motion` calm the moving effects that stay step this often (no beam, no radar).
const CALM_STEP: Duration = Duration::from_millis(66);

fn arr(r: Rect) -> [i32; 4] {
    [r.x, r.y, r.w, r.h]
}

/// The straight colour of an opaque premultiplied one.
fn rgb(c: Rgba) -> [f32; 3] {
    let a = c[3].max(1e-6);
    [c[0] / a, c[1] / a, c[2] / a]
}

/// The box shadow layer of a panel `b` (px, radius `r`): (dy, blur, spread) in px.
fn shadow(app: &mut App, key: Key, b: Rect, r: f32, (dy, blur, spread): (f32, f32, f32), color: Rgba, out: &mut Vec<FxElement>) {
    let sigma = (blur / 2.0).max(0.5);
    let reach = (spread + 3.0 * sigma).ceil() as i32 + 1;
    if reach <= 0 {
        return;
    }
    let dy_px = dy.round() as i32;
    let rect = [b.x - reach, b.y + dy_px - reach, b.w + 2 * reach, b.h + 2 * reach];
    let bx = [reach as f32 - spread, reach as f32 - spread, b.w as f32 + 2.0 * spread, b.h as f32 + 2.0 * spread];
    let hole = [reach as f32, (reach - dy_px) as f32, b.w as f32, b.h as f32];
    let radius = (r + spread).max(0.0);
    out.push(app.fx.element(key, rect, Kind::Shadow { bx, radius, sigma, color, hole, hole_radius: r }));
}

/// The two layers of a shadow token at the GPU level, with the soft accent glow if `glow`.
fn shadows(app: &mut App, name: &'static str, id: u64, b: Rect, r: f32, s: &Shadow, glow: bool, out: &mut Vec<FxElement>) {
    let t = app.theme.clone();
    let k = t.board as f32;
    for (i, l) in s.0.iter().enumerate() {
        let key = (name, id * 8 + i as u64);
        shadow(app, key, b, r, (l.dy * k, l.blur * k, l.spread * k), l.color, out);
    }
    if glow {
        // `hud::glow_soft`'s strength: the design's 22 % at theme.glow's default.
        let a = t.glow[3] / if t.dark { 0.18 } else { 0.14 } * 0.22;
        shadow(app, (name, id * 8 + 2), b, r, (0.0, 40.0 * k, 0.0), alpha(t.accent, a.min(0.5)), out);
    }
}

/// A glass panel at `rect` (logical): the blur under its image, its shadows under that.
pub fn panel(app: &mut App, name: &'static str, rect: Rect, radius: f32, s: &Shadow, glow: bool, out: &mut Vec<FxElement>) {
    if !app.theme.effects_gpu {
        return;
    }
    let t = app.theme.clone();
    let b = px_rect(rect, t.scale);
    let sigma = GLASS_BLUR * t.board as f32;
    out.push(app.fx.element((name, 0), arr(b), Kind::Glass { radius, sigma, saturate: GLASS_SATURATE }));
    shadows(app, name, 1, b, radius, s, glow, out);
}

/// The stage under the tiles (`wallpaper::elements`).
pub fn stage(app: &mut App, b: &Buehne) -> Vec<FxElement> {
    let mut out = Vec::new();
    let t = app.theme.clone();
    panel(app, "stage", b.stage, t.radius_card as f32 * t.board as f32, &t.shadow_lifted, true, &mut out);
    out
}

/// The right column's two frames.
pub fn column(app: &mut App, map: Rect, next: Rect) -> Vec<FxElement> {
    let mut out = Vec::new();
    let t = app.theme.clone();
    let r = t.radius_card as f32 * t.board as f32;
    panel(app, "map", map, r, &t.shadow_raised, false, &mut out);
    panel(app, "next", next, r, &t.shadow_raised, false, &mut out);
    out
}

/// The time strip.
pub fn strip(app: &mut App, r: Rect) -> Vec<FxElement> {
    let mut out = Vec::new();
    let t = app.theme.clone();
    panel(app, "strip", r, t.radius_card as f32 * t.board as f32, &t.shadow_raised, false, &mut out);
    out
}

/// The Feld pill.
pub fn feld(app: &mut App, r: Rect) -> Vec<FxElement> {
    let mut out = Vec::new();
    let t = app.theme.clone();
    let h = px_rect(r, t.scale).h as f32 / 2.0;
    panel(app, "feld", r, h, &t.shadow_lifted, true, &mut out);
    out
}

/// A classic bar: glass with square corners (a revealed one over live windows too).
pub fn bar(app: &mut App, name: &'static str, r: Rect) -> Option<FxElement> {
    if !app.theme.effects_gpu {
        return None;
    }
    let t = app.theme.clone();
    let kind = Kind::Glass { radius: 0.0, sigma: GLASS_BLUR * t.board as f32, saturate: GLASS_SATURATE };
    Some(app.fx.element((name, 0), arr(px_rect(r, t.scale)), kind))
}

/// The soft shadow under a window's content (over the windows below it): lifted when focused or floating.
pub fn window(app: &mut App, p: &Placement<Window>) -> Vec<FxElement> {
    let mut out = Vec::new();
    if !app.theme.effects_gpu || p.fullscreen {
        return out;
    }
    let t = app.theme.clone();
    let id = p.win.surface().map_or(0, |s| {
        use smithay::reexports::wayland_server::Resource;
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        s.id().hash(&mut h);
        h.finish() >> 4
    });
    let lifted = p.focused || matches!(p.place, Place::Floating { .. });
    let s = if lifted { t.shadow_lifted } else { t.shadow_raised };
    let b = px_rect(p.frame, t.scale);
    let r = t.radius as f32 * t.scale as f32;
    shadows(app, "window", id, b, r, &s, false, &mut out);
    out
}

/// A dot around (`cx`, `cy`).
fn dot(app: &mut App, key: Key, (cx, cy): (f32, f32), r: f32, stroke: f32, color: Rgba, glow: f32) -> FxElement {
    let reach = (r + stroke + 3.0 * glow).ceil() as i32 + 2;
    let rect = [cx.round() as i32 - reach, cy.round() as i32 - reach, 2 * reach, 2 * reach];
    app.fx.element(key, rect, Kind::Dot { r, stroke, color, glow })
}

/// The mark's core in the header (`.breathe`): the header image leaves it out at this level.
pub fn mark(app: &mut App, b: &Buehne, dy: f32) -> Option<FxElement> {
    if !app.theme.effects_gpu {
        return None;
    }
    let t = app.theme.clone();
    let k = t.board as f32;
    let m = px_rect(b.mark, t.scale);
    let at = (m.x as f32 + 13.0 * k, m.y as f32 + m.h as f32 / 2.0 + dy);
    let now = app.fx_now();
    let (s, a) = if t.motion == Motion::Off { (1.0, 0.95) } else { motion::breathe(now) };
    Some(dot(app, ("mark", 0), at, 3.6 * k * s, 0.0, alpha(t.accent, a), 0.0))
}

/// Claude's small ring in the Feld pill (`.breathe`).
pub fn feld_ring(app: &mut App, r: Rect) -> Option<FxElement> {
    if !app.theme.effects_gpu {
        return None;
    }
    let t = app.theme.clone();
    let k = t.board as f32;
    let b = px_rect(r, t.scale);
    let at = (b.x as f32 + 35.0 * k, b.y as f32 + b.h as f32 / 2.0);
    let now = app.fx_now();
    let (s, a) = if t.motion == Motion::Off { (1.0, 0.95) } else { motion::breathe(now) };
    Some(dot(app, ("feld-ring", 0), at, 9.0 * k * s, 2.4 * k, alpha(t.accent, a), 0.0))
}

/// The dot before the headline's label (`.pulse`).
pub fn headline_dot(app: &mut App, r: Rect) -> Option<FxElement> {
    if !app.theme.effects_gpu {
        return None;
    }
    let t = app.theme.clone();
    let k = t.board as f32;
    let b = px_rect(r, t.scale);
    let now = app.fx_now();
    let a = if t.motion == Motion::Off { 1.0 } else { motion::pulse(now) };
    Some(dot(app, ("headline-dot", 0), (b.x as f32 + 4.0 * k, b.y as f32 + 8.0 * k), 4.0 * k, 0.0, alpha(t.accent, a), 0.0))
}

/// The headline's text glow (Main.dc.html's `text-shadow`): the halo of the image pushed just before it.
pub fn halo(app: &mut App, name: &'static str, r: Rect) -> Option<FxElement> {
    if !app.theme.effects_gpu {
        return None;
    }
    let t = app.theme.clone();
    let sigma = 18.0 * t.board as f32;
    let b = px_rect(r, t.scale);
    let reach = (3.0 * sigma).ceil() as i32;
    let rect = [b.x - reach, b.y - reach, b.w + 2 * reach, b.h + 2 * reach];
    let gain = (t.glow[3] * 2.5).min(1.0);
    Some(app.fx.element((name, 0), rect, Kind::Halo { sigma, gain }))
}

/// Claude's ring: its dash turns on the GPU; the sprite under it keeps the outer ring and the core.
pub fn ring(app: &mut App, name: &'static str, rect: Rect, size: f32, ring: crate::desk::Ring, dy: f32) -> Option<FxElement> {
    if !app.theme.effects_gpu {
        return None;
    }
    let t = app.theme.clone();
    let b = px_rect(Rect { y: rect.y + dy.round() as i32, ..rect }, t.scale);
    let k = size * t.scale as f32 / 48.0;
    let (on, off) = crate::ring::dash(ring);
    let color = if ring == crate::desk::Ring::Waiting { t.warm } else { t.claude };
    let now = app.fx_now();
    let phase = match motion::ring_turn(ring).filter(|_| t.motion != Motion::Off) {
        Some(secs) => motion::frac(now, secs),
        None => 0.0,
    };
    let g = t.glow[3];
    let kind = Kind::Ring { r: 17.0 * k, width: 3.0 * k, on: on * k, off: off * k, phase, color, glow: 2.5 * k * (g / 0.18).min(2.0) };
    Some(app.fx.element((name, 0), arr(b), kind))
}

/// The map's disc and the radar's sweep (Main.dc.html's "Übersicht": 232 px at 100, 66).
pub fn radar(app: &mut App, map: Rect) -> Option<FxElement> {
    if !app.theme.effects_gpu {
        return None;
    }
    let t = app.theme.clone();
    let k = t.board as f32;
    let m = px_rect(map, t.scale);
    let size = (232.0 * k).round() as i32;
    let rect = [m.x + (100.0 * k).round() as i32, m.y + (66.0 * k).round() as i32, size, size];
    let now = app.fx_now();
    let angle = if t.motion == Motion::Medium { motion::radar_angle(now) } else { 0.0 };
    let accent = rgb(t.accent);
    let ground = if t.dark { [0.0282, 0.0424, 0.0353, 0.6] } else { alpha(t.panel, 0.6) };
    Some(app.fx.element(("radar", 0), rect, Kind::Radar { angle, accent, ground }))
}

/// A thread as flowing dashes: `points` (px) as one path, the dash phase going on from stretch to stretch.
pub fn thread(app: &mut App, name: &'static str, points: &[[f32; 2]], out: &mut Vec<FxElement>) {
    let t = app.theme.clone();
    let k = t.board as f32;
    let now = app.fx_now();
    let flowed = if t.motion == Motion::Medium { motion::flow_offset(now) } else { 0.0 };
    let width = 1.6 * k;
    let reach = (width + 8.0 * k).ceil();
    let mut along = 0.0;
    for (i, w) in points.windows(2).enumerate() {
        let (a, b) = (w[0], w[1]);
        let x0 = a[0].min(b[0]) - reach;
        let y0 = a[1].min(b[1]) - reach;
        let rect = [x0.floor() as i32, y0.floor() as i32, ((a[0] - b[0]).abs() + 2.0 * reach).ceil() as i32 + 1, ((a[1] - b[1]).abs() + 2.0 * reach).ceil() as i32 + 1];
        let origin = [rect[0] as f32, rect[1] as f32];
        let kind = Kind::Flow {
            a: [a[0] - origin[0], a[1] - origin[1]],
            b: [b[0] - origin[0], b[1] - origin[1]],
            width,
            on: 4.0 * k,
            off: 8.0 * k,
            offset: along - flowed * k,
            color: t.accent,
            glow: 2.0 * k,
        };
        out.push(app.fx.element((name, i as u64), rect, kind));
        along += ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2)).sqrt();
    }
}

/// A cubic Bézier flattened for `thread`, end included, start left to the path before it.
pub fn bezier_points(p0: [f32; 2], p1: [f32; 2], p2: [f32; 2], p3: [f32; 2], out: &mut Vec<[f32; 2]>) {
    const STEPS: usize = 16;
    for i in 1..=STEPS {
        let s = i as f32 / STEPS as f32;
        let r = 1.0 - s;
        let at = |n: usize| r * r * r * p0[n] + 3.0 * r * r * s * p1[n] + 3.0 * r * s * s * p2[n] + s * s * s * p3[n];
        out.push([at(0), at(1)]);
    }
}

/// The layers over everything but the installer's overlay and the pointer (Main.dc.html's last elements): the
/// transition, the title's reveal, the scanlines and the beam. Also the ambient clock: this runs in every frame.
/// The scanlines and the beam belong to the green ground (Georg, 5.11.): every bar and window lies over them.
pub fn ground(app: &mut App, full_screen: bool) -> Vec<FxElement> {
    let mut out = Vec::new();
    let t = app.theme.clone();
    if !t.effects_gpu || full_screen || app.boot_shown() {
        return out;
    }
    let (w, h) = crate::chrome_render::wallpaper::output_px(app);
    let k = t.board as f32;
    if t.dark && t.scanlines {
        let p = (3.0 * k).round().max(2.0);
        let color = alpha(t.text, 0.028 * p / 3.0);
        out.push(app.fx.element(("scan", 0), [0, 0, w, h], Kind::Scan { period: p, color }));
    }
    if t.motion == Motion::Medium {
        let tm = app.fx_now();
        let top = motion::beam_top(tm, h as f32, k).round() as i32;
        let (mid, peak) = if t.dark { (0.045, 0.12) } else { (0.035, 0.08) };
        let kind = Kind::Beam { color: rgb(t.accent), peak, mid };
        out.push(app.fx.element(("beam", 0), [0, top, w, (140.0 * k).round() as i32], kind));
    }
    out
}

pub fn top(app: &mut App, full_screen: bool) -> Vec<FxElement> {
    let mut out = Vec::new();
    let t = app.theme.clone();
    if !t.effects_gpu {
        app.fx.still();
        app.fx.playing = None;
        app.fx.title = None;
        return out;
    }
    let now = crate::pacer::now();
    let (w, h) = crate::chrome_render::wallpaper::output_px(app);
    transition(app, now, (w, h), &mut out);
    title(app, now, (w, h), &mut out);
    let booting = app.boot_shown();
    if !booting && !full_screen {
        for d in app.fx.demo.clone() {
            demo(app, d, &mut out);
        }
    }
    let playing = app.fx.playing.is_some() || app.fx.title.is_some();
    let held = app.fx.playing.is_some_and(|p| p.hold.is_some()) || app.fx.title.as_ref().is_some_and(|p| p.hold.is_some());
    let ambient = !booting && !full_screen && t.motion != Motion::Off;
    if held || app.fx.frozen.is_some() && !playing {
        app.fx.still();
    } else if playing {
        app.fx.shown(app.frame_time, app.refresh);
    } else if ambient {
        // From the frame's own time, not from this moment in its draw: a step past the next grid point adds up.
        let step = if t.motion == Motion::Medium { app.refresh } else { CALM_STEP };
        app.fx.shown(app.frame_time, step);
    } else {
        app.fx.still();
    }
    out
}

fn transition(app: &mut App, now: Duration, (w, h): (i32, i32), out: &mut Vec<FxElement>) {
    let Some(p) = app.fx.playing else { return };
    let Some(k) = p.progress(now) else {
        // Its last frame: the next one shows the new scene alone (the tracker damages the element's old place).
        app.fx.playing = None;
        app.needs_frame = true;
        return;
    };
    let t = app.theme.clone();
    let (rect, look): ([i32; 4], Look) = trans::look(p.what, k, (w, h), t.board as f32, t.bg, t.radius_card as f32 * t.board as f32);
    out.push(app.fx.element(("transition", 0), rect, Kind::Snapshot { serial: p.serial, look }));
}

fn title(app: &mut App, now: Duration, (w, h): (i32, i32), out: &mut Vec<FxElement>) {
    let Some(tl) = app.fx.title.clone() else { return };
    let Some(k) = tl.progress(now) else {
        app.fx.title = None;
        app.needs_frame = true;
        return;
    };
    let board = app.theme.board as f32;
    let (sigma, a, stretch) = trans::title_look(k, board);
    let dw = (tl.image.w as f32 * stretch).round() as i32;
    let rect = [(w - dw) / 2, (h - tl.image.h) / 2, dw, tl.image.h];
    out.push(app.fx.element(("title", 0), rect, Kind::Reveal { image: tl.image.clone(), sigma, alpha: a }));
}

fn demo(app: &mut App, d: Demo, out: &mut Vec<FxElement>) {
    let t = app.theme.clone();
    let tm = app.fx_now();
    let moving = t.motion != Motion::Off;
    let centre = |r: [i32; 4]| ((r[0] + r[2] / 2) as f32, (r[1] + r[3] / 2) as f32);
    match d {
        Demo::Pulse(r) => {
            let a = if moving { motion::pulse(tm) } else { 1.0 };
            out.push(dot(app, ("demo-pulse", 0), centre(r), r[2] as f32 / 2.0, 0.0, alpha(t.accent, a), r[2] as f32 / 4.0));
        }
        Demo::Ping(r) => {
            let (s, a) = if moving { motion::ping(tm) } else { (1.0, 0.9) };
            out.push(dot(app, ("demo-ping", 0), centre(r), r[2] as f32 / 2.0 * s, 2.0, alpha(t.accent, a), 0.0));
        }
        Demo::Flow(r) => {
            // Main.dc.html's first thread, its shape stretched into `r`: out to the right, a curve, a long way back.
            let (x, y, w, h) = (r[0] as f32, r[1] as f32, r[2] as f32, r[3] as f32);
            let mut pts = vec![[x + w, y + h]];
            bezier_points([x + w, y + h], [x + w * 0.96, y + h], [x + w * 0.96, y], [x + w * 0.92, y], &mut pts);
            pts.push([x + w * 0.05, y]);
            bezier_points([x + w * 0.05, y], [x + w * 0.02, y], [x, y + 0.03 * h], [x, y + 0.08 * h], &mut pts);
            thread(app, "demo-flow", &pts, out);
        }
    }
}

impl App {
    /// The ambient clocks' seconds now (`ctl fx freeze` stands them still).
    pub fn fx_now(&mut self) -> f64 {
        self.fx.time(crate::pacer::now())
    }
}

/// The arrival on another picture (a workspace, the desktop after the boot scene).
pub const WIPE: Play = Play::Wipe;

/// Whether the level is drawn.
pub fn drawn(t: &Theme) -> bool {
    t.effects_gpu
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_flattened_curve_ends_on_its_end_point() {
        let mut v = Vec::new();
        bezier_points([0.0, 0.0], [10.0, 0.0], [10.0, 10.0], [20.0, 10.0], &mut v);
        assert_eq!(v.last(), Some(&[20.0, 10.0]));
        assert!(v.len() >= 8);
    }
}
