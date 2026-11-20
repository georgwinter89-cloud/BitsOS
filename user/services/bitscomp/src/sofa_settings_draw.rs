//! GM-6c: the look of the sofa mode's Einstellungen (DS rule "new look files"; plan/spec/sofa-mode.md §1, 2.5,
//! board Einstellungen.dc.html): the left column (420 board px, a line at its right; the active section
//! accent_soft with accent text), the title (Bricolage 48 600), the rows (96 high, radius 20, panel with the 1 px
//! line; title 22 600 and a muted line 17; at the right ‹ value › with round 52 px buttons and the value in
//! JetBrains Mono 22 in 200 px, a switch 84x46 with a 36 px knob, or a value), the focused row with the big focus
//! ring, Konten's two lines, a note line, the learn screen's box and the question box. Physical px = board px x k;
//! always the dark tokens. A pure drawing: sofa_draw.rs caches it by its look.

use crate::canvas::{Box2, Canvas};
use crate::layout::Rect;
use crate::sofa::Frame;
use crate::sofa_draw::{Ink, font};
use crate::sofa_focus::Item;
use crate::sofa_header_draw::{local, token_ring};
use crate::sofa_layout::{FOOTER_H, HEADER_H, PAD, SECTIONS};
use crate::sofa_quick::Confirm;
use crate::sofa_settings::{Ctl, KONTEN, SRow};
use crate::sofa_tile_draw::{band, focus_ring};
use crate::text::{Typeface, Weight};
use crate::theme::alpha;

#[derive(Clone, Debug, PartialEq)]
pub struct SettingsLook {
    pub frame: Frame,
    pub bw: i32,
    pub bh: i32,
    pub section: usize,
    pub rows: Vec<SRow>,
    pub items: Vec<Item>,
    pub focus: Option<String>,
    pub note: Option<String>,
    /// The learn screen: how many buttons are done and the next one's name.
    pub learn: Option<(usize, &'static str)>,
    pub confirm: Option<Confirm>,
}

/// The learn screen's words for a button name of `sofa.pad.map`.
pub fn learn_word(n: &str) -> &'static str {
    match n {
        "a" => "A",
        "b" => "B",
        "x" => "X",
        "y" => "Y",
        "lb" => "LB (links oben)",
        "rb" => "RB (rechts oben)",
        "select" => "Select",
        "start" => "Start",
        _ => "?",
    }
}

/// A row's ‹ value › (board px): the round buttons and the value between them, at least 200 wide (wider for a
/// longer value such as "Zuletzt gespielt"); the mouse hits the same rects (sofa_app).
pub fn arrows(r: Rect, value: &str) -> (Rect, Rect, Rect) {
    let right = r.x + r.w - 28;
    let mid = r.y + r.h / 2;
    let vw = crate::text::Font::of(Typeface::JetBrainsMono, 22.0, Weight::Medium, 1.0).width(value).max(200) + 8;
    let next = Rect { x: right - 52, y: mid - 26, w: 52, h: 52 };
    let val = Rect { x: next.x - 24 - vw, y: mid - 16, w: vw, h: 32 };
    let prev = Rect { x: val.x - 24 - 52, ..next };
    (prev, val, next)
}

fn control(c: &mut Canvas, f: &Frame, o: Rect, r: Rect, ctl: &Ctl, ink: &Ink) {
    let k = f.k as f32;
    let right = r.x + r.w - 28;
    let mid = r.y + r.h / 2;
    match ctl {
        Ctl::Arrows { value, greyed } => {
            let (fg, edge) = if *greyed { (alpha(ink.text, 0.4), alpha(ink.line_strong, 0.5)) } else { (ink.text, ink.line_strong) };
            let (prev, val, next) = arrows(r, value);
            let arrow = font(Typeface::Figtree, 24.0, Weight::Regular, k);
            for (b, s) in [(prev, "‹"), (next, "›")] {
                let bb = local(f, o, b);
                c.rrect_line(bb, bb.h / 2.0, k.max(1.0), edge);
                c.text_mid(&arrow, band(bb), s, fg);
            }
            c.text_mid(&font(Typeface::JetBrainsMono, 22.0, Weight::Medium, k), band(local(f, o, val)), value, fg);
        }
        Ctl::Switch(on) => {
            let s = local(f, o, Rect { x: right - 84, y: mid - 23, w: 84, h: 46 });
            let kr = 18.0 * k;
            if *on {
                c.rrect(s, s.h / 2.0, ink.accent);
                c.disc(s.x + s.w - 5.0 * k - kr, s.y + s.h / 2.0, kr, ink.on_accent);
            } else {
                c.rrect_line(s, s.h / 2.0, k.max(1.0), ink.line_strong);
                c.disc(s.x + 5.0 * k + kr, s.y + s.h / 2.0, kr, ink.muted);
            }
        }
        Ctl::Value(v) => {
            c.text_right(&font(Typeface::JetBrainsMono, 22.0, Weight::Regular, k), band(local(f, o, Rect { x: r.x + r.w / 2, y: mid - 16, w: right - r.x - r.w / 2, h: 32 })), v, ink.text);
        }
        Ctl::Action => {
            c.text_right(&font(Typeface::Figtree, 28.0, Weight::Regular, k), band(local(f, o, Rect { x: right - 40, y: mid - 18, w: 40, h: 36 })), "›", ink.muted);
        }
    }
}

/// The body between the header and the footer; `o` is the image's physical place.
pub fn draw_settings(l: &SettingsLook, o: Rect, ink: &Ink) -> Canvas {
    let f = &l.frame;
    let k = f.k as f32;
    let mut c = Canvas::new(o.w, o.h);
    let col = local(f, o, Rect { x: 420, y: HEADER_H, w: 1, h: l.bh - HEADER_H - FOOTER_H });
    c.fill(col.x as i32, 0, k.max(1.0).round() as i32, o.h, ink.line);
    let focus = l.focus.as_deref();
    for it in &l.items {
        let b = local(f, o, it.rect);
        if let Some(n) = it.id.strip_prefix("section:").and_then(|n| n.parse::<usize>().ok()) {
            let active = n == l.section;
            if active {
                c.rrect(b, 14.0 * k, ink.accent_soft);
            }
            let (w, fg) = if active { (Weight::SemiBold, ink.accent) } else { (Weight::Medium, ink.text) };
            let t = Rect { x: it.rect.x + 22, w: it.rect.w - 44, ..it.rect };
            c.text_fit(&font(Typeface::Figtree, 22.0, w, k), band(local(f, o, t)), SECTIONS.get(n).map_or("", |s| s.0), fg);
            if focus == Some(it.id.as_str()) {
                token_ring(&mut c, b, 14.0 * k, k, ink);
            }
            continue;
        }
        let Some(row) = it.id.strip_prefix("row:").and_then(|n| n.parse::<usize>().ok()).and_then(|n| l.rows.get(n)) else { continue };
        c.rrect(b, 20.0 * k, ink.panel);
        c.rrect_line(b, 20.0 * k, k.max(1.0), ink.line);
        let tw = it.rect.w / 2 - 28;
        c.text_fit(&font(Typeface::Figtree, 22.0, Weight::SemiBold, k), band(local(f, o, Rect { x: it.rect.x + 28, y: it.rect.y + 18, w: tw, h: 30 })), &row.title, ink.text);
        c.text_fit(&font(Typeface::Figtree, 17.0, Weight::Regular, k), band(local(f, o, Rect { x: it.rect.x + 28, y: it.rect.y + 50, w: it.rect.w - 56 - 380, h: 26 })), &row.line, ink.muted);
        control(&mut c, f, o, it.rect, &row.ctl, ink);
        if focus == Some(it.id.as_str()) {
            focus_ring(&mut c, b, 20.0 * k, k, ink);
        }
    }
    let x = 420 + PAD;
    let title = Rect { x, y: HEADER_H + 40, w: (1200 - 2 * PAD).min(l.bw - x - PAD), h: 52 };
    c.text_fit(&font(Typeface::Bricolage, 48.0, Weight::SemiBold, k), band(local(f, o, title)), SECTIONS.get(l.section).map_or("", |s| s.0), ink.text);
    let mut y = l.items.iter().filter(|i| i.id.starts_with("row:")).map(|i| i.rect.y + i.rect.h).max().unwrap_or(title.y + title.h + 10) + 24;
    if l.section == 4 {
        for t in KONTEN {
            c.text_fit(&font(Typeface::Figtree, 22.0, Weight::Regular, k), band(local(f, o, Rect { x, y, w: title.w, h: 32 })), t, ink.text);
            y += 44;
        }
    }
    if let Some(n) = &l.note {
        c.text_fit(&font(Typeface::Figtree, 19.0, Weight::Medium, k), band(local(f, o, Rect { x, y, w: title.w, h: 28 })), n, ink.accent);
    }
    if let Some((done, next)) = l.learn {
        let r = Rect { x: (l.bw - 760) / 2, y: (l.bh - 320) / 2, w: 760, h: 320 };
        let b = local(f, o, r);
        c.rrect(b, 20.0 * k, ink.bg);
        c.rrect_line(b, 20.0 * k, k.max(1.0), ink.lifted_ring);
        let line = |c: &mut Canvas, dy: i32, h: i32, px: f32, w: Weight, s: &str, col| {
            c.text_mid(&font(Typeface::Figtree, px, w, k), band(local(f, o, Rect { x: r.x + 28, y: r.y + dy, w: r.w - 56, h })), s, col);
        };
        line(&mut c, 28, 40, 30.0, Weight::SemiBold, "Tasten zuordnen", ink.text);
        line(&mut c, 100, 70, 48.0, Weight::Bold, &format!("Drücke {}", learn_word(next)), ink.accent);
        line(&mut c, 190, 30, 20.0, Weight::Regular, &format!("{done} von 8 gelernt"), ink.muted);
        line(&mut c, 250, 30, 18.0, Weight::Regular, "Abbrechen: Esc · Rechtsklick", ink.muted);
    }
    if let Some(q) = l.confirm {
        let all = Box2 { x: 0.0, y: 0.0, w: o.w as f32, h: o.h as f32 };
        c.rrect(all, 0.0, alpha(ink.bg, 0.6));
        crate::sofa_quick_draw::question(&mut c, f, o, l.bw, l.bh, false, q, focus, ink);
    }
    c
}
