//! MC-9b: how the installer overlay looks and where its buttons are. It is bitscomp's paint layer `System`, above
//! every window (fullscreen ones too), the bars, the menus and the notice; only the pointer lies above it (and the
//! pointer shows bitscomp's own arrow then, never a client's image). The fixed look: the whole screen dimmed, a
//! thick frame in the "waiting for Georg" colour around the whole screen with the tab "BitsOS fragt" at its top,
//! and the dialog in the middle. A client can draw a picture like it inside its window, but it can never draw over
//! this one, and its buttons answer nothing.

use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::element::memory::MemoryRenderBuffer;
use smithay::backend::renderer::element::solid::{SolidColorBuffer, SolidColorRenderElement};
use smithay::utils::{Physical, Point, Scale};

use crate::canvas::Canvas;
use crate::chrome_draw::{Px, font};
use crate::header::px_rect;
use crate::install_dialog::{AskDialog, Btn, GrantDialog, Stop, dur_label, wrap};
use crate::layout::Rect;
use crate::render::{CompElement, CompRenderer};
use crate::state::App;
use crate::text::Weight;
use crate::theme::{Theme, alpha};

/// The frame's width and the tab's size (logical px).
const EDGE: i32 = 6;
const TAB: (i32, i32) = (560, 30);
pub(crate) const PAD: i32 = 28;
const LABEL_H: i32 = 20;
pub(crate) const TITLE_H: i32 = 30;
pub(crate) const LINE_H: i32 = 22;
pub(crate) const NOTE_H: i32 = 20;
pub(crate) const BTN: (i32, i32) = (150, 40);
pub(crate) const ROW_H: i32 = 44;

pub const TAB_TEXT: &str = "BitsOS fragt · nur die echte Tastatur und Maus zählen";
const ASK_LABEL: &str = "Ein Programm möchte etwas am System ändern";
const GRANT_TITLE: &str = "Erlaubnis für Claude";
const MORE: &str = "Weiter unten steht mehr: Mausrad oder Bild ab. Erlauben geht erst, wenn du alles gesehen hast.";
const NONE_RUNNING: &str = "Zur Zeit darf Claude nichts ohne Rückfrage.";
/// The fixed texts (the glyph test of install_dialog checks every character).
#[cfg(test)]
pub const TEXTS: [&str; 10] = [TAB_TEXT, ASK_LABEL, GRANT_TITLE, MORE, NONE_RUNNING, "Ablehnen", "Erlauben", "Abbrechen", "Alle zurücknehmen", "Wie lange"];

fn note(secs: u64) -> String {
    format!("Noch {secs} s. Esc oder keine Antwort heißt Nein.")
}

pub(crate) fn centred(full: Rect, w: i32, h: i32) -> Rect {
    Rect { x: full.x + (full.w - w) / 2, y: full.y + ((full.h - h) / 2).max(EDGE + TAB.1), w, h }
}

pub(crate) fn ask_w(full: Rect) -> i32 {
    (full.w - 96).clamp(1, 760)
}

/// The most text lines the screen has room for (at least three).
fn room(full: Rect) -> usize {
    let fixed = 2 * PAD + LABEL_H + 8 + TITLE_H + 12 + 12 + NOTE_H + 16 + BTN.1;
    ((full.h - 2 * (EDGE + TAB.1 + 16) - fixed) / LINE_H).max(3) as usize
}

/// The dialog's frame and its two buttons (Ablehnen, Erlauben) for the lines it shows.
fn ask_boxes(full: Rect, shown: usize) -> (Rect, Rect, Rect) {
    let w = ask_w(full);
    let h = 2 * PAD + LABEL_H + 8 + TITLE_H + 12 + shown as i32 * LINE_H + 12 + NOTE_H + 16 + BTN.1;
    let f = centred(full, w, h);
    let bw = BTN.0.min((w - 2 * PAD - 12) / 2).max(1);
    let yes = Rect { x: f.x + w - PAD - bw, y: f.y + h - PAD - BTN.1, w: bw, h: BTN.1 };
    (f, Rect { x: yes.x - 12 - bw, ..yes }, yes)
}

fn shown_lines(a: &AskDialog) -> usize {
    a.visible.min(a.lines).max(1)
}

pub fn ask_buttons(full: Rect, a: &AskDialog) -> (Rect, Rect) {
    let (_, no, yes) = ask_boxes(full, shown_lines(a));
    (no, yes)
}

pub fn ask_hit(full: Rect, a: &AskDialog, x: f64, y: f64) -> Option<Btn> {
    let (no, yes) = ask_buttons(full, a);
    if yes.contains(x, y) {
        Some(Btn::Yes)
    } else if no.contains(x, y) {
        Some(Btn::No)
    } else {
        None
    }
}

/// The text of `a` wrapped to the dialog's width, and how many lines fit on the screen.
pub fn ask_lines(full: Rect, t: &Theme, text: &str) -> (Vec<String>, usize) {
    let f = font(t, 15.0, Weight::Regular);
    let max = ((ask_w(full) - 2 * PAD) as f64 * t.scale) as i32;
    (wrap(text, max, |s| f.width(s)), room(full))
}

fn grant_w(full: Rect) -> i32 {
    (full.w - 96).clamp(1, 640)
}

fn grant_frame(full: Rect) -> Rect {
    let h = 2 * PAD + TITLE_H + 8 + LINE_H + 16 + 2 * ROW_H + 10 + 12 + NOTE_H + 16 + BTN.1;
    centred(full, grant_w(full), h)
}

/// The grant dialog's parts: the two value rows, then its buttons from left to right.
pub fn grant_parts(full: Rect, g: &GrantDialog) -> Vec<(Stop, Rect)> {
    let f = grant_frame(full);
    let label_w = 140.min(f.w / 3);
    let row = |i: i32| Rect { x: f.x + PAD + label_w, y: f.y + PAD + TITLE_H + 8 + LINE_H + 16 + i * (ROW_H + 10), w: (f.w - 2 * PAD - label_w).max(1), h: ROW_H };
    let mut out = vec![(Stop::Kind, row(0)), (Stop::Until, row(1))];
    let by = f.y + f.h - PAD - BTN.1;
    let mut x = f.x + f.w - PAD;
    let mut buttons = Vec::new();
    for s in g.stops().into_iter().rev().filter(|s| !matches!(s, Stop::Kind | Stop::Until)) {
        let w = if s == Stop::Revoke { 190 } else { BTN.0 }.min((f.w - 2 * PAD) / 3).max(1);
        x -= w;
        buttons.push((s, Rect { x, y: by, w, h: BTN.1 }));
        x -= 12;
    }
    buttons.reverse();
    out.extend(buttons);
    out
}

pub fn grant_hit(full: Rect, g: &GrantDialog, x: f64, y: f64) -> Option<Stop> {
    grant_parts(full, g).into_iter().find(|(_, r)| r.contains(x, y)).map(|(s, _)| s)
}

#[derive(Clone, Debug, PartialEq)]
struct AskLook {
    frame: Rect,
    no: Rect,
    yes: Rect,
    title: String,
    lines: Vec<String>,
    note: String,
    on_yes: bool,
    hover: Option<Btn>,
    armed: bool,
    theme: u64,
}

#[derive(Clone, Debug, PartialEq)]
struct GrantLook {
    frame: Rect,
    parts: Vec<(Stop, Rect)>,
    kind: String,
    dur: String,
    sentence: String,
    running: String,
    focus: Stop,
    hover: Option<Stop>,
    theme: u64,
}

#[derive(Clone, Debug, PartialEq)]
enum Look {
    Ask(AskLook),
    Grant(GrantLook),
    /// MC-10.
    Access(crate::access_dialog::AccessLook),
}

#[derive(Default)]
pub struct Images {
    dim: Option<SolidColorBuffer>,
    edges: Vec<SolidColorBuffer>,
    tab: Option<((Rect, u64), MemoryRenderBuffer)>,
    dialog: Option<(Look, MemoryRenderBuffer)>,
    /// The wrapped text: (dialog id, width, theme) -> lines.
    lines: Option<((u64, i32, u64), Vec<String>)>,
    marker: Option<((String, Rect, u64), MemoryRenderBuffer)>,
}

/// A button: `focus` gets the ring (where Enter acts), `hover` only the fill (where the pointer is).
#[allow(clippy::too_many_arguments)]
pub(crate) fn button(c: &mut Canvas, p: &Px, t: &Theme, r: Rect, text: &str, focus: bool, hover: bool, live: bool) {
    crate::page_draw::button(c, p, t, r, text, focus || hover, live, &font(t, 14.0, Weight::Medium));
    if focus {
        c.rrect_line(p.b(r).inset(-p.v(3.0)), p.v(10.0), p.v(1.5), t.accent);
    }
}

pub(crate) fn frame_canvas(f: Rect, t: &Theme) -> (Canvas, Px) {
    let p = Px::new(f, t.scale);
    let (w, h) = p.size();
    let mut c = Canvas::new(w, h);
    let all = crate::canvas::Box2 { x: 0.0, y: 0.0, w: w as f32, h: h as f32 };
    c.rrect(all, p.v(14.0), t.panel);
    c.rrect_line(all, p.v(14.0), p.v(2.0), t.warm);
    (c, p)
}

fn draw_ask(l: &AskLook, t: &Theme) -> Canvas {
    let f = l.frame;
    let (mut c, p) = frame_canvas(f, t);
    let band = |y: i32, h: i32| Rect { x: f.x + PAD, y, w: f.w - 2 * PAD, h };
    let mut y = f.y + PAD;
    c.text_fit(&font(t, 13.0, Weight::Medium), p.r(band(y, LABEL_H)), ASK_LABEL, t.warm);
    y += LABEL_H + 8;
    c.text_fit(&font(t, 22.0, Weight::SemiBold), p.r(band(y, TITLE_H)), &l.title, t.text);
    y += TITLE_H + 12;
    let body = font(t, 15.0, Weight::Regular);
    for line in &l.lines {
        c.text_fit(&body, p.r(band(y, LINE_H)), line, t.text);
        y += LINE_H;
    }
    y += 12;
    c.text_fit(&font(t, 13.0, Weight::Regular), p.r(band(y, NOTE_H)), &l.note, t.muted);
    let (no, yes) = (l.no, l.yes);
    button(&mut c, &p, t, no, Btn::No.label(), !l.on_yes, l.hover == Some(Btn::No), true);
    button(&mut c, &p, t, yes, Btn::Yes.label(), l.on_yes, l.hover == Some(Btn::Yes), l.armed);
    c
}

fn draw_grant(l: &GrantLook, t: &Theme) -> Canvas {
    let f = l.frame;
    let (mut c, p) = frame_canvas(f, t);
    let band = |y: i32, h: i32| Rect { x: f.x + PAD, y, w: f.w - 2 * PAD, h };
    c.text_fit(&font(t, 22.0, Weight::SemiBold), p.r(band(f.y + PAD, TITLE_H)), GRANT_TITLE, t.text);
    c.text_fit(&font(t, 15.0, Weight::Medium), p.r(band(f.y + PAD + TITLE_H + 8, LINE_H)), &l.sentence, t.accent);
    let label = font(t, 14.0, Weight::Regular);
    for (stop, r) in &l.parts {
        let (focus, hover) = (l.focus == *stop, l.hover == Some(*stop));
        match stop {
            Stop::Kind | Stop::Until => {
                let lr = Rect { x: f.x + PAD, w: r.x - f.x - PAD, ..*r };
                c.text_mid(&label, p.r(Rect { w: lr.w, ..lr }), stop.label(), t.muted);
                let value = if *stop == Stop::Kind { &l.kind } else { &l.dur };
                button(&mut c, &p, t, *r, &format!("{value}  ·  ändern"), focus, hover, true);
            }
            b => button(&mut c, &p, t, *r, b.label(), focus, hover, true),
        }
    }
    let ry = l.parts.get(1).map_or(f.y, |(_, r)| r.y + r.h + 12);
    c.text_fit(&font(t, 13.0, Weight::Regular), p.r(band(ry, NOTE_H)), &l.running, t.muted);
    c
}

fn solid<R: CompRenderer>(buf: &mut SolidColorBuffer, r: Rect, s: f64, c: crate::theme::Rgba) -> CompElement<R> {
    buf.update((r.w, r.h), c);
    let loc: Point<i32, Physical> = Point::from((px_rect(r, s).x, px_rect(r, s).y));
    SolidColorRenderElement::from_buffer(buf, loc, Scale::from(s), 1.0, Kind::Unspecified).into()
}

impl App {
    /// Sets the shown dialog's line count and room (scrolling, "seen to the end"); cached by id, width, theme.
    pub fn install_fit(&mut self) {
        let full = self.area_full();
        let t = self.theme.clone();
        let tg = self.theme_gen;
        let Some(a) = self.install.ask.as_mut() else { return };
        let key = (a.id, full.w, tg);
        let im = &mut self.install.images;
        if im.lines.as_ref().is_none_or(|(k, _)| *k != key) {
            im.lines = Some((key, ask_lines(full, &t, &a.text).0));
        }
        let n = im.lines.as_ref().map_or(0, |(_, l)| l.len());
        a.fit(n, room(full));
    }
}

/// The overlay's elements, topmost first (paint layer `System`); the grant marker when the bar is hidden.
pub fn elements<R: CompRenderer>(app: &mut App, renderer: &mut R) -> Vec<CompElement<R>> {
    let t = app.theme.clone();
    let s = t.scale;
    let full = app.area_full();
    let mut out = Vec::new();
    if !app.install.shown() {
        app.install.images.dialog = None;
        if app.chrome().status.is_none()
            && let Some(m) = app.install_marker()
        {
            let f = font(&t, 13.0, Weight::Medium);
            let w = (crate::chrome::logical_width(&f, &m, s) + 24).min(full.w);
            let r = Rect { x: full.x + full.w - w - 8, y: full.y + full.h - 38, w, h: 30 };
            let key = (m.clone(), r, app.theme_gen);
            let im = &mut app.install.images;
            if im.marker.as_ref().is_none_or(|(k, _)| *k != key) {
                im.marker = Some((key, crate::chrome_draw::notice(&m, r, &t).into_buffer()));
            }
            if let Some((_, buf)) = &im.marker {
                let p = px_rect(r, s);
                out.extend(crate::deco::image(renderer, buf, r, (p.w, p.h), s));
            }
        }
        return out;
    }
    app.install_fit();
    let now = crate::pacer::now();
    let tg = app.theme_gen;
    let look = if let Some(a) = &app.install.ask {
        let all = app.install.images.lines.as_ref().map(|(_, l)| l.as_slice()).unwrap_or(&[]);
        let shown = shown_lines(a);
        let lines: Vec<String> = all.iter().skip(a.scroll).take(shown).cloned().collect();
        let more = a.scroll + shown < a.lines;
        let (frame, no, yes) = ask_boxes(full, shown);
        Look::Ask(AskLook {
            frame,
            no,
            yes,
            title: a.title.clone(),
            lines,
            note: if more { MORE.to_string() } else { note(a.secs_left(now)) },
            on_yes: a.on_yes,
            hover: a.hover,
            armed: a.armed(now),
            theme: tg,
        })
    } else if let Some(a) = &app.install.access {
        let lines = app.access_lines();
        let note = a.deadline.map(|d| format!("Noch {} s. Nur die echte Tastatur und Maus zählen. Esc heißt Ablehnen.", d.saturating_sub(now).as_millis().div_ceil(1000)));
        Look::Access(crate::access_dialog::AccessLook::of(full, a, app.access_title(a), lines, note, a.armed(now), tg))
    } else if let Some(g) = &app.install.grant {
        let d = g.durs[g.dur.min(g.durs.len() - 1)];
        let until = app.install_hm(app.install_until(d));
        Look::Grant(GrantLook {
            frame: grant_frame(full),
            parts: grant_parts(full, g),
            kind: crate::install_dialog::KINDS[g.kind].1.to_string(),
            dur: format!("{} (bis {until})", dur_label(d)),
            sentence: g.sentence(&until),
            running: app.install_grant_marker().map_or(NONE_RUNNING.to_string(), |m| format!("Läuft schon: {}", m.trim_start_matches("Claude darf: "))),
            focus: g.focus,
            hover: g.hover,
            theme: tg,
        })
    } else {
        return out;
    };
    let im = &mut app.install.images;
    // The tab on the frame's top edge.
    let tab = Rect { x: full.x + (full.w - TAB.0.min(full.w - 32)) / 2, y: full.y, w: TAB.0.min(full.w - 32).max(1), h: TAB.1 };
    if im.tab.as_ref().is_none_or(|(k, _)| *k != (tab, tg)) {
        let p = Px::new(tab, s);
        let (w, h) = p.size();
        let mut c = Canvas::new(w, h);
        let b = crate::canvas::Box2 { x: 0.0, y: -p.v(10.0), w: w as f32, h: h as f32 + p.v(10.0) };
        c.rrect(b, p.v(10.0), t.warm);
        c.text_mid(&font(&t, 13.0, Weight::SemiBold), Rect { x: 0, y: 0, w, h }, TAB_TEXT, t.bg);
        im.tab = Some(((tab, tg), c.into_buffer()));
    }
    if let Some((_, buf)) = &im.tab {
        let p = px_rect(tab, s);
        out.extend(crate::deco::image(renderer, buf, tab, (p.w, p.h), s));
    }
    // The frame around the whole screen: top, bottom, left, right.
    let edges = [
        Rect { x: full.x, y: full.y, w: full.w, h: EDGE },
        Rect { x: full.x, y: full.y + full.h - EDGE, w: full.w, h: EDGE },
        Rect { x: full.x, y: full.y, w: EDGE, h: full.h },
        Rect { x: full.x + full.w - EDGE, y: full.y, w: EDGE, h: full.h },
    ];
    im.edges.resize_with(4, || SolidColorBuffer::new((1, 1), t.warm));
    for (b, r) in im.edges.iter_mut().zip(edges) {
        out.push(solid(b, r, s, t.warm));
    }
    if im.dialog.as_ref().is_none_or(|(l, _)| *l != look) {
        let c = match &look {
            Look::Ask(l) => draw_ask(l, &t),
            Look::Grant(l) => draw_grant(l, &t),
            Look::Access(l) => crate::access_dialog::draw(l, &t),
        };
        im.dialog = Some((look.clone(), c.into_buffer()));
    }
    let frame = match &look {
        Look::Ask(l) => l.frame,
        Look::Grant(l) => l.frame,
        Look::Access(l) => l.frame,
    };
    if let Some((_, buf)) = &im.dialog {
        let p = px_rect(frame, s);
        out.extend(crate::deco::image(renderer, buf, frame, (p.w, p.h), s));
    }
    let dim = im.dim.get_or_insert_with(|| SolidColorBuffer::new((1, 1), t.bg));
    out.push(solid(dim, full, s, alpha(t.bg, 0.82)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn ask(lines: usize, visible: usize) -> AskDialog {
        let mut a = AskDialog::new(1, "install".into(), "t".into(), String::new(), Duration::ZERO, Duration::from_secs(5));
        a.fit(lines, visible);
        a
    }

    #[test]
    fn the_dialog_and_its_buttons_lie_inside_the_frame_on_any_screen() {
        for full in [Rect { x: 0, y: 0, w: 2048, h: 1152 }, Rect { x: 0, y: 0, w: 1280, h: 720 }, Rect { x: 0, y: 0, w: 400, h: 300 }] {
            let a = ask(200, room(full));
            let (f, no, yes) = ask_boxes(full, shown_lines(&a));
            assert!(f.x >= full.x + EDGE && f.x + f.w <= full.x + full.w - EDGE, "{full:?}");
            assert!(f.y >= EDGE + TAB.1, "below the tab {full:?}");
            assert!(no.x + no.w < yes.x && yes.x + yes.w <= f.x + f.w && no.x >= f.x, "{full:?}");
            if full.h >= 700 {
                assert!(f.y + f.h <= full.y + full.h - EDGE, "the whole box is on a real screen {full:?}");
            }
            assert_eq!(ask_hit(full, &a, (yes.x + 2) as f64, (yes.y + 2) as f64), Some(Btn::Yes));
            assert_eq!(ask_hit(full, &a, (no.x + 2) as f64, (no.y + 2) as f64), Some(Btn::No));
            assert_eq!(ask_hit(full, &a, f.x as f64 + 1.0, f.y as f64 + 1.0), None);
            for revoke in [false, true] {
                let g = GrantDialog::new(crate::install_dialog::DURS.to_vec(), revoke);
                let gf = grant_frame(full);
                let parts = grant_parts(full, &g);
                assert_eq!(parts.len(), if revoke { 5 } else { 4 });
                for (stop, r) in &parts {
                    assert!(r.x >= gf.x && r.x + r.w <= gf.x + gf.w && r.w > 0, "{stop:?} {full:?}");
                    assert_eq!(grant_hit(full, &g, (r.x + r.w / 2) as f64, (r.y + r.h / 2) as f64), Some(*stop));
                }
            }
        }
        // A tiny output: no overflow, no panic.
        let _ = ask_boxes(Rect { x: 0, y: 0, w: 10, h: 10 }, usize::MAX / 64);
        let _ = grant_parts(Rect { x: 0, y: 0, w: 10, h: 10 }, &GrantDialog::new(vec![crate::install_dialog::Dur::Secs(5)], true));
    }

    #[test]
    fn a_long_installer_text_wraps_inside_the_box() {
        let t = Theme::default();
        let full = Rect { x: 0, y: 0, w: 2048, h: 1152 };
        let text = format!("Werkzeug: install_kernel\nurl: {}\nDanach startet die neue Fassung.", "x".repeat(300));
        let (lines, room) = ask_lines(full, &t, &text);
        let f = font(&t, 15.0, Weight::Regular);
        let max = ((ask_w(full) - 2 * PAD) as f64 * t.scale) as i32;
        assert!(lines.len() >= 4 && room >= 3, "{lines:?}");
        assert!(lines.iter().all(|l| f.width(l) <= max), "{lines:?}");
        assert_eq!(lines.concat().matches('x').count(), 300, "nothing of the argument lost");
    }
}
