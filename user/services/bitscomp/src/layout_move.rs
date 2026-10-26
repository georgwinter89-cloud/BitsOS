//! Layout changes the mouse makes: moving a window to a drop target, reordering a stack's tabs, dragging the
//! split between two tiles.

use super::{Floating, Focus, Place, Rect, Tile, Workspace};
use crate::zones::Drop;

impl<W: Clone + PartialEq> Workspace<W> {
    /// Tile frames left to right; the last tile takes the rounding remainder so the row ends exactly at the gap.
    pub fn tile_frames(&self, area: Rect, gap: i32) -> Vec<Rect> {
        let total: f32 = self.tiles.iter().map(|t| t.weight).sum();
        let usable = area.w - gap * (self.tiles.len() as i32 + 1);
        let mut x = area.x + gap;
        let mut out = Vec::with_capacity(self.tiles.len());
        for (t, tile) in self.tiles.iter().enumerate() {
            let w = if t + 1 == self.tiles.len() {
                area.x + area.w - gap - x
            } else {
                (usable as f32 * tile.weight / total).round() as i32
            };
            out.push(Rect { x, y: area.y + gap, w: w.max(1), h: (area.h - 2 * gap).max(1) });
            x += w + gap;
        }
        out
    }

    /// Takes a window out of the workspace; also says which tile vanished with it.
    pub(super) fn take(&mut self, w: &W) -> Option<(W, Option<usize>)> {
        let place = self.find(w)?;
        // WY-8: a window that leaves the workspace leaves its fullscreen too.
        self.fullscreen.take_if(|f| f == w);
        match place {
            Place::Tiled { tile, index, .. } => {
                let n = self.tiles.len();
                let win = self.take_from_tile(tile, index);
                Some((win, (self.tiles.len() < n).then_some(tile)))
            }
            Place::Floating { index } => {
                let Floating { win, .. } = self.floating.remove(index);
                self.focus = match self.focus {
                    Focus::Float(i) if i == index => self.fallback_focus(),
                    Focus::Float(i) if i > index => Focus::Float(i - 1),
                    f => f,
                };
                Some((win, None))
            }
        }
    }

    /// Puts a window where a dragged header was dropped (`zones::drop_target`); tile indices are those before
    /// the move.
    pub fn move_window(&mut self, w: &W, d: Drop) -> bool {
        let source_alone = match self.find(w) {
            Some(Place::Tiled { tile, depth: 1, .. }) => Some(tile),
            None => return false,
            _ => None,
        };
        match d {
            Drop::None => return false,
            Drop::Reorder { tile, to, .. } => return self.reorder(w, tile, to),
            Drop::Stack(t) if source_alone == Some(t) || t >= self.tiles.len() => return false,
            _ => {}
        }
        let Some((win, removed)) = self.take(w) else { return false };
        let shift = |i: usize| if removed.is_some_and(|r| r < i) { i - 1 } else { i };
        if let Drop::Stack(t) = d {
            let t = shift(t);
            let tile = &mut self.tiles[t];
            tile.stack.push(win);
            tile.top = tile.stack.len() - 1;
            self.focus = Focus::Tile(t);
        } else if let Some(i) = d.insert_index() {
            let i = shift(i).min(self.tiles.len());
            self.tiles.insert(i, Tile { stack: vec![win], top: 0, weight: 1.0 });
            self.focus = Focus::Tile(i);
        }
        true
    }

    /// Moves the split between tile `t` and the next one to `x` (the pointer in the gap); their combined width
    /// stays, neither gets narrower than `min_w`.
    pub fn set_split(&mut self, t: usize, x: f64, area: Rect, gap: i32, min_w: i32) -> bool {
        let frames = self.tile_frames(area, gap);
        let (Some(a), Some(b)) = (frames.get(t), frames.get(t + 1)) else { return false };
        let pair = a.w + b.w;
        if pair < 2 * min_w {
            return false;
        }
        let left = ((x - a.x as f64 - gap as f64 / 2.0).round() as i32).clamp(min_w, pair - min_w);
        let sum = self.tiles[t].weight + self.tiles[t + 1].weight;
        self.tiles[t].weight = sum * left as f32 / pair as f32;
        self.tiles[t + 1].weight = sum - self.tiles[t].weight;
        true
    }

    /// Moves `w` to index `to` of its stack `t`; the shown window stays shown.
    pub fn reorder(&mut self, w: &W, t: usize, to: usize) -> bool {
        let Some(Place::Tiled { tile, index, .. }) = self.find(w) else { return false };
        if tile != t || to >= self.tiles[t].stack.len() {
            return false;
        }
        let s = &mut self.tiles[t];
        let shown = s.stack[s.top].clone();
        let win = s.stack.remove(index);
        s.stack.insert(to, win);
        s.top = s.stack.iter().position(|x| *x == shown).unwrap_or(0);
        self.focus = Focus::Tile(t);
        true
    }
}
