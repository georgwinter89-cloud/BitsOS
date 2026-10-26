//! The chrome of Main.dc.html (WY-5) as pure geometry in logical pixels: the top bar (mark, back/forward, the
//! address bar, the ring button that folds Claude's sidebar), the favourites bar, the workspace list on the left,
//! Claude's sidebar on the right, the status bar. What is left in the middle is the work area for the tiles (or
//! the start page). Drawing (`chrome_draw`), hit testing (`hit`) and `ctl hot` all read this one layout.
//! WY-5c: both side bars fold away completely (theme.layout.workspaces_bar, theme.layout.claude_sidebar); a folded
//! one can be revealed over the tiles (`reveal`), and the folded list leaves a small button in the top bar.

use crate::layout::Rect;
use crate::reveal::Edge;
use crate::text::{Font, Weight};
use crate::theme::Theme;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Part {
    Back,
    Forward,
    Address,
    Ring,
    Fav(usize),
    Ws(usize),
    WsNew,
    Home,
    /// WY-5c: the list's fold button (on a revealed list it pins it).
    WsFold,
    /// WY-5c: the top bar's workspaces button while the list is folded (it brings the list back).
    WsButton,
}

/// A workspace entry with its second line ("2 Kacheln · 3 Tabs"), and the lowest one (name only).
pub const WS_FULL: i32 = 58;
pub const WS_LOW: i32 = 30;

/// What the layout depends on besides the theme.
#[derive(Clone, Debug, PartialEq)]
pub struct Inputs {
    /// The output in logical pixels.
    pub size: (i32, i32),
    pub favourites: Vec<String>,
    pub workspaces: usize,
    /// A folded bar shown over the tiles for a while (pointer against its edge).
    pub reveal: Option<Edge>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Chrome {
    pub top: Rect,
    pub mark: Rect,
    pub back: Rect,
    pub forward: Rect,
    pub address: Rect,
    pub ring: Rect,
    pub favs: Option<Rect>,
    pub fav_items: Vec<Rect>,
    /// The list's place; it is there only when `list_shown`.
    pub list: Rect,
    pub list_shown: bool,
    /// Docked: the list takes its width from the tiles; else it is folded (or revealed over the tiles).
    pub list_docked: bool,
    pub ws_fold: Rect,
    pub ws_button: Option<Rect>,
    pub ws_title: Rect,
    pub ws_items: Vec<Rect>,
    pub ws_new: Rect,
    pub home: Rect,
    /// Claude's sidebar when shown (docked or revealed).
    pub side: Option<Rect>,
    pub side_docked: bool,
    pub status: Option<Rect>,
    /// Between all bars: tiles or the start page.
    pub work: Rect,
}

/// The favourites bar's label of an address (the address itself, as in the mock-up).
pub fn fav_label(addr: &str) -> &str {
    addr
}

/// The favourites bar's text style.
pub fn fav_font(t: &Theme) -> Font {
    Font::new(t.fs(13.0), Weight::Regular, t.scale)
}

/// Logical width of `s` in `f` (measured at the output scale, so it matches what is drawn).
pub fn logical_width(f: &Font, s: &str, scale: f64) -> i32 {
    (f.width(s) as f64 / scale).ceil() as i32
}

pub fn layout(t: &Theme, i: &Inputs) -> Chrome {
    let (w, h) = (i.size.0.max(1), i.size.1.max(1));
    let top = Rect { x: 0, y: 0, w, h: t.top_h };
    let mid = |hh: i32| top.y + (top.h - hh) / 2;
    let (list_docked, side_docked) = (t.layout.workspaces, t.layout.sidebar);
    let ws_button = (!list_docked).then(|| Rect { x: 16, y: mid(36), w: 36, h: 36 });
    let mark = Rect { x: ws_button.map_or(16, |b| b.x + b.w + 12), y: mid(26), w: 184, h: 26 };
    let back = Rect { x: mark.x + mark.w + 12, y: mid(36), w: 36, h: 36 };
    let forward = Rect { x: back.x + back.w + 12, ..back };
    let ring = Rect { x: w - 16 - 40, y: mid(40), w: 40, h: 40 };
    let ax = forward.x + forward.w + 12;
    let address = Rect { x: ax, y: mid(40), w: (ring.x - 12 - ax).max(40), h: 40 };
    let mut y = top.h;
    let (favs, fav_items) = if t.layout.favourites {
        let bar = Rect { x: 0, y, w, h: t.fav_h };
        let f = fav_font(t);
        let mut x = if list_docked { t.list_w + 16 } else { 16 };
        let mut items = Vec::new();
        for a in &i.favourites {
            let iw = logical_width(&f, fav_label(a), t.scale) + 20;
            if x + iw > w - 16 {
                break;
            }
            items.push(Rect { x, y: y + (t.fav_h - 26) / 2, w: iw, h: 26 });
            x += iw + 6;
        }
        y += t.fav_h;
        (Some(bar), items)
    } else {
        (None, Vec::new())
    };
    let status = t.layout.statusbar.then(|| Rect { x: 0, y: h - t.status_h, w, h: t.status_h });
    let bottom = status.map_or(h, |s| s.y);
    let list = Rect { x: 0, y, w: t.list_w.min(w / 3), h: (bottom - y).max(1) };
    let ws_fold = Rect { x: list.x + list.w - 12 - 28, y: list.y + 12, w: 28, h: 28 };
    let ws_title = Rect { x: list.x + 20, y: list.y + 16, w: ws_fold.x - 8 - list.x - 20, h: 20 };
    let item_w = list.w - 24;
    let home = Rect { x: list.x + 12, y: list.y + list.h - 16 - 40, w: item_w, h: 40 };
    let mut iy = ws_title.y + ws_title.h + 6;
    // Many workspaces: the entries get lower (name only below `WS_FULL`) until they fit; what still does not fit
    // is reached by Ctrl+Alt+digit and `ctl ws` (`ws_shown`).
    let room = home.y - 6 - 52 - iy;
    let n = i.workspaces.max(1) as i32;
    let item_h = (room / n - 6).clamp(WS_LOW, WS_FULL);
    let mut ws_items = Vec::new();
    for _ in 0..i.workspaces {
        ws_items.push(Rect { x: list.x + 12, y: iy, w: item_w, h: item_h });
        iy += item_h + 6;
    }
    let ws_new = Rect { x: list.x + 12, y: iy + 6, w: item_w, h: 40 };
    let side_rect = Rect { x: w - t.side_w.min(w / 3), y, w: t.side_w.min(w / 3), h: list.h };
    let side = (side_docked || i.reveal == Some(Edge::Right)).then_some(side_rect);
    let left = if list_docked { list.x + list.w } else { 0 };
    let right = if side_docked { side_rect.x } else { w };
    let work = Rect { x: left, y, w: (right - left).max(1), h: list.h };
    Chrome {
        top,
        mark,
        back,
        forward,
        address,
        ring,
        favs,
        fav_items,
        list,
        list_shown: list_docked || i.reveal == Some(Edge::Left),
        list_docked,
        ws_fold,
        ws_button,
        ws_title,
        ws_items,
        ws_new,
        home,
        side,
        side_docked,
        status,
        work,
    }
}

impl Chrome {
    /// The chrome part under (x, y); `None` over the work area or empty chrome.
    pub fn hit(&self, x: f64, y: f64) -> Option<Part> {
        let parts = [(self.back, Part::Back), (self.forward, Part::Forward), (self.address, Part::Address), (self.ring, Part::Ring)];
        if let Some((_, p)) = parts.iter().find(|(r, _)| r.contains(x, y)) {
            return Some(*p);
        }
        if self.ws_button.is_some_and(|b| b.contains(x, y)) {
            return Some(Part::WsButton);
        }
        if let Some(i) = self.fav_items.iter().position(|r| r.contains(x, y)) {
            return Some(Part::Fav(i));
        }
        if !self.list_shown {
            return None;
        }
        if self.ws_fold.contains(x, y) {
            return Some(Part::WsFold);
        }
        if let Some(i) = self.ws_items[..self.ws_shown()].iter().position(|r| r.contains(x, y)) {
            return Some(Part::Ws(i));
        }
        // The "+" button can be pushed below the list's end by many workspaces: then it is not there.
        if self.ws_new.contains(x, y) && self.ws_new.y + self.ws_new.h <= self.home.y {
            return Some(Part::WsNew);
        }
        if self.home.contains(x, y) {
            return Some(Part::Home);
        }
        None
    }

    /// Whether (x, y) is on any bar (and so not on the work area); a revealed bar lies over the work area.
    pub fn covers(&self, x: f64, y: f64) -> bool {
        !self.work.contains(x, y) || self.overlay().is_some_and(|r| r.contains(x, y))
    }

    /// The bar revealed over the tiles, if any.
    pub fn overlay(&self) -> Option<Rect> {
        if self.list_shown && !self.list_docked {
            return Some(self.list);
        }
        self.side.filter(|_| !self.side_docked)
    }

    /// The folded bar whose screen edge (x, y) is against: the outermost logical pixel column beside the bar's
    /// place.
    pub fn edge_at(&self, x: f64, y: f64) -> Option<Edge> {
        let rows = |r: Rect| y >= r.y as f64 && y < (r.y + r.h) as f64;
        if !self.list_docked && x < 1.0 && rows(self.list) {
            return Some(Edge::Left);
        }
        if !self.side_docked && x >= (self.top.w - 1) as f64 && rows(self.list) {
            return Some(Edge::Right);
        }
        None
    }

    /// Workspace entries that fit above the "Startseite" link.
    pub fn ws_shown(&self) -> usize {
        self.ws_items.iter().take_while(|r| r.y + r.h <= self.home.y - 6).count()
    }
}

#[cfg(test)]
#[path = "chrome_tests.rs"]
mod tests;
