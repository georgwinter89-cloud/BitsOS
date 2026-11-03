//! WY-12: minimised windows. xdg-shell's set_minimized is a request without a state the client is told; a
//! minimised window leaves its tile or float (a tile it was alone in is freed for the others) and waits in its
//! workspace's list until it is restored (the window list, Alt+Tab, its launcher tile). It comes back where it
//! was: into the stack it left when that stack is still there, else as a tile of its own at its old index; a
//! float with its old rectangle (and maximised again when it was).

use super::{Floating, Focus, Place, Rect, Tile, Workspace};

/// Where a minimised window goes back to.
#[derive(Clone, Debug, PartialEq)]
pub enum Back<W> {
    /// Its own tile at index `at` (or the end), or onto the stack of `with` while that is still tiled.
    Tile { at: usize, with: Option<W> },
    Float { rect: Rect, restore: Option<Rect> },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Minimized<W> {
    pub win: W,
    pub back: Back<W>,
}

impl<W: Clone + PartialEq> Workspace<W> {
    /// Takes `w` out of its tile or float into the minimised list; the focus moves to a neighbour (as when the
    /// window had closed). False if `w` is not shown on this workspace (already minimised, or elsewhere).
    pub fn minimize(&mut self, w: &W) -> bool {
        let back = match self.find(w) {
            Some(Place::Tiled { tile, index, depth, .. }) => {
                let stack = &self.tiles[tile].stack;
                let with = (depth > 1).then(|| stack[if index == 0 { 1 } else { index - 1 }].clone());
                Back::Tile { at: tile, with }
            }
            Some(Place::Floating { index }) => {
                let f = &self.floating[index];
                Back::Float { rect: f.rect, restore: f.restore }
            }
            None => return false,
        };
        let Some((win, _)) = self.take(w) else { return false };
        self.minimized.push(Minimized { win, back });
        true
    }

    pub fn is_minimized(&self, w: &W) -> bool {
        self.minimized.iter().any(|m| &m.win == w)
    }

    /// Puts a minimised window back where it was and focuses it; false if it is not minimised here.
    pub fn restore(&mut self, w: &W) -> bool {
        let Some(i) = self.minimized.iter().position(|m| &m.win == w) else { return false };
        let Minimized { win, back } = self.minimized.remove(i);
        match back {
            Back::Float { rect, restore } => {
                self.floating.push(Floating { win, rect, restore });
                self.focus = Focus::Float(self.floating.len() - 1);
            }
            Back::Tile { at, with } => match with.as_ref().and_then(|x| self.find(x)) {
                Some(Place::Tiled { tile, .. }) => {
                    let t = &mut self.tiles[tile];
                    t.stack.push(win);
                    t.top = t.stack.len() - 1;
                    self.focus = Focus::Tile(tile);
                }
                _ => {
                    let at = at.min(self.tiles.len());
                    self.tiles.insert(at, Tile { stack: vec![win], top: 0, weight: 1.0 });
                    self.focus = Focus::Tile(at);
                }
            },
        }
        true
    }

    /// A window that is gone leaves the list, and no other entry goes back next to it.
    pub(super) fn forget_minimized(&mut self, w: &W) {
        self.minimized.retain(|m| &m.win != w);
        for m in &mut self.minimized {
            if let Back::Tile { with, .. } = &mut m.back {
                with.take_if(|x| x == w);
            }
        }
    }

    /// Alt+Tab: the next window in the order tiles, floats, then the minimised ones newest first (`dir` -1 goes
    /// back). Landing on a minimised one returns it, for the caller to restore (`restore`).
    pub fn switch_step(&mut self, dir: i32) -> Option<W> {
        let n = self.tiles.len() + self.floating.len();
        let total = (n + self.minimized.len()) as i32;
        if total == 0 {
            return None;
        }
        let cur = match self.focus {
            Focus::Tile(t) => t as i32,
            Focus::Float(i) => (self.tiles.len() + i) as i32,
            // Nothing focused: forward starts at the first, backward at the last.
            Focus::None => if dir < 0 { total } else { -1 },
        };
        let next = (cur + dir).rem_euclid(total) as usize;
        if next < self.tiles.len() {
            self.focus = Focus::Tile(next);
            return None;
        }
        if next < n {
            self.focus = Focus::Float(next - self.tiles.len());
            return None;
        }
        Some(self.minimized[self.minimized.len() - 1 - (next - n)].win.clone())
    }

    /// Every window of the workspace, the minimised ones too.
    pub fn all_windows(&self) -> impl Iterator<Item = &W> {
        let tiled = self.tiles.iter().flat_map(|t| t.stack.iter());
        tiled.chain(self.floating.iter().map(|f| &f.win)).chain(self.minimized.iter().map(|m| &m.win))
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Focus, Rect, Workspace};

    const AREA: Rect = Rect { x: 0, y: 0, w: 1600, h: 900 };

    fn shown(ws: &Workspace<u32>) -> Vec<u32> {
        ws.arrange(AREA, AREA, 8).into_iter().filter(|p| p.visible).map(|p| p.win).collect()
    }

    #[test]
    fn a_minimised_tile_frees_its_place_and_the_focus_moves_on() {
        let mut ws = Workspace::default();
        for w in [1, 2, 3] {
            ws.add(w);
        }
        ws.focus_window(&2);
        assert!(ws.minimize(&2));
        assert!(!ws.minimize(&2), "twice is nothing");
        assert_eq!(ws.tiles.len(), 2, "the tile is freed");
        assert_eq!(shown(&ws), [1, 3]);
        assert!(ws.focused().is_some_and(|f| *f != 2));
        assert!(ws.find(&2).is_none() && ws.is_minimized(&2));
        assert!(ws.restore(&2));
        assert_eq!(ws.focused(), Some(&2));
        assert_eq!(ws.tiles.iter().map(|t| t.stack[0]).collect::<Vec<_>>(), [1, 2, 3], "back at its index");
        assert!(!ws.restore(&2));
    }

    #[test]
    fn a_stack_page_goes_back_into_its_stack() {
        let mut ws = Workspace::default();
        ws.add(1);
        ws.add(2);
        ws.stack_focused(); // 2 on top of 1's stack
        ws.add(3);
        ws.focus_window(&2);
        assert!(ws.minimize(&2));
        assert_eq!(ws.tiles[0].stack, [1]);
        assert_eq!(ws.focused(), Some(&1), "the stack shows its other page");
        ws.restore(&2);
        assert_eq!(ws.tiles[0].stack, [1, 2]);
        assert_eq!(ws.tiles[0].top, 1);
        assert_eq!(ws.focused(), Some(&2));
    }

    #[test]
    fn a_float_comes_back_with_its_rect_and_maximised() {
        let mut ws = Workspace::default();
        ws.add(1);
        ws.add(2);
        ws.toggle_float(&2, AREA);
        ws.toggle_max(&2, AREA);
        let rect = ws.floating[0].rect;
        ws.minimize(&2);
        assert!(ws.floating.is_empty());
        assert_eq!(ws.focus, Focus::Tile(0));
        ws.restore(&2);
        assert_eq!(ws.floating[0].rect, rect);
        assert!(ws.is_max(&2));
        assert_eq!(ws.focus, Focus::Float(0));
    }

    #[test]
    fn the_last_window_minimised_leaves_nothing_focused() {
        let mut ws = Workspace::default();
        ws.add(1);
        ws.minimize(&1);
        assert_eq!(ws.focus, Focus::None);
        assert!(ws.tiles.is_empty() && shown(&ws).is_empty());
        assert_eq!(ws.all_windows().copied().collect::<Vec<_>>(), [1]);
        assert_eq!(ws.switch_step(1), Some(1), "Alt+Tab brings it back");
        ws.restore(&1);
        assert_eq!(ws.focused(), Some(&1));
    }

    #[test]
    fn alt_tab_visits_the_shown_windows_then_restores_the_newest_minimised() {
        let mut ws = Workspace::default();
        for w in [1, 2, 3, 4] {
            ws.add(w);
        }
        ws.minimize(&3);
        ws.minimize(&4);
        ws.focus_window(&1);
        assert_eq!(ws.switch_step(1), None);
        assert_eq!(ws.focused(), Some(&2));
        assert_eq!(ws.switch_step(1), Some(4), "newest minimised first");
        ws.restore(&4);
        assert_eq!(ws.focused(), Some(&4));
        ws.focus_window(&1);
        assert_eq!(ws.switch_step(-1), Some(3), "backwards from the first: the oldest minimised");
    }

    #[test]
    fn a_gone_window_leaves_the_list_and_its_neighbour_entry() {
        let mut ws = Workspace::default();
        ws.add(1);
        ws.add(2);
        ws.stack_focused();
        ws.minimize(&2);
        ws.remove(&1);
        ws.add(5);
        ws.restore(&2);
        assert_eq!(ws.tiles.len(), 2, "its stack is gone: a tile of its own");
        ws.minimize(&2);
        ws.remove(&2);
        assert!(ws.minimized.is_empty());
    }

    #[test]
    fn a_fullscreen_window_minimised_leaves_its_fullscreen() {
        let mut ws = Workspace::default();
        ws.add(1);
        ws.add(2);
        assert!(ws.set_fullscreen(&2, true));
        ws.minimize(&2);
        assert!(ws.fullscreen.is_none());
        assert_eq!(shown(&ws), [1]);
        ws.restore(&2);
        assert!(!ws.is_fullscreen(&2));
    }
}
