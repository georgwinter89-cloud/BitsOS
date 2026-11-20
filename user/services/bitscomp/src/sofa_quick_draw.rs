//! GM-6c: the look of the sofa mode's quick menu (DS rule "new look files"; plan/spec/sofa-mode.md §1, 2.4, board
//! Schnellmenue.dc.html): on GM-6a's panel frame the Läuft-gerade well (bg, radius 20) with Weiterspielen (accent)
//! and Spiel beenden (warnung outline), Lautstärke with "30 % · HDMI", the slider (track 14, knob 36 with the token
//! ring), the output chips (the active one accent_soft), the 2x2 tiles (panel, 1 px line, label and value), Zum
//! normalen Desktop, Neu starten, Ausschalten (line_strong outline), a note line, and the question box with Ja and
//! Nein. Physical px = board px x k; always the dark tokens. A pure drawing: sofa_draw.rs caches it by its look.

use crate::canvas::{Box2, Canvas};
use crate::layout::Rect;
use crate::sofa::Frame;
use crate::sofa_draw::{Ink, font};
use crate::sofa_focus::Item;
use crate::sofa_header_draw::{local, token_ring};
use crate::sofa_layout::{self as lay, QUICK_RUN_H, QUICK_W};
use crate::sofa_quick::Confirm;
use crate::sofa_tile_draw::{band, label, warnung};
use crate::text::{Typeface, Weight};
use crate::theme::alpha;

#[derive(Clone, Debug, PartialEq)]
pub struct QuickLook {
    pub frame: Frame,
    pub bw: i32,
    pub bh: i32,
    pub clock: String,
    pub running: Option<String>,
    pub volume: Option<u8>,
    pub volume_text: String,
    /// (label, active) of each output chip.
    pub outputs: Vec<(String, bool)>,
    /// (label, value) of WLAN, Gamepad, Bildschirm, Leistung.
    pub tiles: Vec<(&'static str, String)>,
    pub note: Option<String>,
    pub confirm: Option<Confirm>,
    pub items: Vec<Item>,
    pub focus: Option<String>,
}

fn rect_of(items: &[Item], id: &str) -> Option<Rect> {
    items.iter().find(|i| i.id == id).map(|i| i.rect)
}

/// A button of the board: `fill` (accent) or an outline in `edge`, its word centred (or from the left).
#[allow(clippy::too_many_arguments)]
fn button(c: &mut Canvas, f: &Frame, o: Rect, r: Rect, word: &str, fill: Option<crate::theme::Rgba>, edge: crate::theme::Rgba, fg: crate::theme::Rgba, left: bool) {
    let k = f.k as f32;
    let b = local(f, o, r);
    match fill {
        Some(col) => c.rrect(b, 14.0 * k, col),
        None => c.rrect_line(b, 14.0 * k, k.max(1.0), edge),
    }
    let ft = font(Typeface::Figtree, 20.0, if fill.is_some() { Weight::Bold } else { Weight::SemiBold }, k);
    if left {
        c.text_fit(&ft, band(local(f, o, Rect { x: r.x + 22, w: r.w - 44, ..r })), word, fg);
    } else {
        c.text_mid(&ft, band(b), word, fg);
    }
}

/// The question's box and its Ja / Nein (in the quick panel or, from Einstellungen, in the middle of the screen).
pub fn question(c: &mut Canvas, f: &Frame, o: Rect, bw: i32, bh: i32, panel: bool, q: Confirm, focus: Option<&str>, ink: &Ink) {
    let k = f.k as f32;
    let r = lay::confirm_box(bw, bh, panel);
    let b = local(f, o, r);
    c.rrect(b, 20.0 * k, ink.bg);
    c.rrect_line(b, 20.0 * k, k.max(1.0), ink.lifted_ring);
    let t = Rect { x: r.x + 28, y: r.y + 28, w: r.w - 56, h: 40 };
    c.text_fit(&font(Typeface::Bricolage, 30.0, Weight::SemiBold, k), band(local(f, o, t)), q.title(), ink.text);
    let s = Rect { y: t.y + 52, h: 30, ..t };
    c.text_fit(&font(Typeface::Figtree, 20.0, Weight::Regular, k), band(local(f, o, s)), q.text(), ink.muted);
    for it in lay::confirm(bw, bh, panel) {
        let yes = it.id == "c:yes";
        let word = if yes { "Ja" } else { "Nein" };
        match (yes, q) {
            (true, Confirm::End) => button(c, f, o, it.rect, word, None, warnung(), warnung(), false),
            (true, _) => button(c, f, o, it.rect, word, Some(ink.accent), ink.accent, ink.on_accent, false),
            _ => button(c, f, o, it.rect, word, None, ink.line_strong, ink.text, false),
        }
        if focus == Some(it.id.as_str()) {
            token_ring(c, local(f, o, it.rect), 14.0 * k, k, ink);
        }
    }
}

/// The panel's content on GM-6a's frame (`sofa_header_draw::draw_quick`); `o` is the panel's physical place.
pub fn draw(l: &QuickLook, o: Rect, ink: &Ink) -> Canvas {
    let f = &l.frame;
    let k = f.k as f32;
    let mut c = crate::sofa_header_draw::draw_quick(f, l.bw, l.bh, &l.clock, o, ink);
    let x = l.bw - QUICK_W + 48;
    let w = QUICK_W - 96;
    let focus = l.focus.as_deref();
    let ring = |c: &mut Canvas, id: &str, r: f32| {
        if focus == Some(id)
            && let Some(rr) = rect_of(&l.items, id)
        {
            token_ring(c, local(f, o, rr), r * k, k, ink);
        }
    };
    if let Some(name) = &l.running {
        let well = Rect { x, y: 48 + 52 + 28, w, h: QUICK_RUN_H };
        let b = local(f, o, well);
        c.rrect(b, 20.0 * k, ink.bg);
        c.rrect_line(b, 20.0 * k, k.max(1.0), ink.line);
        c.text_fit(&label(k), band(local(f, o, Rect { x: x + 24, y: well.y + 22, w: w - 48, h: 16 })), "LÄUFT GERADE", ink.muted);
        c.text_fit(&font(Typeface::Figtree, 24.0, Weight::SemiBold, k), band(local(f, o, Rect { x: x + 24, y: well.y + 48, w: w - 48, h: 32 })), name, ink.text);
        if let Some(r) = rect_of(&l.items, "q:resume") {
            button(&mut c, f, o, r, "Weiterspielen", Some(ink.accent), ink.accent, ink.on_accent, false);
        }
        if let Some(r) = rect_of(&l.items, "q:end") {
            button(&mut c, f, o, r, "Spiel beenden", None, warnung(), warnung(), false);
        }
        ring(&mut c, "q:resume", 14.0);
        ring(&mut c, "q:end", 14.0);
    }
    let vy = lay::quick_volume_y(l.running.is_some());
    c.text_fit(&font(Typeface::Figtree, 22.0, Weight::SemiBold, k), band(local(f, o, Rect { x, y: vy, w, h: 30 })), "Lautstärke", ink.text);
    c.text_right(&font(Typeface::JetBrainsMono, 18.0, Weight::Regular, k), band(local(f, o, Rect { x, y: vy, w, h: 30 })), &l.volume_text, ink.muted);
    if let Some(s) = rect_of(&l.items, "q:volume") {
        let track = local(f, o, Rect { x: s.x, y: s.y + (s.h - 14) / 2, w: s.w, h: 14 });
        c.rrect(track, track.h / 2.0, ink.line);
        if let Some(p) = l.volume {
            let fill = Box2 { w: track.w * p as f32 / 100.0, ..track };
            if fill.w >= 1.0 {
                c.rrect(fill, track.h / 2.0, ink.accent);
            }
            let kx = track.x + track.w * p as f32 / 100.0;
            let (cy, r) = (track.y + track.h / 2.0, 18.0 * k);
            let kx = kx.clamp(track.x + r, track.x + track.w - r);
            c.disc(kx, cy, r, ink.text);
            if focus == Some("q:volume") {
                token_ring(&mut c, Box2 { x: kx - r, y: cy - r, w: 2.0 * r, h: 2.0 * r }, r, k, ink);
            }
        } else if focus == Some("q:volume") {
            token_ring(&mut c, track, track.h / 2.0, k, ink);
        }
    }
    let chip = font(Typeface::Figtree, 17.0, Weight::SemiBold, k);
    for (i, (word, active)) in l.outputs.iter().enumerate() {
        let id = format!("q:out:{i}");
        let Some(r) = rect_of(&l.items, &id) else { continue };
        let b = local(f, o, r);
        if *active {
            c.rrect(b, b.h / 2.0, ink.accent_soft);
            c.text_mid(&chip, band(b), word, ink.accent);
        } else {
            c.rrect_line(b, b.h / 2.0, k.max(1.0), ink.line_strong);
            c.text_mid(&chip, band(b), word, ink.text);
        }
        ring(&mut c, &id, r.h as f32 / 2.0);
    }
    let value = font(Typeface::Figtree, 21.0, Weight::SemiBold, k);
    for ((lbl, v), id) in l.tiles.iter().zip(["q:wlan", "q:pad", "q:screen", "q:power"]) {
        let Some(r) = rect_of(&l.items, id) else { continue };
        let b = local(f, o, r);
        c.rrect(b, 20.0 * k, ink.panel);
        c.rrect_line(b, 20.0 * k, k.max(1.0), ink.line);
        c.text_fit(&label(k), band(local(f, o, Rect { x: r.x + 20, y: r.y + 18, w: r.w - 40, h: 16 })), &lbl.to_uppercase(), ink.muted);
        c.text_fit(&value, band(local(f, o, Rect { x: r.x + 20, y: r.y + r.h - 18 - 28, w: r.w - 40, h: 28 })), v, ink.text);
        ring(&mut c, id, 20.0);
    }
    for (id, word, left) in [("q:desktop", "Zum normalen Desktop", true), ("q:reboot", "Neu starten", false), ("q:off", "Ausschalten", false)] {
        if let Some(r) = rect_of(&l.items, id) {
            button(&mut c, f, o, r, word, None, ink.line_strong, ink.text, left);
        }
        ring(&mut c, id, 14.0);
    }
    if let (Some(n), Some(d)) = (&l.note, rect_of(&l.items, "q:desktop")) {
        let t = Rect { x, y: d.y - 14 - 26, w, h: 26 };
        c.text_fit(&font(Typeface::Figtree, 18.0, Weight::Medium, k), band(local(f, o, t)), n, ink.accent);
    }
    if let Some(q) = l.confirm {
        let all = local(f, o, Rect { x: l.bw - QUICK_W, y: 0, w: QUICK_W, h: l.bh });
        c.fill(all.x as i32, 0, o.w - all.x as i32, o.h, alpha(ink.bg, 0.6));
        question(&mut c, f, o, l.bw, l.bh, true, q, focus, ink);
    }
    c
}
