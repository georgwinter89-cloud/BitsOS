//! Opening the circle menu and picking from it (the geometry is in `menu`).

use smithay::desktop::Window;

use crate::grab::Grab;
use crate::layout::Place;
use crate::menu::{self, Menu, MenuHit};
use crate::state::App;

impl App {
    pub(crate) fn open_menu(&mut self, target: Option<Window>) {
        let items = match &target {
            Some(w) => {
                let floating = matches!(self.ws.find(w), Some(Place::Floating { .. }));
                let depth = match self.ws.find(w) {
                    Some(Place::Tiled { depth, .. }) => depth,
                    _ => 0,
                };
                let maximised = floating.then(|| self.ws.is_max(w));
                menu::window_items(floating, maximised, !floating && self.ws.tiles.len() > 1, depth > 1)
            }
            None => menu::empty_items(self.ws_count() > 1 && self.ws_counts(self.desk.active) == (0, 0)),
        };
        let centre = menu::clamp_centre(self.pointer.x, self.pointer.y, self.area(), &self.theme);
        self.anim.menu_opened(crate::pacer::now());
        let opened_at = (self.pointer.x, self.pointer.y);
        self.menu = Some(Menu { centre, target, items, hover: None, held: true, opened_at });
        self.hover = None;
        self.needs_frame = true;
        self.motion(self.now_ms());
    }

    fn menu_pick(&mut self) -> MenuHit {
        let m = self.menu.as_ref().unwrap();
        menu::hit(m.centre, &m.items, &self.theme, self.pointer.x, self.pointer.y)
    }

    fn run_menu_item(&mut self, d: usize) {
        let Some(m) = self.menu.take() else { return };
        match m.items[d] {
            Some(item) if item.enabled => self.act_on(m.target.as_ref(), item.action),
            // A greyed-out entry does nothing and leaves the menu open.
            _ => self.menu = Some(m),
        }
        self.needs_frame = true;
    }

    pub(crate) fn menu_press(&mut self) {
        match self.menu_pick() {
            MenuHit::Item(d) => self.run_menu_item(d),
            MenuHit::Centre => {}
            MenuHit::Outside => {
                self.menu = None;
                self.needs_frame = true;
            }
        }
    }

    /// Releasing the opening right button picks the item under it; released where it opened, the menu stays
    /// open for a click.
    pub(crate) fn menu_release(&mut self) {
        let (p, threshold) = (self.pointer, self.theme.drag_threshold);
        let m = self.menu.as_mut().unwrap();
        if !m.held {
            return;
        }
        m.held = false;
        if (p.x - m.opened_at.0).hypot(p.y - m.opened_at.1) >= threshold {
            self.menu_press();
        }
    }

    /// Esc: closes the menu or drops a header drag. Returns whether there was one.
    pub fn cancel_pointer_modes(&mut self) -> bool {
        if self.menu.take().is_some() {
            self.needs_frame = true;
            return true;
        }
        if let Some(Grab::MoveTile { .. }) = self.grab {
            self.grab = None;
            self.needs_frame = true;
            return true;
        }
        false
    }
}
