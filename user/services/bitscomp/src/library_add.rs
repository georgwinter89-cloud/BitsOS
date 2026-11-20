//! LIB-ADD (Georg 12.11.: "Es gibt noch keinen Button, um die Programmbibliothek aufzurufen, um Programme zu
//! installieren"): the library overlay's ways to the "Programme" page. "Programme hinzufügen" and the quieter
//! "Aktualisierungen" sit in the head beside the search field; a search that finds nothing shows "Nicht dabei?
//! Programme hinzufügen", which hands the typed text to the page's search (`programme/hinzufuegen/<text>`). Each
//! opens its address as the address bar does. Keys: Up from the first line, Tab past the last row and Shift+Tab
//! before the first reach the head; Left/Right walk it, Down goes back to the tiles, Enter opens. Opening a page
//! changes nothing, so injected input may do it. The overlay asks nothing itself: the count of updates is the
//! programme page's last check, shown only when a page found some.

use crate::address::Where;
use crate::library_view::{self as view, Button, Move};
use crate::state::App;

impl App {
    /// The updates the programme page's last check found (none asked here: the overlay sends no request).
    pub fn library_updates_count(&self) -> Option<usize> {
        let n = self.prog.updates.len();
        (n > 0).then_some(n)
    }

    pub fn library_head(&self) -> view::Head {
        view::head(&self.library_geo(), &self.theme, self.library_updates_count())
    }

    /// Whether the body shows the empty search (its link instead of tiles).
    pub fn library_empty(&self) -> bool {
        !self.lib.query.trim().is_empty() && self.library_rows().is_empty()
    }

    pub(crate) fn library_last_head(&self) -> Button {
        if self.library_head().updates.is_some() { Button::Updates } else { Button::Add }
    }

    pub(crate) fn library_focus_button(&mut self, b: Option<Button>) {
        self.lib.button = match b {
            Some(Button::Updates) if self.library_head().updates.is_none() => Some(Button::Add),
            b => b,
        };
        self.needs_frame = true;
    }

    /// The first tile of row `r` (the last row for None), scrolled into view; the empty search's link instead.
    fn library_focus_row(&mut self, r: Option<usize>) {
        let rows = self.library_rows();
        if rows.is_empty() {
            let link = self.library_empty().then_some(Button::AddSearch);
            return self.library_focus_button(link.or(self.lib.button));
        }
        let r = r.unwrap_or(rows.len() - 1).min(rows.len() - 1);
        self.lib.button = None;
        self.lib.focus = Some((r, rows[r].ids[0].clone()));
        let g = self.library_geo();
        let lens: Vec<usize> = rows.iter().map(|r| r.ids.len()).collect();
        let (placed, content) = view::place(&g, &lens);
        self.lib.scroll = view::scroll_to(self.lib.scroll, placed[r].1[0], Some(placed[r].0), g.body.h, content);
        self.needs_frame = true;
    }

    pub(crate) fn library_button_move(&mut self, b: Button, m: Move) {
        let updates = self.library_head().updates.is_some();
        match (b, m) {
            (Button::Add, Move::Right) if updates => self.library_focus_button(Some(Button::Updates)),
            (Button::Updates, Move::Left | Move::PrevRow) => self.library_focus_button(Some(Button::Add)),
            (Button::Add, Move::NextRow) if updates => self.library_focus_button(Some(Button::Updates)),
            (Button::Add | Button::Updates, Move::Down | Move::NextRow) => self.library_focus_row(Some(0)),
            (Button::Add, Move::PrevRow) => self.library_focus_row(None),
            (Button::AddSearch, Move::Up | Move::NextRow) => self.library_focus_button(Some(Button::Add)),
            (Button::AddSearch, Move::PrevRow) => {
                let last = self.library_last_head();
                self.library_focus_button(Some(last))
            }
            _ => {}
        }
    }

    /// Opens the button's page the way a typed address does; the overlay closes first.
    pub fn library_button(&mut self, b: Button) {
        let addr = b.address(&self.lib.query);
        eprintln!("bitscomp: library: {} opens {addr}", b.name());
        self.library_close("programme page");
        self.open_address(&addr, Where::Tab, None);
    }

    pub(crate) fn library_button_at(&self, x: f64, y: f64) -> Option<Button> {
        let h = self.library_head();
        if h.add.contains(x, y) {
            return Some(Button::Add);
        }
        if h.updates.is_some_and(|r| r.contains(x, y)) {
            return Some(Button::Updates);
        }
        (self.library_empty() && view::empty(&self.library_geo(), &self.theme).1.contains(x, y)).then_some(Button::AddSearch)
    }

    /// `ctl hot`'s lines of the open overlay: `library add`, `library updates` (with its count), and while a search
    /// finds nothing `library add-search` with the text it hands on.
    pub fn library_hot(&self) -> String {
        if !self.lib.open {
            return String::new();
        }
        let s = self.theme.scale;
        let pt = |r: crate::layout::Rect| format!("{},{}", ((r.x as f64 + r.w as f64 / 2.0) * s).round(), ((r.y as f64 + r.h as f64 / 2.0) * s).round());
        let focus = |b: Button| self.lib.button == Some(b);
        let h = self.library_head();
        let mut out = format!("library add at={} focus={}\n", pt(h.add), focus(Button::Add));
        if let Some(r) = h.updates {
            let count = self.library_updates_count().unwrap_or(0);
            out.push_str(&format!("library updates at={} count={count} focus={}\n", pt(r), focus(Button::Updates)));
        }
        if self.library_empty() {
            let link = view::empty(&self.library_geo(), &self.theme).1;
            out.push_str(&format!("library add-search at={} query={:?} focus={}\n", pt(link), self.lib.query.trim(), focus(Button::AddSearch)));
        }
        out
    }
}

#[cfg(test)]
#[path = "library_add_tests.rs"]
mod tests;
