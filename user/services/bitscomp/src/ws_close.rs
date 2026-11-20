//! A (Georg 12.11.: "Arbeitsräume schließen gibt es auch noch kein x am Tab"): a workspace closes by the x on its
//! entry (the Bühne's chip: over its colour dot while the pointer is on the chip; classic: at the entry's right).
//! An empty workspace goes at once. One with windows asks first, with the pages' confirmation dialog drawn over the
//! whole screen: on "Schließen" its windows (tiles with their tabs, floats, minimised ones) move into the neighbour
//! workspace, which is then shown; nothing is closed. Never the only workspace, and not the sofa mode's own while
//! the sofa mode is on (its x is not there; `ws_remove` keeps that rule too).

use smithay::backend::renderer::element::memory::MemoryRenderBuffer;
use smithay::input::keyboard::Keysym;

use crate::desk::Spot;
use crate::layout::Rect;
use crate::page::Part;
use crate::page_confirm::{self, Dialog, Step};
use crate::state::App;

pub struct Ask {
    /// The workspace and its name when asked (a workspace closed or added meanwhile drops the question).
    pub ws: usize,
    pub name: String,
    pub dialog: Dialog,
    pub hover: Option<bool>,
    image: Option<((Dialog, Option<bool>, Rect, u64), MemoryRenderBuffer)>,
}

#[cfg(test)]
impl Ask {
    /// An open question about workspace `ws`, for tests of what may not open over it.
    pub fn test(ws: usize) -> Ask {
        Ask { ws, name: String::new(), dialog: page_confirm::plain("?".into(), Vec::new(), "Schließen"), hover: None, image: None }
    }
}

/// The neighbour that takes the windows: the one before, the second for the first.
pub fn neighbour(i: usize) -> usize {
    if i == 0 { 1 } else { i - 1 }
}

/// The question's lines in plain German.
pub fn question(name: &str, windows: usize, to: &str) -> (String, Vec<String>) {
    let moves = if windows == 1 { format!("Sein Fenster wandert in „{to}“.") } else { format!("Seine {windows} Fenster wandern in „{to}“.") };
    (format!("„{name}“ schließen?"), vec![moves, "Kein Fenster wird geschlossen, nichts geht verloren.".into()])
}

impl App {
    /// Whether workspace `i` can be closed: not the only one, not the sofa mode's own while it is on.
    pub fn ws_closable(&self, i: usize) -> bool {
        self.ws_count() >= 2 && i < self.ws_count() && !(self.sofa.active && self.desk.names[i] == crate::sofa::WORKSPACE)
    }

    /// The x of workspace `i`: empty goes at once, with windows the question opens.
    pub fn ws_close_press(&mut self, i: usize) {
        if !self.ws_closable(i) {
            // The x is not drawn there: the click is the chip's.
            return self.act(crate::wm::Action::Workspace(i));
        }
        let windows = self.ws_counts(i).1;
        if windows == 0 {
            let name = self.desk.names[i].clone();
            if self.ws_remove(i) {
                eprintln!("bitscomp: workspace {name} closed (empty)");
            }
            return;
        }
        let (name, to) = (self.desk.names[i].clone(), self.desk.names[neighbour(i)].clone());
        let (title, lines) = question(&name, windows, &to);
        eprintln!("bitscomp: workspace {name}: asked to close it ({windows} windows to {to})");
        self.desk.ws_ask = Some(Ask { ws: i, name, dialog: page_confirm::plain(title, lines, "Schließen"), hover: None, image: None });
        self.menu = None;
        self.address_end();
        self.needs_frame = true;
    }

    pub fn ws_ask_answer(&mut self, yes: bool) {
        let Some(a) = self.desk.ws_ask.take() else { return };
        self.needs_frame = true;
        if !yes {
            eprintln!("bitscomp: workspace {}: not closed (cancelled)", a.name);
            return;
        }
        if self.desk.names.get(a.ws) != Some(&a.name) {
            eprintln!("bitscomp: workspace {}: not closed (the workspaces changed meanwhile)", a.name);
            return;
        }
        self.ws_close_moving(a.ws);
    }

    /// Closes workspace `i`, its windows into the neighbour, which is shown; false when `i` may not close.
    pub fn ws_close_moving(&mut self, i: usize) -> bool {
        if !self.ws_closable(i) {
            return false;
        }
        let to = neighbour(i);
        let (name, to_name, windows) = (self.desk.names[i].clone(), self.desk.names[to].clone(), self.ws_counts(i).1);
        // Show the neighbour first: then `i` is a parked workspace whose windows are unmapped.
        self.ws_switch(to);
        let moved = std::mem::take(&mut self.desk.parked[i]);
        self.ws.absorb(moved);
        self.desk.names.remove(i);
        self.desk.parked.remove(i);
        if self.desk.active > i {
            self.desk.active -= 1;
        }
        self.layout_dirty = true;
        self.chrome_dirty();
        self.session_dirty();
        eprintln!("bitscomp: workspace {name} closed: {windows} windows moved to {to_name}");
        true
    }

    /// While the question is open: its buttons, and everything else as a modal nothing.
    pub fn ws_ask_spot(&self, x: f64, y: f64) -> Option<Spot> {
        let a = self.desk.ws_ask.as_ref()?;
        let (_, cancel, ok) = page_confirm::boxes(self.area_full(), a.dialog.lines.len());
        Some(Spot::WsAsk(if ok.contains(x, y) {
            Some(true)
        } else if cancel.contains(x, y) {
            Some(false)
        } else {
            None
        }))
    }

    /// A key while the question is open (Esc cancels, Tab and the arrows move, Enter answers).
    pub fn ws_ask_key(&mut self, sym: Keysym) {
        let Some(a) = self.desk.ws_ask.as_mut() else { return };
        match a.dialog.key(sym) {
            Step::Stay => self.needs_frame = true,
            Step::Cancel => self.ws_ask_answer(false),
            Step::Confirm => self.ws_ask_answer(true),
        }
    }

    pub fn ws_ask_motion(&mut self) {
        let hover = match self.ws_ask_spot(self.pointer.x, self.pointer.y) {
            Some(Spot::WsAsk(h)) => h,
            _ => None,
        };
        if let Some(a) = self.desk.ws_ask.as_mut()
            && a.hover != hover
        {
            a.hover = hover;
            self.needs_frame = true;
        }
    }

    /// `ctl hot`'s lines of the question.
    pub fn ws_ask_hot(&self) -> String {
        let Some(a) = self.desk.ws_ask.as_ref() else { return String::new() };
        let s = self.theme.scale;
        let pt = |r: Rect| format!("{},{}", ((r.x as f64 + r.w as f64 / 2.0) * s).round(), ((r.y as f64 + r.h as f64 / 2.0) * s).round());
        let (_, cancel, ok) = page_confirm::boxes(self.area_full(), a.dialog.lines.len());
        format!(
            "wsask title={:?} line={:?} cancel={} ok={} focus={}\n",
            a.dialog.title,
            a.dialog.lines[0],
            pt(cancel),
            pt(ok),
            if a.dialog.on_ok { "ok" } else { "cancel" }
        )
    }

    /// The question over the whole screen (topmost first: one image).
    pub fn ws_ask_elements<R: crate::render::CompRenderer>(&mut self, renderer: &mut R) -> Vec<crate::render::CompElement<R>> {
        let full = self.area_full();
        let (t, tg) = (self.theme.clone(), self.theme_gen);
        let Some(a) = self.desk.ws_ask.as_mut() else { return Vec::new() };
        let look = (a.dialog.clone(), a.hover, full, tg);
        if a.image.as_ref().is_none_or(|(l, _)| *l != look) {
            let p = crate::chrome_draw::Px::new(full, t.scale);
            let (w, h) = p.size();
            let mut c = crate::canvas::Canvas::new(w, h);
            let hover = a.hover.map(|ok| if ok { Part::DialogOk } else { Part::DialogCancel });
            page_confirm::draw(&mut c, &p, &t, full, &a.dialog, hover);
            a.image = Some((look, c.into_buffer()));
        }
        let buf = &a.image.as_ref().unwrap().1;
        let px = crate::header::px_rect(full, t.scale);
        crate::deco::image(renderer, buf, full, (px.w, px.h), t.scale).into_iter().collect()
    }
}

#[cfg(test)]
#[path = "ws_close_tests.rs"]
mod tests;
