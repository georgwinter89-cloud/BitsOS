//! The Feld's look (Feld.dc.html), drawn in the library's paint layer (the two never show together): the screen
//! below the top bar dimmed as for the library, and the Feld board's panel: the head with Claude's ring, the typed
//! text in Bricolage Grotesque with its caret (the invitation while empty) and the Esc chip; the rows under their
//! uppercase labels (the chosen one in the accent's soft ground with "Enter" at its right); the foot with the
//! assistant's name, what Enter does and the key hints. Left out of the design for now: the
//! microphone, the "Versuch" chips and "Bühne bauen" (nothing behind them yet). One cached image.

use smithay::backend::renderer::element::Kind as ElementKind;
use smithay::backend::renderer::element::memory::MemoryRenderBuffer;
use smithay::backend::renderer::element::solid::{SolidColorBuffer, SolidColorRenderElement};
use smithay::utils::{Physical, Point, Scale};

use crate::canvas::Canvas;
use crate::chrome_draw::Px;
use crate::feld::{self, Kind};
use crate::header::px_rect;
use crate::layout::Rect;
use crate::render::{CompElement, CompRenderer};
use crate::state::App;
use crate::text::{Family, Weight};
use crate::theme::{Style, Theme, alpha};

pub const NOTHING: &str = "Dazu kennt BitsOS keine Adresse und kein Programm.";

#[derive(Clone, Debug, PartialEq)]
struct RowLook {
    label: Option<(Rect, &'static str)>,
    rect: Rect,
    kind: Kind,
    title: String,
    detail: String,
    pick: bool,
    hover: bool,
}

#[derive(Clone, Debug, PartialEq)]
struct Look {
    /// The image's place: the panel.
    at: Rect,
    g: crate::feld_view::Geo,
    text: String,
    /// The text before the cursor (where the caret is drawn).
    before: String,
    rows: Vec<RowLook>,
    name: String,
    says: String,
    hover_esc: bool,
    theme: u64,
}

#[derive(Default)]
pub struct Images {
    dim: Option<SolidColorBuffer>,
    panel: Option<(Look, MemoryRenderBuffer)>,
}

fn label(t: &Theme) -> crate::text::Font {
    t.style(Style::Label)
}

fn draw(l: &Look, t: &Theme) -> Canvas {
    let p = Px::new(l.at, t.scale);
    let (w, h) = p.size();
    let k = t.board as f32;
    let one = k.max(1.0);
    let mut c = Canvas::new(w, h);
    let panel = p.b(l.g.panel);
    let rad = 28.0 * k;
    c.rrect(panel, rad, t.panel);
    // The foot in the accent's soft ground, under the line that parts it from the rows.
    let foot = p.r(l.g.foot);
    c.rrect_rows(panel, rad, foot.y as f32, (foot.y + foot.h) as f32, alpha(t.accent_soft, 0.35));
    c.fill(foot.x, foot.y, foot.w, one as i32, t.line);
    c.rrect_line(panel, rad, one, alpha(t.accent, 0.4));
    head(&mut c, &p, l, t);
    for r in &l.rows {
        row(&mut c, &p, r, t);
    }
    if l.rows.is_empty() && !l.text.trim().is_empty() {
        let b = p.r(l.g.body);
        c.text_mid(&t.bfont(Family::Ui, 16.0, Weight::Medium), Rect { h: (52.0 * k) as i32, ..b }, NOTHING, t.muted);
    }
    // The foot: the assistant's ring and name, what Enter does.
    let cy = foot.y as f32 + foot.h as f32 / 2.0;
    let rx = foot.x as f32 + 32.0 * k + 11.0 * k;
    c.ring(rx, cy, 7.0 * k, 2.4 * k, t.accent);
    let nf = t.bfont(Family::Ui, 16.0, Weight::SemiBold);
    let nx = (rx + 11.0 * k + 16.0 * k) as i32;
    let nw = c.text_fit(&nf, Rect { x: nx, y: foot.y, w: (foot.w / 4).max(1), h: foot.h }, &l.name, t.accent);
    let sx = nx + nw + (16.0 * k) as i32;
    let sf = t.bfont(Family::Ui, 15.0, Weight::Regular);
    let keys = p.r(l.g.keys);
    c.text_fit(&sf, Rect { x: sx, y: foot.y, w: (keys.x - (24.0 * k) as i32 - sx).max(1), h: foot.h }, &l.says, t.text);
    c.text_right(&label(t), keys, &crate::feld_view::keys_text(), t.muted);
    c
}

/// The head: Claude's ring (a faint outer circle, the ring, its dot), the text with the caret, the Esc chip.
fn head(c: &mut Canvas, p: &Px, l: &Look, t: &Theme) {
    let k = t.board as f32;
    let one = k.max(1.0);
    let hd = p.r(l.g.head);
    c.fill(hd.x, hd.y + hd.h - one as i32, hd.w, one as i32, t.line);
    let i = p.b(l.g.icon);
    let (cx, cy) = (i.x + i.w / 2.0, i.y + i.h / 2.0);
    c.arc(cx, cy, 15.0 * k, 1.5 * k, Some((4.0 * k, 4.0 * k, 0.0)), alpha(t.accent, 0.5));
    c.ring(cx, cy, 9.0 * k, 2.6 * k, t.accent);
    c.disc(cx, cy, 3.0 * k, t.accent);
    let big = t.bfont(Family::Display, 34.0, Weight::Medium);
    let tr = p.r(l.g.text);
    let caret_h = (36.0 * k) as i32;
    let caret_y = tr.y + (tr.h - caret_h) / 2;
    let caret_x = if l.text.is_empty() {
        let x = tr.x;
        c.text_fit(&big, Rect { x: x + (12.0 * k) as i32, w: tr.w - (12.0 * k) as i32, ..tr }, feld::PLACEHOLDER, t.muted);
        x
    } else {
        c.text_fit(&big, tr, &l.text, t.text);
        tr.x + big.width(&l.before).min(tr.w) + (2.0 * k) as i32
    };
    c.fill(caret_x, caret_y, (3.0 * k).round().max(1.0) as i32, caret_h, t.accent);
    let esc = p.b(l.g.esc);
    if l.hover_esc {
        c.rrect(esc, 14.0 * k, t.panel2);
    }
    c.rrect_line(esc, 14.0 * k, one, if l.hover_esc { t.accent } else { t.line });
    c.text_mid(&label(t), p.r(l.g.esc), &Theme::label(Style::Label, crate::library_draw::ESC), t.muted);
}

/// A row as the design's: a small colour mark, the title, the quiet detail, "Enter" on the chosen one.
fn row(c: &mut Canvas, p: &Px, r: &RowLook, t: &Theme) {
    let k = t.board as f32;
    let one = k.max(1.0);
    if let Some((lr, text)) = r.label {
        c.text_fit(&label(t), p.r(lr), &Theme::label(Style::Label, text), t.muted);
    }
    let b = p.b(r.rect);
    let rad = 14.0 * k;
    if r.pick {
        c.rrect(b, rad, t.accent_soft);
        c.rrect_line(b, rad, one, alpha(t.accent, 0.7));
    } else {
        c.rrect(b, rad, alpha(t.panel2, 0.5));
        c.rrect_line(b, rad, one, if r.hover { t.line_strong } else { t.line });
    }
    let (mark, title, detail, hint) = crate::feld_view::row_parts(r.rect, t);
    c.rrect(p.b(mark), 3.0 * k, if r.kind == Kind::Ask { t.accent } else { t.line_strong });
    let tf = t.bfont(Family::Ui, 16.0, if r.pick { Weight::SemiBold } else { Weight::Medium });
    c.text_fit(&tf, p.r(title), &r.title, t.text);
    c.text_right(&t.bfont(Family::Mono, 12.0, Weight::Medium), p.r(detail), &r.detail, t.muted);
    if r.pick {
        c.text_right(&t.bfont(Family::Mono, 11.0, Weight::Medium), p.r(hint), "Enter", t.accent);
    }
}

fn solid<R: CompRenderer>(buf: &mut SolidColorBuffer, r: Rect, s: f64, c: crate::theme::Rgba) -> CompElement<R> {
    buf.update((r.w, r.h), c);
    let p = px_rect(r, s);
    let loc: Point<i32, Physical> = Point::from((p.x, p.y));
    SolidColorRenderElement::from_buffer(buf, loc, Scale::from(s), 1.0, ElementKind::Unspecified).into()
}

fn look(app: &App) -> Look {
    let (g, rows, placed) = app.feld_placed();
    let pick = app.feld.pick(rows.len());
    let rows = rows
        .iter()
        .zip(&placed)
        .enumerate()
        .map(|(i, (r, (lab, rect)))| RowLook {
            label: lab.map(|l| (l, r.kind.group())),
            rect: *rect,
            kind: r.kind,
            title: r.title.clone(),
            detail: r.detail.clone(),
            pick: pick == Some(i),
            hover: app.feld.hover == Some(i),
        })
        .collect::<Vec<_>>();
    let chosen = pick.and_then(|i| app.feld_placed().1.get(i).cloned());
    let at = g.panel;
    let e = &app.feld.edit;
    Look {
        at,
        g,
        text: e.text.clone(),
        before: e.before().to_string(),
        rows,
        name: if app.assist_on() { app.assist.name.clone() } else { app.desk.claude_name.clone() },
        says: feld::says(chosen.as_ref()),
        hover_esc: app.feld.hover_esc,
        theme: app.theme_gen,
    }
}

/// The Feld's elements, topmost first; none while it is closed.
pub fn elements<R: CompRenderer>(app: &mut App, renderer: &mut R) -> Vec<CompElement<R>> {
    let mut out = Vec::new();
    if !app.feld.open {
        app.feld.images.panel = None;
        return out;
    }
    let t = app.theme.clone();
    let s = t.scale;
    let l = look(app);
    let dim = l.g.dim;
    let im = &mut app.feld.images;
    if im.panel.as_ref().is_none_or(|(o, _)| *o != l) {
        im.panel = Some((l.clone(), draw(&l, &t).into_buffer()));
    }
    let buf = &im.panel.as_ref().unwrap().1;
    let p = px_rect(l.at, s);
    out.extend(crate::deco::image(renderer, buf, l.at, (p.w, p.h), s));
    let d = im.dim.get_or_insert_with(|| SolidColorBuffer::new((1, 1), t.bg));
    out.push(solid(d, dim, s, alpha(t.bg, 0.62)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_text_of_the_feld_has_its_glyphs() {
        let t = Theme::default();
        let (big, ui, lab) = (t.bfont(Family::Display, 34.0, Weight::Medium), t.bfont(Family::Ui, 16.0, Weight::SemiBold), label(&t));
        for s in [feld::PLACEHOLDER, "einstellungen/aussehen", "wie spät ist es? Größe"] {
            assert!(s.chars().all(|c| c == ' ' || big.draws(c)), "{s}");
        }
        let says = [feld::says(None), "Enter öffnet einstellungen/aussehen.".into(), "Enter startet „Terminal“.".into(), "Enter stellt die Frage in der Seitenleiste an Claude.".into()];
        for s in says.iter().map(String::as_str).chain([NOTHING, feld::ASK, "Adressen Programme Frage"]) {
            assert!(s.chars().all(|c| c == ' ' || ui.draws(c)), "{s}");
        }
        for s in crate::feld_view::KEYS.iter().chain(["Adressen", "Programme", "Frage", crate::library_draw::ESC].iter()) {
            assert!(Theme::label(Style::Label, s).chars().all(|c| c == ' ' || lab.draws(c)), "{s}");
        }
    }

    #[test]
    fn the_panel_draws_its_rows_in_both_modes() {
        for dark in [true, false] {
            let mut t = Theme::default();
            t.dark = dark;
            let g = crate::feld_view::geo(Rect { x: 0, y: 0, w: 2560, h: 1440 }, 60, &t);
            let kinds = [Kind::Address, Kind::Program, Kind::Ask];
            let placed = crate::feld_view::place(&g, &kinds, &t);
            assert_eq!(placed.len(), 3);
            let rows = kinds
                .iter()
                .zip(&placed)
                .enumerate()
                .map(|(i, (k, (lab, r)))| RowLook { label: lab.map(|l| (l, k.group())), rect: *r, kind: *k, title: "Terminal".into(), detail: "programm/terminal".into(), pick: i == 0, hover: i == 1 })
                .collect();
            let at = g.panel;
            let l = Look { at, g, text: "term".into(), before: "te".into(), rows, name: "Claude".into(), says: "Enter startet „Terminal“.".into(), hover_esc: false, theme: 0 };
            let c = draw(&l, &t);
            let p = Px::new(l.at, t.scale);
            let inside = |r: Rect| p.r(Rect { x: r.x + 4, y: r.y + r.h / 2, w: 1, h: 1 });
            let first = inside(placed[0].1);
            assert_eq!(c.pixel(first.x, first.y), crate::canvas::pack(t.accent_soft), "the chosen row lies in the accent's soft ground");
            let mid = p.r(Rect { x: g.panel.x + g.panel.w / 2, y: g.panel.y + g.panel.h / 2 + 200, w: 1, h: 1 });
            assert_ne!(c.pixel(mid.x, mid.y), 0, "the panel is filled");
            // The caret sits after "te", inside the text's band.
            let tr = p.r(g.text);
            assert!((tr.y..tr.y + tr.h).any(|y| (tr.x..tr.x + tr.w).any(|x| c.pixel(x, y) == crate::canvas::pack(t.accent))), "the caret is drawn");
        }
    }
}
