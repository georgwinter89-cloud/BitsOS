//! Workspace layout (plan/spec/desktop.md §1): tiles side by side, a tile holds a stack of windows of which
//! one is shown ("wie ein Buch blättern"), single windows may float above the tiles.
//! WY-5c: floating windows are the classic alternative: above everything, anywhere on the screen as long as
//! `REACH` of their header stays on it, maximised to the whole screen and back.
//! Generic over the window type so the rules are testable without a Wayland display.

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.x as f64 && y >= self.y as f64 && x < (self.x + self.w) as f64 && y < (self.y + self.h) as f64
    }
}

pub struct Tile<W> {
    pub stack: Vec<W>,
    pub top: usize,
    pub weight: f32,
}

pub struct Floating<W> {
    pub win: W,
    pub rect: Rect,
    /// Maximised (WY-5c): `rect` is the whole screen, this is where it goes back to.
    pub restore: Option<Rect>,
}

/// Logical pixels of a floating window's header that always stay on the screen, so it can be grabbed again.
pub const REACH: i32 = 48;

/// `r` moved (never resized) so that at least `REACH` of its header row (`header` high) lies on `full`: the header
/// may not leave at the top or the bottom, and the window may hang out at the sides up to `REACH`.
pub fn clamp_float(r: Rect, full: Rect, header: i32) -> Rect {
    // i64: a client's sizes or a far pointer must not overflow into a min > max clamp (that panics).
    let (rx, ry, rw) = (r.x as i64, r.y as i64, r.w as i64);
    let (fx, fy, fw, fh, hd) = (full.x as i64, full.y as i64, full.w as i64, full.h as i64, header as i64);
    let reach = (REACH as i64).min(rw).min(fw);
    // reach <= rw and reach <= fw, so the lower bound <= fx <= the upper bound.
    let x = rx.clamp(fx + reach - rw, fx + fw - reach);
    let y = ry.clamp(fy, (fy + fh - hd.min(fh)).max(fy));
    Rect { x: sat(x), y: sat(y), ..r }
}

fn sat(v: i64) -> i32 {
    v.clamp(i32::MIN as i64, i32::MAX as i64) as i32
}

/// A resized floating frame cut back so its header stays reachable: the top edge never above the screen, and
/// the same horizontal reach as `clamp_float` (the opposite edge stays where it was).
pub fn clamp_resized(r: Rect, full: Rect, header: i32) -> Rect {
    let (mut x, mut y, mut w, mut h) = (r.x as i64, r.y as i64, r.w as i64, r.h as i64);
    let (fx, fy, fw, fh, hd) = (full.x as i64, full.y as i64, full.w as i64, full.h as i64, header as i64);
    if y < fy {
        h -= fy - y;
        y = fy;
    }
    let bottom = fy + fh - hd.min(fh);
    if y > bottom {
        y = bottom;
    }
    let reach = (REACH as i64).min(fw);
    let max = i32::MAX as i64;
    if x + w < fx + reach {
        // The right edge is the reachable one: it stays at `reach` even where the width no longer fits.
        let right = fx + reach;
        w = (right - x).min(max);
        x = right - w;
    }
    if x > fx + fw - reach {
        let right = x + w;
        x = fx + fw - reach;
        w = (right - x).min(max);
    }
    Rect { x: sat(x), y: sat(y), w: sat(w), h: sat(h) }
}

/// Where a maximised window goes when its header is dragged at (px, py): its old size, under the pointer at the
/// same share of its width, the header row where the pointer holds it (`grab_y` below the top).
pub fn restored_at(restore: Rect, full: Rect, px: f64, py: f64, grab_y: f64) -> Rect {
    let share = ((px - full.x as f64) / full.w.max(1) as f64).clamp(0.0, 1.0);
    let x = (px - share * restore.w as f64).round() as i32;
    let y = (py - grab_y).round() as i32;
    Rect { x, y, ..restore }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    None,
    Tile(usize),
    Float(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Place {
    Tiled { tile: usize, index: usize, depth: usize, top: usize },
    Floating { index: usize },
}

pub struct Placement<W> {
    pub win: W,
    pub frame: Rect,
    pub visible: bool,
    pub focused: bool,
    pub place: Place,
    /// A float maximised to the screen (WY-5c).
    pub maximised: bool,
    /// WY-8: the window covers the whole screen above everything; `frame` is the screen then.
    pub fullscreen: bool,
}

pub struct Workspace<W> {
    pub tiles: Vec<Tile<W>>,
    pub floating: Vec<Floating<W>>,
    pub focus: Focus,
    /// WY-8: the window the client put into fullscreen; its tile or float stays as it was, so leaving restores it.
    pub fullscreen: Option<W>,
}

impl<W: Clone + PartialEq> Default for Workspace<W> {
    fn default() -> Self {
        Self { tiles: Vec::new(), floating: Vec::new(), focus: Focus::None, fullscreen: None }
    }
}

impl<W: Clone + PartialEq> Workspace<W> {
    pub fn add(&mut self, w: W) {
        self.tiles.push(Tile { stack: vec![w], top: 0, weight: 1.0 });
        self.focus = Focus::Tile(self.tiles.len() - 1);
    }

    pub fn find(&self, w: &W) -> Option<Place> {
        for (t, tile) in self.tiles.iter().enumerate() {
            if let Some(i) = tile.stack.iter().position(|x| x == w) {
                return Some(Place::Tiled { tile: t, index: i, depth: tile.stack.len(), top: tile.top });
            }
        }
        self.floating.iter().position(|f| &f.win == w).map(|index| Place::Floating { index })
    }

    pub fn focused(&self) -> Option<&W> {
        match self.focus {
            Focus::Tile(t) => self.tiles.get(t).map(|tile| &tile.stack[tile.top]),
            Focus::Float(i) => self.floating.get(i).map(|f| &f.win),
            Focus::None => None,
        }
    }

    pub fn remove(&mut self, w: &W) {
        self.take(w);
    }

    /// Removes a window from a tile; drops the tile when its stack gets empty and keeps the focus valid.
    fn take_from_tile(&mut self, t: usize, i: usize) -> W {
        let tile = &mut self.tiles[t];
        let w = tile.stack.remove(i);
        if i < tile.top || tile.top >= tile.stack.len() {
            tile.top = tile.top.saturating_sub(1);
        }
        if tile.stack.is_empty() {
            self.tiles.remove(t);
            self.focus = match self.focus {
                Focus::Tile(f) if f == t => self.fallback_focus_near(t),
                Focus::Tile(f) if f > t => Focus::Tile(f - 1),
                f => f,
            };
        }
        w
    }

    fn fallback_focus_near(&self, t: usize) -> Focus {
        if self.tiles.is_empty() {
            return self.fallback_focus();
        }
        Focus::Tile(t.min(self.tiles.len() - 1))
    }

    fn fallback_focus(&self) -> Focus {
        if !self.floating.is_empty() {
            Focus::Float(self.floating.len() - 1)
        } else if !self.tiles.is_empty() {
            Focus::Tile(self.tiles.len() - 1)
        } else {
            Focus::None
        }
    }

    /// Focusing a window in a stack turns the stack to it; focusing a floating window raises it.
    pub fn focus_window(&mut self, w: &W) {
        match self.find(w) {
            Some(Place::Tiled { tile, index, .. }) => {
                self.tiles[tile].top = index;
                self.focus = Focus::Tile(tile);
            }
            Some(Place::Floating { index }) => {
                let f = self.floating.remove(index);
                self.floating.push(f);
                self.focus = Focus::Float(self.floating.len() - 1);
            }
            None => {}
        }
    }

    /// A tiled window floats, centred over the tiles (`area`; moved down and right by 24 while another floating
    /// window has exactly that place, so none hides another); a floating one goes back into a tile.
    pub fn toggle_float(&mut self, w: &W, area: Rect) {
        match self.find(w) {
            Some(Place::Tiled { tile, index, .. }) => {
                let win = self.take_from_tile(tile, index);
                let (fw, fh) = (area.w * 3 / 5, area.h * 3 / 5);
                let mut rect = Rect { x: area.x + (area.w - fw) / 2, y: area.y + (area.h - fh) / 2, w: fw, h: fh };
                // Each step moves past one float, so this ends after at most as many steps as there are floats.
                while self.floating.iter().any(|f| f.rect.x == rect.x && f.rect.y == rect.y) {
                    rect.x += 24;
                    rect.y += 24;
                }
                self.floating.push(Floating { win, rect, restore: None });
                self.focus = Focus::Float(self.floating.len() - 1);
            }
            Some(Place::Floating { index }) => {
                let f = self.floating.remove(index);
                self.add(f.win);
            }
            None => {}
        }
    }

    /// Maximises the floating window `w` to `full` or gives it back its old place; false if it does not float.
    pub fn toggle_max(&mut self, w: &W, full: Rect) -> bool {
        let Some(Place::Floating { index }) = self.find(w) else { return false };
        let f = &mut self.floating[index];
        match f.restore.take() {
            Some(old) => f.rect = old,
            None => {
                f.restore = Some(f.rect);
                f.rect = full;
            }
        }
        true
    }

    pub fn is_max(&self, w: &W) -> bool {
        matches!(self.find(w), Some(Place::Floating { index }) if self.floating[index].restore.is_some())
    }

    /// WY-8: the client's own fullscreen on or off; returns whether it changed. One per workspace (a second one
    /// takes over, as in sway). Only the focused window may go fullscreen: the user's click or key went to it (a
    /// browser asks only on one), and a window in the background cannot take the screen and the keys unasked.
    pub fn set_fullscreen(&mut self, w: &W, on: bool) -> bool {
        if !on {
            return self.fullscreen.take_if(|f| f == w).is_some();
        }
        if self.focused() != Some(w) {
            return false;
        }
        self.fullscreen.replace(w.clone()).as_ref() != Some(w)
    }

    /// WY-8: a new window as a tile of its own; it takes the focus unless a fullscreen window is shown, which then
    /// keeps the screen (as in sway: a window opened under a fullscreen one is not focused; a playing video is not
    /// ended by a background program's window).
    pub fn add_behind(&mut self, w: W) {
        let keep = self.fullscreen_shown().is_some().then_some(self.focus);
        self.add(w);
        if let Some(f) = keep {
            self.focus = f;
        }
    }

    pub fn is_fullscreen(&self, w: &W) -> bool {
        self.fullscreen.as_ref() == Some(w)
    }

    /// The window that covers the screen: fullscreen and focused.
    pub fn fullscreen_shown(&self) -> Option<&W> {
        self.fullscreen.as_ref().filter(|f| self.focused() == Some(*f))
    }

    /// Ends a fullscreen whose window lost the focus (another one was focused or opened, its stack was turned):
    /// a window nobody sees keeps no fullscreen, and coming back to it later does not cover the screen unasked.
    pub fn settle_fullscreen(&mut self) -> Option<W> {
        if self.fullscreen_shown().is_none() { self.fullscreen.take() } else { None }
    }

    /// After the screen changed size (or a float was placed by a client): maximised windows cover `full` again,
    /// the others keep their header reachable.
    pub fn fit_floats(&mut self, full: Rect, header: i32) -> bool {
        let mut changed = false;
        for f in &mut self.floating {
            let r = if f.restore.is_some() { full } else { clamp_float(f.rect, full, header) };
            changed |= r != f.rect;
            f.rect = r;
        }
        changed
    }

    /// Puts the focused tiled window on top of the neighbouring tile's stack (left one, else right one).
    pub fn stack_focused(&mut self) -> bool {
        let Focus::Tile(t) = self.focus else { return false };
        if self.tiles.len() < 2 {
            return false;
        }
        let target = if t > 0 { t - 1 } else { t + 1 };
        let top = self.tiles[t].top;
        let single = self.tiles[t].stack.len() == 1;
        let w = self.take_from_tile(t, top);
        let target = if single && target > t { target - 1 } else { target };
        let tile = &mut self.tiles[target];
        tile.stack.push(w);
        tile.top = tile.stack.len() - 1;
        self.focus = Focus::Tile(target);
        true
    }

    /// Takes the shown window of the focused stack out into its own tile right of it.
    pub fn unstack_focused(&mut self) -> bool {
        let Focus::Tile(t) = self.focus else { return false };
        if self.tiles[t].stack.len() < 2 {
            return false;
        }
        let top = self.tiles[t].top;
        let w = self.take_from_tile(t, top);
        self.tiles.insert(t + 1, Tile { stack: vec![w], top: 0, weight: 1.0 });
        self.focus = Focus::Tile(t + 1);
        true
    }

    pub fn flip(&mut self, t: usize, dir: i32) -> bool {
        let Some(tile) = self.tiles.get_mut(t) else { return false };
        let n = tile.stack.len() as i32;
        if n < 2 {
            return false;
        }
        tile.top = (tile.top as i32 + dir).rem_euclid(n) as usize;
        true
    }

    pub fn focus_step(&mut self, dir: i32) {
        let n = (self.tiles.len() + self.floating.len()) as i32;
        if n == 0 {
            return;
        }
        let cur = match self.focus {
            Focus::Tile(t) => t as i32,
            Focus::Float(i) => (self.tiles.len() + i) as i32,
            Focus::None => -1,
        };
        let next = (cur + dir).rem_euclid(n) as usize;
        self.focus = if next < self.tiles.len() { Focus::Tile(next) } else { Focus::Float(next - self.tiles.len()) };
    }

    pub fn grow_focused(&mut self, delta: f32) -> bool {
        let Focus::Tile(t) = self.focus else { return false };
        let tile = &mut self.tiles[t];
        // Gap dragging can leave weights far from 1; the wide clamp keeps keys working from there.
        tile.weight = (tile.weight + delta).clamp(0.1, 10.0);
        true
    }

    /// Frames in stacking order, bottom first. Hidden stack members get their tile's frame so a flip
    /// shows them at once without a new configure round trip. WY-8: a shown fullscreen window comes last with
    /// `full` as its frame, every other window is hidden under it (and keeps its frame for the way back).
    pub fn arrange(&self, area: Rect, full: Rect, gap: i32) -> Vec<Placement<W>> {
        let mut out = Vec::new();
        for ((t, tile), frame) in self.tiles.iter().enumerate().zip(self.tile_frames(area, gap)) {
            for (i, win) in tile.stack.iter().enumerate() {
                out.push(Placement {
                    win: win.clone(),
                    frame,
                    visible: i == tile.top,
                    focused: i == tile.top && self.focus == Focus::Tile(t),
                    place: Place::Tiled { tile: t, index: i, depth: tile.stack.len(), top: tile.top },
                    maximised: false,
                    fullscreen: false,
                });
            }
        }
        // Floats in their raise order, the focused one on top whichever way it got the focus (keys cycle the focus
        // without changing the order, so a full cycle visits every window).
        let focused = match self.focus {
            Focus::Float(i) => Some(i),
            _ => None,
        };
        let order = (0..self.floating.len()).filter(|i| Some(*i) != focused).chain(focused.filter(|i| *i < self.floating.len()));
        for i in order {
            let f = &self.floating[i];
            out.push(Placement {
                win: f.win.clone(),
                frame: f.rect,
                visible: true,
                focused: self.focus == Focus::Float(i),
                place: Place::Floating { index: i },
                maximised: f.restore.is_some(),
                fullscreen: false,
            });
        }
        if let Some(fs) = self.fullscreen_shown() {
            for p in &mut out {
                p.fullscreen = p.win == *fs;
                p.visible = p.fullscreen;
                if p.fullscreen {
                    p.frame = full;
                }
            }
            if let Some(i) = out.iter().position(|p| p.fullscreen) {
                let p = out.remove(i);
                out.push(p);
            }
        }
        out
    }
}

#[path = "layout_move.rs"]
mod moves;

#[cfg(test)]
#[path = "layout_tests.rs"]
mod tests;
