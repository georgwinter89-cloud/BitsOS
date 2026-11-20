//! REWIND: the rewind view's look (Verlauf.dc.html without streaks and blur), drawn in the
//! library's paint layer over the whole screen: the header (Zur Bühne, "Zurückspulen", the date), the upper
//! panel (the chosen time big in JetBrains Mono with how long ago and its kind, the three ways back, the chosen
//! way's side: folders, programs, or what "Alles" means), the lower panel (the time line, newest first, with its
//! arrows), the foot (what happened, "Zum Jetzt"), and over it the pages' confirmation dialog. One picture while
//! the view is open; a change redraws only the parts that changed into it (rewind_paint.rs).

use smithay::backend::renderer::element::memory::MemoryRenderBuffer;

use crate::canvas::Canvas;
use crate::chrome_draw::Px;
use crate::header::px_rect;
use crate::layout::Rect;
use crate::page_confirm::Dialog;
use crate::render::{CompElement, CompRenderer};
use crate::rewind::{self, Avail, EntryKind, Hot, SnapKind, Tab};
use crate::rewind_view::{self as view, Geo, PaneGeo};
use crate::state::App;
use crate::text::{Family, Weight};
use crate::theme::{Style, Theme, alpha};

pub const CHOOSE: &str = "Wähle links, was zurückkommen soll.";
pub const KEYS: &str = "← → Stand   ↑ ↓ wählen   Tab Weg   Enter   Esc schließen";

#[derive(Clone, Debug, PartialEq)]
struct ChipLook {
    rect: Rect,
    time: String,
    day: String,
    kind: SnapKind,
    pick: bool,
    hover: bool,
}

#[derive(Clone, Debug, PartialEq)]
struct RowLook {
    rect: Rect,
    title: String,
    detail: String,
    folder: bool,
    pick: bool,
    hover: bool,
}

#[derive(Clone, Debug, PartialEq)]
struct Look {
    g: Geo,
    pane: PaneGeo,
    date: String,
    time: String,
    ago: String,
    kind: Option<(SnapKind, String)>,
    tab: Tab,
    chips: Vec<ChipLook>,
    head: String,
    up: bool,
    rows: Vec<RowLook>,
    /// The pane's text when it has no rows (empty, loading, an error, the choice, "Alles").
    lines: Vec<(String, bool)>,
    says: String,
    act: (String, String),
    foot: Option<(String, bool)>,
    avail: Avail,
    hover: Option<Hot>,
    dialog: Option<Dialog>,
    theme: u64,
}

#[derive(Default)]
pub struct Images {
    /// The picture, the look it shows and its size in pixels.
    view: Option<(Look, MemoryRenderBuffer, (i32, i32))>,
    /// The view's redraws since it opened: how many, the longest (ms) and the pixels drawn (`ctl rewind state`).
    pub draws: u64,
    pub draw_max_ms: f64,
    pub draw_px: u64,
}

fn ui(t: &Theme, px: f32, w: Weight) -> crate::text::Font {
    t.bfont(Family::Ui, px, w)
}

fn button(c: &mut Canvas, p: &Px, t: &Theme, r: Rect, text: &str, hover: bool, strong: bool) {
    let k = t.board as f32;
    let b = p.b(r);
    let rad = 14.0 * k;
    if strong {
        c.rrect(b, rad, if hover { t.accent } else { alpha(t.accent, 0.85) });
        c.text_mid(&ui(t, 14.0, Weight::SemiBold), p.r(r), text, t.on_accent);
    } else {
        c.rrect(b, rad, if hover { t.panel2 } else { alpha(t.panel2, 0.4) });
        c.rrect_line(b, rad, k.max(1.0), if hover { t.accent } else { t.line_strong });
        c.text_mid(&ui(t, 14.0, Weight::SemiBold), p.r(r), text, t.text);
    }
}

/// A chevron in `r` pointing left (`left`) or right.
fn chevron(c: &mut Canvas, p: &Px, t: &Theme, r: Rect, left: bool, col: crate::theme::Rgba) {
    let k = t.board as f32;
    let b = p.b(r);
    let (cx, cy, s) = (b.x + b.w / 2.0, b.y + b.h / 2.0, 6.0 * k);
    let (a, z) = if left { (cx + s / 2.0, cx - s / 2.0) } else { (cx - s / 2.0, cx + s / 2.0) };
    c.line((a, cy - s), (z, cy), 2.0 * k, col);
    c.line((z, cy), (a, cy + s), 2.0 * k, col);
}

fn kind_colour(t: &Theme, k: SnapKind) -> crate::theme::Rgba {
    match k {
        SnapKind::Auto => t.line_strong,
        SnapKind::Manual => t.accent,
        SnapKind::BeforeRestore | SnapKind::BeforeRollback => t.warm,
    }
}

/// The view's part `at` (logical; the whole view is `l.g.full`) as a picture of its own size.
fn draw(l: &Look, t: &Theme, at: Rect) -> Canvas {
    let g = &l.g;
    let p = Px::new(at, t.scale);
    let (w, h) = p.size();
    let k = t.board as f32;
    let one = k.max(1.0);
    let mut c = Canvas::new(w, h);
    c.fill(0, 0, w, h, t.bg);
    let lab = t.style(Style::Label);
    let mono = |px: f32, wt: Weight| t.bfont(Family::Mono, px, wt);
    // The header.
    let hb = p.b(g.back);
    c.rrect_line(hb, 14.0 * k, one, if l.hover == Some(Hot::Back) { t.accent } else { t.line_strong });
    chevron(&mut c, &p, t, Rect { w: t.bl(36.0), ..g.back }, true, t.text);
    c.text_mid(&ui(t, 14.0, Weight::SemiBold), p.r(Rect { x: g.back.x + t.bl(24.0), w: g.back.w - t.bl(24.0), ..g.back }), view::BACK, t.text);
    c.text_fit(&t.bfont(Family::Display, 24.0, Weight::SemiBold), p.r(g.title), "Zurückspulen", t.text);
    c.text_right(&mono(14.0, Weight::Medium), p.r(g.date), &l.date, t.muted);
    // The upper panel.
    let ub = p.b(g.upper);
    c.rrect(ub, 28.0 * k, alpha(t.panel, 0.5));
    c.rrect_line(ub, 28.0 * k, one, alpha(t.accent, 0.2));
    c.text_fit(&lab, p.r(g.label), &Theme::label(Style::Label, "Stand um"), t.muted);
    c.text_fit(&mono(120.0, Weight::Medium), p.r(g.time), &l.time, t.text);
    c.text_fit(&mono(14.0, Weight::Regular), p.r(g.ago), &l.ago, t.muted);
    if let Some((kind, label)) = &l.kind {
        let f = ui(t, 13.0, Weight::SemiBold);
        let word = kind.word();
        let cw = (f.width(word) as f64 / t.scale).ceil() as i32 + t.bl(28.0);
        let pill = Rect { w: cw, ..g.chip };
        c.rrect(p.b(pill), p.v(pill.h as f32 / 2.0), kind_colour(t, *kind));
        c.text_mid(&f, p.r(pill), word, t.bg);
        if !label.is_empty() {
            let band = Rect { x: pill.x + pill.w + t.bl(12.0), w: g.chip.w - pill.w - t.bl(12.0), ..g.chip };
            c.text_fit(&mono(12.0, Weight::Regular), p.r(band), label, t.muted);
        }
    }
    for (i, r) in g.ways.iter().enumerate() {
        let tab = view::way(i);
        let (on, hover) = (l.tab == tab, l.hover == Some(Hot::Way(tab)));
        let b = p.b(*r);
        c.rrect(b, 20.0 * k, if on { t.accent_soft } else { alpha(t.panel2, 0.5) });
        c.rrect_line(b, 20.0 * k, one, if on { t.accent } else if hover { t.line_strong } else { t.line });
        let top = Rect { x: r.x + t.bl(22.0), y: r.y + t.bl(12.0), w: r.w - t.bl(44.0), h: (r.h / 2 - t.bl(8.0)).max(1) };
        c.text_fit(&ui(t, 17.0, Weight::SemiBold), p.r(top), tab.title(), t.text);
        let low = Rect { y: top.y + top.h + t.bl(2.0), ..top };
        c.text_fit(&ui(t, 14.0, Weight::Regular), p.r(low), tab.line(), t.muted);
    }
    pane(&mut c, &p, l, t);
    // The lower panel: the time line.
    let lb = p.b(g.lower);
    c.rrect(lb, 28.0 * k, alpha(t.bg, 0.75));
    c.rrect_line(lb, 28.0 * k, one, alpha(t.accent, 0.2));
    c.text_fit(&lab, p.r(g.lower_label), &Theme::label(Style::Label, "Verlauf"), t.muted);
    c.text_fit(&t.bfont(Family::Display, 20.0, Weight::SemiBold), p.r(g.lower_day), "Stände", t.text);
    for (r, left, hot) in [(g.older, true, Hot::Older), (g.newer, false, Hot::Newer)] {
        c.rrect_line(p.b(r), 14.0 * k, one, if l.hover == Some(hot) { t.accent } else { t.line_strong });
        chevron(&mut c, &p, t, r, left, t.text);
    }
    if l.chips.is_empty() {
        let band = Rect { x: g.chips.first().map_or(g.lower.x, |r| r.x), y: g.lower.y, w: g.lower.w / 2, h: g.lower.h };
        c.text_fit(&ui(t, 16.0, Weight::Medium), p.r(band), l.avail.reason().unwrap_or("Noch kein Stand gespeichert."), t.muted);
    }
    for ch in &l.chips {
        let b = p.b(ch.rect);
        c.rrect(b, 18.0 * k, if ch.pick { t.accent_soft } else { alpha(t.panel2, 0.4) });
        c.rrect_line(b, 18.0 * k, if ch.pick { 2.0 * k } else { one }, if ch.pick { t.accent } else if ch.hover { t.line_strong } else { t.line });
        let r = ch.rect;
        let dot = p.b(Rect { x: r.x + t.bl(18.0), y: r.y + t.bl(20.0), w: t.bl(10.0), h: t.bl(10.0) });
        c.disc(dot.x + dot.w / 2.0, dot.y + dot.h / 2.0, dot.w / 2.0, kind_colour(t, ch.kind));
        let band = |y: f32, hh: f32| Rect { x: r.x + t.bl(16.0), y: r.y + t.bl(y), w: r.w - t.bl(32.0), h: t.bl(hh) };
        c.text_fit(&mono(24.0, Weight::SemiBold), p.r(band(40.0, 30.0)), &ch.time, if ch.pick { t.accent } else { t.text });
        c.text_fit(&ui(t, 13.0, Weight::Medium), p.r(band(76.0, 18.0)), &ch.day, t.muted);
        c.text_fit(&ui(t, 12.0, Weight::Regular), p.r(band(100.0, 18.0)), ch.kind.short(), t.muted);
    }
    // The foot.
    if let Some((text, bad)) = &l.foot {
        let band = Rect { w: g.foot.w * 2 / 3, ..g.foot };
        c.text_fit(&ui(t, 15.0, Weight::Medium), p.r(band), text, if *bad { t.danger } else { t.text });
    } else {
        let band = Rect { w: g.now.x - g.foot.x - t.bl(24.0), ..g.foot };
        c.text_fit(&lab, p.r(band), &Theme::label(Style::Label, KEYS), t.muted);
    }
    button(&mut c, &p, t, g.now, view::NOW, l.hover == Some(Hot::Now), false);
    if let Some(d) = &l.dialog {
        let hover = match l.hover {
            Some(Hot::DialogOk) => Some(crate::page::Part::DialogOk),
            Some(Hot::DialogCancel) => Some(crate::page::Part::DialogCancel),
            _ => None,
        };
        crate::page_confirm::draw(&mut c, &p, t, g.full, d, hover);
    }
    c
}

/// The chosen way's side.
fn pane(c: &mut Canvas, p: &Px, l: &Look, t: &Theme) {
    let k = t.board as f32;
    let one = k.max(1.0);
    let g = &l.g;
    let pb = p.b(g.pane);
    c.rrect(pb, 24.0 * k, alpha(t.panel, 0.35));
    c.rrect_line(pb, 24.0 * k, one, alpha(t.accent, 0.3));
    let pg = &l.pane;
    if l.tab == Tab::Choose {
        c.text_mid(&ui(t, 17.0, Weight::Medium), p.r(g.pane), CHOOSE, t.muted);
        return;
    }
    if l.tab == Tab::Path && l.up {
        c.rrect_line(p.b(pg.up), 14.0 * k, one, if l.hover == Some(Hot::Up) { t.accent } else { t.line_strong });
        c.text_mid(&ui(t, 14.0, Weight::SemiBold), p.r(pg.up), "Ordner höher", t.text);
    }
    if !l.head.is_empty() {
        let f = if l.tab == Tab::Path { t.bfont(Family::Mono, 14.0, Weight::Medium) } else { t.style(Style::Label) };
        let band = if l.tab == Tab::Path && l.up { pg.head } else { Rect { x: pg.up.x, w: pg.head.x + pg.head.w - pg.up.x, ..pg.head } };
        c.text_fit(&f, p.r(band), &l.head, t.muted);
    }
    for r in &l.rows {
        let b = p.b(r.rect);
        c.rrect(b, 14.0 * k, if r.pick { t.accent_soft } else { alpha(t.panel2, 0.5) });
        c.rrect_line(b, 14.0 * k, one, if r.pick { alpha(t.accent, 0.7) } else if r.hover { t.line_strong } else { t.line });
        let m = Rect { x: r.rect.x + t.bl(16.0), y: r.rect.y + (r.rect.h - t.bl(12.0)) / 2, w: t.bl(12.0), h: t.bl(12.0) };
        c.rrect(p.b(m), 3.0 * k, if r.folder { t.accent } else { t.line_strong });
        let dw = r.rect.w * 2 / 5;
        let title = Rect { x: m.x + m.w + t.bl(14.0), w: r.rect.x + r.rect.w - dw - t.bl(16.0) - (m.x + m.w + t.bl(14.0)), ..r.rect };
        c.text_fit(&ui(t, 16.0, if r.pick { Weight::SemiBold } else { Weight::Medium }), p.r(title), &r.title, t.text);
        let detail = Rect { x: r.rect.x + r.rect.w - dw - t.bl(16.0), w: dw, ..r.rect };
        c.text_right(&t.bfont(Family::Mono, 12.0, Weight::Medium), p.r(detail), &r.detail, t.muted);
    }
    let lines = view::text_lines(g, t, l.lines.len());
    let top = if l.tab == Tab::All { 0 } else { pg.up.y + pg.up.h + t.bl(16.0) - lines.first().map_or(0, |r| r.y) };
    for ((text, strong), r) in l.lines.iter().zip(lines) {
        let r = Rect { y: r.y + top, ..r };
        c.text_fit(&ui(t, 16.0, if *strong { Weight::SemiBold } else { Weight::Regular }), p.r(r), text, if *strong { t.text } else { t.muted });
    }
    if !l.says.is_empty() {
        c.text_fit(&ui(t, 14.0, Weight::Regular), p.r(pg.says), &l.says, t.muted);
    }
    let (a, b) = &l.act;
    if !a.is_empty() {
        let hot = if l.tab == Tab::All && !b.is_empty() { Hot::Restart } else { Hot::Do };
        button(c, p, t, pg.act, a, l.hover == Some(hot), true);
    }
    if !b.is_empty() {
        button(c, p, t, pg.act2, b, l.hover == Some(Hot::Undo), false);
    }
}

fn look(app: &App) -> Look {
    let r = &app.rewind;
    let g = app.rewind_geo();
    let pane = app.rewind_pane(&g);
    let (off, now) = (r.off_min, r.now_s);
    let s = r.chosen();
    let chips = g
        .chips
        .iter()
        .enumerate()
        .filter_map(|(k, rect)| {
            let i = r.first + k;
            let sn = r.snaps.get(i)?;
            Some(ChipLook { rect: *rect, time: rewind::clock(sn.created_s, off), day: rewind::day(sn.created_s, off, now), kind: sn.kind, pick: i == r.pick, hover: r.hover == Some(Hot::Chip(i)) })
        })
        .collect();
    let (mut head, mut rows, mut lines, mut says, mut up) = (String::new(), Vec::new(), Vec::new(), String::new(), false);
    match r.tab {
        Tab::Choose => {}
        Tab::Path => {
            let b = &r.browse;
            (head, up) = (b.path.clone(), rewind::parent(&b.path).is_some());
            rows = pane
                .rows
                .iter()
                .enumerate()
                .filter_map(|(k, rect)| {
                    let i = b.first + k;
                    let e = b.entries.get(i)?;
                    let detail = match (e.kind, e.bytes) {
                        (EntryKind::Folder, _) if e.open => "Ordner ›".to_string(),
                        (EntryKind::Folder, _) => "Ordner".into(),
                        (EntryKind::Link, _) => "Verknüpfung".into(),
                        (_, Some(n)) => rewind::size(n),
                        _ => String::new(),
                    };
                    Some(RowLook { rect: *rect, title: e.name.clone(), detail, folder: e.kind == EntryKind::Folder, pick: b.pick == Some(i), hover: r.hover == Some(Hot::Row(i)) })
                })
                .collect();
            if b.loading {
                lines.push(("Wird geladen …".into(), false));
            } else if let Some(e) = &b.error {
                lines.push((e.clone(), true));
            } else if b.entries.is_empty() {
                lines.push(("Dieser Ordner ist in diesem Stand leer.".into(), false));
            }
            says = match b.pick.and_then(|i| b.entries.get(i)) {
                Some(e) => format!("Gewählt: {}", e.name),
                None if up => "Gewählt: dieser Ordner".into(),
                None => String::new(),
            };
        }
        Tab::Program => {
            let pr = &r.progs;
            head = Theme::label(Style::Label, "Programme in diesem Stand");
            rows = pane
                .rows
                .iter()
                .enumerate()
                .filter_map(|(k, rect)| {
                    let i = pr.first + k;
                    let e = pr.list.get(i)?;
                    let detail = match (&e.version, &e.now) {
                        (Some(v), Some(n)) if v != n => format!("{v} · jetzt {n}"),
                        (Some(v), _) => format!("Version {v}"),
                        (None, _) if e.data => "nur Daten".into(),
                        _ => String::new(),
                    };
                    Some(RowLook { rect: *rect, title: e.name.clone(), detail, folder: e.data, pick: pr.pick == Some(i), hover: r.hover == Some(Hot::Row(i)) })
                })
                .collect();
            if pr.loading {
                lines.push(("Wird geladen …".into(), false));
            } else if let Some(e) = &pr.error {
                lines.push((e.clone(), true));
            } else if pr.list.is_empty() {
                lines.push(("In diesem Stand gibt es keine Programme mit Daten.".into(), false));
            }
            if let Some(e) = pr.pick.and_then(|i| pr.list.get(i)) {
                says = format!("Gewählt: {}", e.name);
            }
        }
        Tab::All => {
            lines = rewind::ALL_TEXT.iter().map(|s| (s.to_string(), false)).collect();
            lines[0].1 = true;
            if r.set_for_start || r.pending.is_some() {
                lines.push((String::new(), false));
                lines.push(("Für den nächsten Start ist ein Zurücksetzen eingestellt.".into(), true));
            }
        }
    }
    let foot = r.busy.clone().map(|b| (b, false)).or_else(|| r.msg.clone());
    Look {
        g: g.clone(),
        pane,
        date: if now > 0 { rewind::date_long(now, off) } else { String::new() },
        time: s.map_or("--:--".into(), |s| rewind::clock(s.created_s, off)),
        ago: s.map_or(String::new(), |s| format!("{} · {} · {}", rewind::ago(s.created_s, now), rewind::day(s.created_s, off, now), rewind::date_long(s.created_s, off))),
        kind: s.map(|s| {
            let own = !matches!(s.label.as_str(), "auto" | "manual" | crate::rewind::BEFORE_RESTORE | crate::rewind::BEFORE_ROLLBACK);
            (s.kind, if own { s.label.clone() } else { String::new() })
        }),
        tab: r.tab,
        chips,
        head,
        up,
        rows,
        lines,
        says,
        act: crate::rewind_app::act_texts(r),
        foot,
        avail: r.avail.clone(),
        hover: r.hover,
        dialog: r.dialog.as_ref().map(|(d, _)| d.clone()),
        theme: app.theme_gen,
    }
}

/// The view's element; none while it is closed.
pub fn elements<R: CompRenderer>(app: &mut App, renderer: &mut R) -> Vec<CompElement<R>> {
    let mut out = Vec::new();
    if !paint::paint(app) {
        return out;
    }
    let (s, at) = (app.theme.scale, app.rewind_geo().full);
    let Some((_, buf, _)) = &app.rewind.images.view else { return out };
    let px = px_rect(at, s);
    out.extend(crate::deco::image(renderer, buf, at, (px.w, px.h), s));
    out
}

#[path = "rewind_paint.rs"]
pub(crate) mod paint;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_text_of_the_view_has_its_glyphs() {
        let t = Theme::default();
        let (u, m, lab) = (ui(&t, 16.0, Weight::Medium), t.bfont(Family::Mono, 120.0, Weight::Medium), t.style(Style::Label));
        let texts = [CHOOSE, view::BACK, view::NOW, "Zurückspulen", "Gespeicherte Stände", "Ordner höher", "Ordner ›", "Verknüpfung", "„brief.txt“ zurückholen", "vor dem Zurücksetzen", "vor dem Zurückholen", "Größe 12,3 kB"];
        for s in texts.iter().copied().chain(rewind::ALL_TEXT).chain(Tab::WAYS.iter().flat_map(|w| [w.title(), w.line()])) {
            assert!(s.chars().all(|c| c == ' ' || u.draws(c)), "{s}");
        }
        assert!("0123456789:-".chars().all(|c| m.draws(c)));
        for s in [KEYS, "Stand um", "Verlauf", "Programme in diesem Stand"] {
            assert!(Theme::label(Style::Label, s).chars().all(|c| c == ' ' || lab.draws(c)), "{s}");
        }
    }
}
