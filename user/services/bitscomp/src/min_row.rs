//! C (Georg 12.11.: "Außerdem fehlt noch eine Methode um Fenster zu minimieren und sie wiederherzustellen"): the
//! way back of minimised windows in the Bühne's bottom row. Each chip has the program's letter (as a library tile
//! shows it without an icon) and the title; what does not fit is behind "+N", which opens a list of every
//! minimised window of the workspace above the row (a click restores; any other press closes the list).
//! DESK-LEFT: the classic preset has the same "+N" and list (right of its window list); every chip, entry and list
//! line has a small x that closes the window without restoring it (`min_close.rs`).

use smithay::backend::renderer::element::memory::MemoryRenderBuffer;

use crate::canvas::{Box2, Canvas};
use crate::chrome::Part as C;
use crate::chrome_draw::Px;
use crate::desk::Spot;
use crate::layout::Rect;
use crate::state::App;
use crate::text::{Family, Weight};
use crate::theme::{Theme, alpha};

#[derive(Default)]
pub struct MinList {
    pub open: bool,
    /// The hovered spot is a line of the open list (`App::bar_hover`).
    pub pointer_on: bool,
    image: Option<(ListLook, MemoryRenderBuffer)>,
}

#[derive(Clone, Debug, PartialEq)]
struct ListLook {
    /// Index among the minimised windows, place, letter, title, its x (the ones the list has room for).
    entries: Vec<(usize, Rect, char, String, Rect)>,
    hover: Option<usize>,
    /// The pointer is on the hovered line's x.
    on_x: bool,
    theme: u64,
}

impl App {
    /// The program's letter of a minimised window: its library entry's name, else its app_id, else its title.
    pub fn min_letter(&self, w: &crate::win::Window) -> char {
        let (app_id, _) = crate::ctl::names(w);
        let name = self.library.entries.iter().find(|e| !app_id.is_empty() && e.owns(&app_id)).map(|e| e.name.clone());
        crate::library::letter(&name.unwrap_or_else(|| if app_id.is_empty() { crate::minimize::label(w) } else { app_id }))
    }

    pub fn min_items_shown(&self) -> Vec<(char, String)> {
        self.ws.minimized.iter().map(|m| (self.min_letter(&m.win), crate::minimize::label(&m.win))).collect()
    }

    /// "+N": the list opens or closes.
    pub fn min_more_press(&mut self) {
        self.desk.min_list.open = !self.desk.min_list.open && !self.ws.minimized.is_empty();
        eprintln!("bitscomp: minimised windows: list {}", if self.desk.min_list.open { "open" } else { "closed" });
        self.needs_frame = true;
    }

    /// The list's entry under (x, y) while it is open (a click restores it, as a chip does). A list whose bar is
    /// folded is not there (it is closed at the next frame or fold).
    pub fn min_list_spot(&self, x: f64, y: f64) -> Option<Spot> {
        if !self.desk.min_list.open {
            return None;
        }
        let c = self.chrome();
        if !c.list_shown {
            return None;
        }
        let n = self.ws.minimized.len();
        if let Some(i) = c.min_list_close.iter().take(n).position(|r| r.contains(x, y)) {
            return Some(Spot::Chrome(C::MinClose(i)));
        }
        c.min_list.iter().take(n).position(|r| r.contains(x, y)).map(|i| Spot::Chrome(C::Min(i)))
    }

    /// The hover the bars draw: a minimised window's index under the pointer is the open list's while the pointer is
    /// on the list, not the chip's or entry's of the same index (they would light up and show their x).
    pub fn bar_hover(&self) -> Option<Spot> {
        match self.desk.hover {
            Some(Spot::Chrome(C::Min(_) | C::MinClose(_))) if self.desk.min_list.pointer_on && self.desk.min_list.open => None,
            h => h,
        }
    }

    /// A press anywhere but on the list or its "+N" closes the list.
    pub fn min_list_press(&mut self) {
        if !self.desk.min_list.open {
            return;
        }
        let keep = matches!(self.spot_at(self.pointer.x, self.pointer.y), Some(Spot::Chrome(C::Min(_) | C::MinMore | C::MinClose(_))));
        if !keep {
            self.desk.min_list.open = false;
            self.needs_frame = true;
        }
    }

    /// The open list's place (its lines and their margin) while its bar is shown: a revealed bar stays while the
    /// pointer is on it.
    pub fn min_list_area(&self) -> Option<Rect> {
        let b = self.chrome();
        if !(b.list_shown && self.desk.min_list.open && b.min_more.is_some()) {
            return None;
        }
        let rects: Vec<Rect> = b.min_list.iter().take(self.ws.minimized.len()).copied().filter(|r| r.w > 0).collect();
        bounds(&rects, self.theme.bl(8.0))
    }

    /// `ctl hot`'s lines: the "+N" chip, and the open list's entries.
    pub fn min_row_hot(&self) -> String {
        let b = self.chrome();
        if !b.list_shown {
            return String::new();
        }
        let s = self.theme.scale;
        let pt = |r: Rect| format!("{},{}", ((r.x as f64 + r.w as f64 / 2.0) * s).round(), ((r.y as f64 + r.h as f64 / 2.0) * s).round());
        let mut out = String::new();
        if let Some(r) = b.min_more {
            let n = self.ws.minimized.len() - b.min_items.len().min(self.ws.minimized.len());
            out.push_str(&format!("minmore at={} count={n} open={}\n", pt(r), self.desk.min_list.open));
        }
        if self.desk.min_list.open {
            for (i, ((r, x), m)) in b.min_list.iter().zip(&b.min_list_close).zip(&self.ws.minimized).enumerate().filter(|(_, ((r, _), _))| r.w > 0) {
                out.push_str(&format!("minlist {i} at={} close={} title={:?}\n", pt(*r), pt(*x), crate::minimize::label(&m.win)));
            }
        }
        out
    }

    /// The open list's image, topmost.
    pub fn min_list_elements<R: crate::render::CompRenderer>(&mut self, renderer: &mut R) -> Vec<crate::render::CompElement<R>> {
        let b = self.chrome();
        if !(b.list_shown && self.desk.min_list.open && !self.ws.minimized.is_empty() && b.min_more.is_some()) {
            // A list that cannot be seen (its bar folded, nothing behind "+N" any more) is closed, so it never takes
            // clicks over the tiles and does not come back by itself when the bar does.
            self.desk.min_list.open = false;
            self.desk.min_list.image = None;
            return Vec::new();
        }
        let entries: Vec<(usize, Rect, char, String, Rect)> = b
            .min_list
            .iter()
            .zip(&b.min_list_close)
            .zip(self.min_items_shown())
            .enumerate()
            .filter(|(_, ((r, _), _))| r.w > 0)
            .map(|(i, ((r, x), (ch, name)))| (i, *r, ch, name, *x))
            .collect();
        // The hover is the list's only while the pointer is on the list (a chip of the same index is not).
        let on_list = self.min_list_spot(self.pointer.x, self.pointer.y).is_some();
        let (hover, on_x) = match self.desk.hover {
            Some(Spot::Chrome(C::Min(i))) if on_list => (Some(i), false),
            Some(Spot::Chrome(C::MinClose(i))) if on_list => (Some(i), true),
            _ => (None, false),
        };
        let (t, look) = (self.theme.clone(), ListLook { entries, hover, on_x, theme: self.theme_gen });
        let rects: Vec<Rect> = look.entries.iter().map(|e| e.1).collect();
        let Some(area) = bounds(&rects, t.bl(8.0)) else { return Vec::new() };
        let slot = &mut self.desk.min_list.image;
        if slot.as_ref().is_none_or(|(l, _)| *l != look) {
            *slot = Some((look.clone(), draw(&look, &t, area).into_buffer()));
        }
        let px = crate::header::px_rect(area, t.scale);
        crate::deco::image(renderer, &slot.as_ref().unwrap().1, area, (px.w, px.h), t.scale).into_iter().collect()
    }
}

fn bounds(rects: &[Rect], m: i32) -> Option<Rect> {
    let (first, last) = (rects.first()?, rects.last()?);
    Some(Rect { x: first.x - m, y: first.y - m, w: first.w + 2 * m, h: last.y + last.h - first.y + 2 * m })
}

/// The letter square of a minimised window (the library's fallback icon, small).
pub fn letter(c: &mut Canvas, b: Box2, ch: char, t: &Theme) {
    c.rrect(b, b.w * 0.25, t.accent);
    let f = t.bfont(Family::Display, 11.0, Weight::SemiBold);
    c.text_mid(&f, Rect { x: b.x as i32, y: b.y as i32, w: b.w as i32, h: b.h as i32 }, &ch.to_string(), t.on_accent);
}

/// A minimised window's x (DESK-LEFT), as a workspace chip's: a soft disc behind it while the pointer is on it.
pub fn close_mark(c: &mut Canvas, xb: Box2, on_x: bool, t: &Theme) {
    let kk = t.board as f32;
    let (x, y) = (xb.x + xb.w / 2.0, xb.y + xb.h / 2.0);
    let ink = if on_x { t.text } else { t.muted };
    if on_x {
        c.disc(x, y, 10.0 * kk, alpha(ink, 0.22));
    }
    crate::chrome_draw::buehne::close_x(c, x, y, 4.5 * kk, 1.6 * kk, ink);
}

fn draw(l: &ListLook, t: &Theme, area: Rect) -> Canvas {
    let p = Px::new(area, t.scale);
    let (w, h) = p.size();
    let kk = t.board as f32;
    let mut c = Canvas::new(w, h);
    let all = Box2 { x: 0.0, y: 0.0, w: w as f32, h: h as f32 };
    c.rrect(all, 14.0 * kk, t.panel);
    c.rrect_line(all, 14.0 * kk, kk, alpha(t.accent, 0.5));
    let f = t.bfont(Family::Ui, 13.0, Weight::Medium);
    for (i, r, ch, name, xr) in &l.entries {
        let b = p.b(*r);
        let on = l.hover == Some(*i);
        if on {
            c.rrect(b, 10.0 * kk, t.panel2);
        }
        let side = 18.0 * kk;
        letter(&mut c, Box2 { x: b.x + 10.0 * kk, y: b.y + (b.h - side) / 2.0, w: side, h: side }, *ch, t);
        let pr = p.r(*r);
        let x = pr.x + (38.0 * kk) as i32;
        let end = if on { p.r(*xr).x - (6.0 * kk) as i32 } else { pr.x + pr.w - (10.0 * kk) as i32 };
        c.text_fit(&f, Rect { x, w: end - x, ..pr }, name, t.text);
        if on {
            close_mark(&mut c, p.b(*xr), l.on_x, t);
        }
    }
    c
}
