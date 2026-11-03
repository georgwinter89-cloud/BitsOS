//! DS-3: the Bühne's bottom: the time strip "Verlauf · Heute" (hour ticks, one lane per workspace, the now marker;
//! empty until the snapshots fill it) and the row under it (the hint line from real counts, the Feld bar as a
//! look, minimised windows and Startseite). The now marker moves only when the status clock's minute changes, so
//! the strip is redrawn at most once a minute and a still desktop draws 0 frames.

use crate::canvas::{Box2, Canvas};
use crate::chrome::Part as C;
use crate::chrome_draw::Px;
use crate::chrome_draw::buehne::{Hover, k};
use crate::deco::hud::{self, Glass};
use crate::layout::Rect;
use crate::text::{Family, Weight};
use crate::theme::{Style, Theme, alpha};

/// The hours the strip spans and where "now" is on it: 12 hours with now in the middle, inside the day.
pub fn window(hour: i32, minute: i32) -> (i32, f32) {
    let start = (hour.clamp(0, 23) - 6).clamp(0, 12);
    (start, (hour.clamp(0, 23) - start) as f32 + minute.clamp(0, 59) as f32 / 60.0)
}

/// (hour, minute) from the status clock ("Mi 28.10. · 14:32[ UTC]").
pub fn hour_minute(clock: &str) -> Option<(i32, i32)> {
    let time = clock.rsplit(" · ").next()?.split(' ').next()?;
    let (h, m) = time.split_once(':')?;
    Some((h.trim().parse().ok()?, m.trim().parse().ok()?))
}

#[derive(Clone, Debug, PartialEq)]
pub struct StripLook {
    pub rect: Rect,
    pub theme: u64,
    /// One lane per workspace: its name and context colour (0..3); the shown one is stronger.
    pub lanes: Vec<(String, usize)>,
    pub active: usize,
    /// From the status clock: changes once a minute.
    pub now: Option<(i32, i32)>,
}

pub fn strip(l: &StripLook, t: &Theme, area: Rect) -> Canvas {
    let kk = k(t);
    let p = Px::new(area, t.scale);
    let (w, h) = p.size();
    let mut c = Canvas::new(w, h);
    let b = p.b(l.rect);
    let r = t.radius_card as f32 * kk;
    hud::shadow(&mut c, b, r, &t.shadow_raised, t);
    c.rrect(b, r, hud::glass(t, Glass::Bar));
    c.rrect_line(b, r, kk, alpha(t.accent, 0.18));
    hud::brackets(&mut c, b, t);
    let s = p.r(l.rect);
    let at = |x: f32, y: f32| (s.x as f32 + x * kk, s.y as f32 + y * kk);
    // The label column: "VERLAUF", "Heute", and "Zurückspulen" (shown as not available until the snapshots exist).
    let lab = t.style(Style::Label);
    let (lx, _) = at(24.0, 0.0);
    let lx = lx as i32;
    c.text_fit(&lab, Rect { x: lx, y: s.y + (16.0 * kk) as i32, w: (150.0 * kk) as i32, h: (14.0 * kk) as i32 }, &Theme::label(Style::Label, "Verlauf"), t.muted);
    let day = t.bfont(Family::Display, 18.0, Weight::SemiBold);
    c.text_fit(&day, Rect { x: lx, y: s.y + (34.0 * kk) as i32, w: (150.0 * kk) as i32, h: (22.0 * kk) as i32 }, "Heute", t.text);
    let bf = t.bfont(Family::Ui, 13.0, Weight::SemiBold);
    let bw = bf.width("Zurückspulen") + (52.0 * kk) as i32;
    let bb = Box2 { x: lx as f32, y: s.y as f32 + 64.0 * kk, w: bw as f32, h: 44.0 * kk };
    if bb.y + bb.h <= b.y + b.h {
        c.rrect_line(bb, 14.0 * kk, kk, t.line);
        let (ix, iy) = (bb.x + 14.0 * kk, bb.y + bb.h / 2.0);
        for dx in [0.0, 5.5] {
            let x0 = ix + dx * kk;
            c.poly(&[(x0 + 5.5 * kk, iy - 4.5 * kk), (x0 + 5.5 * kk, iy + 4.5 * kk), (x0, iy)], t.muted);
        }
        let tx = (bb.x + 38.0 * kk) as i32;
        c.text_fit(&bf, Rect { x: tx, y: bb.y as i32, w: bw - (38.0 * kk) as i32, h: bb.h as i32 }, "Zurückspulen", t.muted);
    }
    // The hours: 13 ticks over 12 hours, labels above.
    let (ax, ay) = at(200.0, 34.0);
    let aw = (s.w as f32 - 264.0 * kk).max(12.0);
    let ah = 80.0 * kk;
    let step = aw / 12.0;
    let (hour, minute) = l.now.unwrap_or((12, 0));
    let (start, now) = window(hour, minute);
    let mono = t.bfont(Family::Mono, 11.0, Weight::Regular);
    for i in 0..=12 {
        let x = ax + i as f32 * step;
        c.fill(x.round() as i32, ay as i32, kk.round().max(1.0) as i32, ah as i32, t.line);
        let label = format!("{:02}:00", start + i);
        let band = Rect { x: (x - 20.0 * kk) as i32, y: s.y + (12.0 * kk) as i32, w: (40.0 * kk) as i32, h: (14.0 * kk) as i32 };
        c.text_mid(&mono, band, &label, t.muted);
    }
    // One lane per workspace in its colour (empty: nothing recorded yet), as many as fit at 3 px or more.
    let (top, room, gap) = (s.y as f32 + 42.0 * kk, 62.0 * kk, 4.0 * kk);
    let n = l.lanes.len().max(1) as f32;
    let lane_h = ((room - (n - 1.0) * gap) / n).min(30.0 * kk);
    let (lane_h, fit) = if lane_h >= 3.0 * kk { (lane_h, l.lanes.len()) } else { (3.0 * kk, ((room + gap) / (3.0 * kk + gap)) as usize) };
    let name_font = t.bfont(Family::Ui, 12.0, Weight::SemiBold);
    for (i, (name, col)) in l.lanes.iter().enumerate().take(fit) {
        let y = top + i as f32 * (lane_h + gap);
        let colour = t.kontext[col % 3];
        let lane = Box2 { x: ax, y, w: aw, h: lane_h };
        c.rrect(lane, (8.0 * kk).min(lane_h / 2.0), alpha(colour, if i == l.active { 0.2 } else { 0.1 }));
        c.disc(ax - 12.0 * kk, y + lane_h / 2.0, (4.0 * kk).min(lane_h / 2.0 + kk), colour);
        // The lane says whose it is once the board's 12 px text fits (no recorded blocks yet, so the whole lane is a
        // faint tint, not the board's solid blocks).
        if lane_h >= 14.0 * kk {
            let band = Rect { x: (ax + 10.0 * kk) as i32, y: y as i32, w: (aw - 20.0 * kk) as i32, h: lane_h as i32 };
            c.text_fit(&name_font, band, name, if i == l.active { t.text } else { t.muted });
        }
    }
    // The future: faint diagonal hatching right of now.
    let nx = ax + now * step;
    hatch(&mut c, Box2 { x: nx, y: ay, w: ax + aw - nx, h: ah }, 12.0 * kk, kk, alpha(t.text, 0.035));
    // The now marker: a line, the time on a pill above it, a ring at its foot.
    c.line((nx, s.y as f32 + 30.0 * kk), (nx, s.y as f32 + 116.0 * kk), 2.0 * kk, t.accent);
    let pill = Box2 { x: nx - 26.0 * kk, y: s.y as f32 + 6.0 * kk, w: 52.0 * kk, h: 22.0 * kk };
    c.rrect(pill, pill.h / 2.0, t.accent);
    let pf = t.bfont(Family::Mono, 11.0, Weight::SemiBold);
    let pr = Rect { x: pill.x as i32, y: pill.y as i32, w: pill.w as i32, h: pill.h as i32 };
    c.text_mid(&pf, pr, &format!("{hour:02}:{minute:02}"), t.on_accent);
    let (fx, fy) = (nx, s.y as f32 + 115.0 * kk);
    c.disc(fx, fy, 7.0 * kk, t.bg);
    c.ring(fx, fy, 6.0 * kk, 2.0 * kk, t.accent);
    c
}

/// Diagonal stripes (135 degrees) every `every` px inside `b`.
fn hatch(c: &mut Canvas, b: Box2, every: f32, width: f32, col: crate::theme::Rgba) {
    if b.w <= 0.0 || b.h <= 0.0 {
        return;
    }
    let mut d = -b.h;
    while d < b.w {
        // The stripe x = b.x + d + (b.h - (y - b.y)), clipped to the box's columns.
        let (mut x0, mut y0, mut x1, mut y1) = (b.x + d, b.y + b.h, b.x + d + b.h, b.y);
        if x0 < b.x {
            y0 -= b.x - x0;
            x0 = b.x;
        }
        if x1 > b.x + b.w {
            y1 += x1 - (b.x + b.w);
            x1 = b.x + b.w;
        }
        if x1 > x0 {
            c.line((x0, y0), (x1, y1), width, col);
        }
        d += every;
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct BottomLook {
    pub row: Rect,
    pub hint: Rect,
    pub feld: Rect,
    pub home: Option<Rect>,
    pub min_items: Vec<Rect>,
    pub theme: u64,
    pub text: String,
    /// MC-9b: "Claude darf: installieren bis 08:00" while a grant runs.
    pub grant: Option<String>,
    pub minimized: Vec<String>,
    pub home_lit: bool,
    pub hover: Option<Hover>,
}

pub fn bottom(l: &BottomLook, t: &Theme, area: Rect) -> Canvas {
    let kk = k(t);
    let p = Px::new(area, t.scale);
    let (w, h) = p.size();
    let mut c = Canvas::new(w, h);
    // The hint: "BEREIT", the counts, the grant while one runs.
    let hr = p.r(l.hint);
    let lab = t.style(Style::Label);
    let y = hr.y + (6.0 * kk) as i32;
    c.text_fit(&lab, Rect { y, h: (14.0 * kk) as i32, ..hr }, &Theme::label(Style::Label, "Bereit"), t.accent);
    let f = t.bfont(Family::Ui, 13.0, Weight::Regular);
    let y = y + (18.0 * kk) as i32;
    c.text_fit(&f, Rect { y, h: (18.0 * kk) as i32, ..hr }, &l.text, t.muted);
    if let Some(g) = &l.grant {
        c.text_fit(&f, Rect { y: y + (18.0 * kk) as i32, h: (18.0 * kk) as i32, ..hr }, g, t.warm);
    }
    feld(&mut c, p.b(l.feld), t, l.hover == Some(Hover::Buehne(crate::chrome_render::buehne::Part::Feld)));
    let mono = t.bfont(Family::Mono, 12.0, Weight::Medium);
    for (i, (r, name)) in l.min_items.iter().zip(&l.minimized).enumerate() {
        let b = p.b(*r);
        let on = l.hover == Some(Hover::Chrome(C::Min(i)));
        c.rrect(b, b.h / 2.0, alpha(t.panel, 0.6));
        c.rrect_line(b, b.h / 2.0, kk, if on { t.line_strong } else { t.line });
        let pr = p.r(*r);
        let band = Rect { x: pr.x + (12.0 * kk) as i32, w: pr.w - (24.0 * kk) as i32, ..pr };
        c.text_fit(&mono, band, name, if on { t.text } else { t.muted });
    }
    if let Some(r) = l.home {
        let b = p.b(r);
        let on = l.hover == Some(Hover::Chrome(C::Home));
        if on {
            c.rrect(b, 14.0 * kk, t.chip);
        }
        c.rrect_line(b, 14.0 * kk, kk, if l.home_lit { t.accent } else { t.line_strong });
        let bf = t.bfont(Family::Ui, 13.0, Weight::SemiBold);
        c.text_mid(&bf, p.r(r), "Startseite", t.text);
    }
    c
}

/// The Feld bar as the design shows it (opening it comes with the Feld): a glass pill with Claude's small ring, a
/// caret, the invitation and the key that will open it (Win+Leertaste, Georg's answer 6). No microphone yet.
fn feld(c: &mut Canvas, b: Box2, t: &Theme, hover: bool) {
    let kk = k(t);
    let r = b.h / 2.0;
    hud::shadow(c, b, r, &t.shadow_lifted, t);
    hud::glow_soft(c, b, r, t);
    c.rrect(b, r, alpha(t.panel, 0.78));
    c.rrect_line(Box2 { x: b.x - kk, y: b.y - kk, w: b.w + 2.0 * kk, h: b.h + 2.0 * kk }, r + kk, kk, alpha(t.accent, if hover { 0.7 } else { 0.45 }));
    let cy = b.y + b.h / 2.0;
    let ix = b.x + 22.0 * kk + 13.0 * kk;
    c.ring(ix, cy, 9.0 * kk, 2.4 * kk, t.accent);
    c.disc(ix, cy, 3.0 * kk, t.accent);
    let cx = ix + 13.0 * kk + 14.0 * kk;
    c.fill(cx as i32, (cy - 11.0 * kk) as i32, (2.0 * kk).round() as i32, (22.0 * kk) as i32, t.accent);
    let keys = ["Win", "Leertaste"];
    let kf = t.bfont(Family::Mono, 11.0, Weight::Regular);
    let mut kx = b.x + b.w - 22.0 * kk;
    let mut caps = Vec::new();
    for k in keys.iter().rev() {
        let kw = kf.width(k) as f32 + 12.0 * kk;
        kx -= kw;
        caps.push((Box2 { x: kx, y: cy - 10.0 * kk, w: kw, h: 20.0 * kk }, *k));
        kx -= 4.0 * kk;
    }
    for (kb, k) in &caps {
        c.rrect_line(*kb, 6.0 * kk, kk, t.line);
        c.text_mid(&kf, Rect { x: kb.x as i32, y: kb.y as i32, w: kb.w as i32, h: kb.h as i32 }, k, t.muted);
    }
    let f = t.bfont(Family::Ui, 17.0, Weight::Regular);
    let tx = (cx + 10.0 * kk) as i32;
    let band = Rect { x: tx, y: b.y as i32, w: (kx - 14.0 * kk) as i32 - tx, h: b.h as i32 };
    c.text_fit(&f, band, "Sag, was du vorhast, oder gib eine Adresse ein", t.muted);
}

/// The stage under the tiles: the lifted shadow, the soft accent glow, glass, an accent edge at 18 %, brackets.
pub fn stage(stage: Rect, t: &Theme, area: Rect) -> Canvas {
    let kk = k(t);
    let p = Px::new(area, t.scale);
    let (w, h) = p.size();
    let mut c = Canvas::new(w, h);
    let b = p.b(stage);
    let r = t.radius_card as f32 * kk;
    hud::shadow(&mut c, b, r, &t.shadow_lifted, t);
    hud::glow_soft(&mut c, b, r, t);
    c.rrect(b, r, hud::glass(t, Glass::Card));
    c.rrect_line(b, r, kk, alpha(t.accent, 0.18));
    hud::brackets(&mut c, b, t);
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_strip_spans_twelve_hours_around_now_inside_the_day() {
        assert_eq!(window(14, 32), (8, 6.0 + 32.0 / 60.0));
        assert_eq!(window(2, 0), (0, 2.0));
        assert_eq!(window(23, 30), (12, 11.5));
        assert_eq!(window(99, 99).0, 12);
        assert_eq!(hour_minute("Mi 21.10. · 14:32 UTC"), Some((14, 32)));
        assert_eq!(hour_minute("Di 21.10. · 09:05"), Some((9, 5)));
        assert_eq!(hour_minute(""), None);
        assert_eq!(hour_minute("kaputt"), None);
    }

    #[test]
    fn the_now_marker_moves_only_with_the_minute() {
        let t = crate::theme::Theme::build(&Default::default());
        let rect = Rect { x: 51, y: 879, w: 1946, h: 137 };
        let area = crate::chrome_draw::buehne::padded(rect, &t, 0);
        let look = |now| StripLook { rect, theme: 1, lanes: vec![("A".into(), 0), ("B".into(), 1), ("C".into(), 2)], active: 0, now };
        let a = strip(&look(Some((14, 32))), &t, area);
        let b = strip(&look(Some((14, 32))), &t, area);
        let c2 = strip(&look(Some((14, 33))), &t, area);
        let diff = |x: &Canvas, y: &Canvas| (0..x.h).flat_map(|j| (0..x.w).map(move |i| (i, j))).filter(|&(i, j)| x.pixel(i, j) != y.pixel(i, j)).count();
        assert_eq!(diff(&a, &b), 0, "the same minute draws the same picture");
        assert!(diff(&a, &c2) > 0, "a new minute moves the marker");
    }
}
