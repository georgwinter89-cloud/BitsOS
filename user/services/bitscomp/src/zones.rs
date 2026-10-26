//! What a pointer position means on the desktop: header buttons and tabs, the edges of a floating
//! window, the gaps between tiles, and where a dragged header would land. Pure geometry in logical pixels,
//! shared by hit testing, drawing and the control socket's `hot` listing.

use crate::layout::{Place, Rect};
use crate::tabs;
use crate::theme::Theme;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Edges {
    pub left: bool,
    pub right: bool,
    pub top: bool,
    pub bottom: bool,
}

/// A part of a window's server-side frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Part {
    Body,
    Float,
    /// WY-5c: a floating window's maximise (and restore) button.
    Max,
    Close,
    /// A tab and its close button, by stack page (a floating window has one tab, page 0).
    Tab(usize),
    TabClose(usize),
    /// The "+N" chip of a strip with more pages than tabs.
    More,
    Resize(Edges),
    /// The "Beenden" button on a hung window's veil (`veil`).
    Kill,
    /// "Warten" on the veil: hides it for a while.
    Wait,
    /// The tile's own address field (theme.layout.address_bar = tile).
    Address,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
}

/// Where a dragged tile header lands; indices are tile positions before the move.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Drop {
    None,
    Stack(usize),
    Beside(usize, Side),
    OwnTile(usize),
    /// Inside its own tab strip: insert marker `slot`, new stack index `to`.
    Reorder {
        tile: usize,
        slot: usize,
        to: usize,
    },
}

impl Drop {
    /// For drops that make a new tile: its index among the tiles before the move.
    pub fn insert_index(self) -> Option<usize> {
        match self {
            Drop::Beside(t, Side::Left) => Some(t),
            Drop::Beside(t, Side::Right) => Some(t + 1),
            Drop::OwnTile(i) => Some(i),
            _ => None,
        }
    }
}

fn pad(t: &Theme) -> i32 {
    (t.tab_row - t.button_size) / 2
}

/// The header buttons sit in the tab row (below the tile's own address bar, if any).
pub fn close_button(f: Rect, t: &Theme) -> Rect {
    let s = t.button_size;
    Rect { x: f.x + f.w - t.border - pad(t) - s, y: f.y + t.tile_bar + pad(t), w: s, h: s }
}

pub fn float_button(f: Rect, t: &Theme) -> Rect {
    let c = close_button(f, t);
    Rect { x: c.x - 2 - c.w, ..c }
}

/// WY-5c: a floating window's third button, left of the float button.
pub fn max_button(f: Rect, t: &Theme) -> Rect {
    let c = float_button(f, t);
    Rect { x: c.x - 2 - c.w, ..c }
}

/// The frame the tab strip is laid out in: a floating window's header has one button more, so its tabs end
/// before the maximise button.
pub fn tab_frame(f: Rect, floating: bool, t: &Theme) -> Rect {
    if floating { Rect { w: f.w - t.button_size - 2, ..f } } else { f }
}

/// Pages and the shown page of the window at `place`: a floating window is a strip of one.
pub fn pages(place: Place) -> (usize, usize) {
    match place {
        Place::Tiled { depth, top, .. } => (depth, top),
        Place::Floating { .. } => (1, 0),
    }
}

/// The part of frame `f` at (x, y), which must lie inside it; borders count as the body.
pub fn header_part(f: Rect, place: Place, t: &Theme, x: f64, y: f64) -> Part {
    if y >= (f.y + t.header_height) as f64 {
        return Part::Body;
    }
    if y < (f.y + t.tile_bar) as f64 {
        let a = crate::header::tile_address_rect(f, t);
        return if a.contains(x, y) { Part::Address } else { Part::Body };
    }
    if close_button(f, t).contains(x, y) {
        return Part::Close;
    }
    if float_button(f, t).contains(x, y) {
        return Part::Float;
    }
    if matches!(place, Place::Floating { .. }) && max_button(f, t).contains(x, y) {
        return Part::Max;
    }
    let (n, shown) = pages(place);
    let strip = tabs::strip(tab_frame(f, matches!(place, Place::Floating { .. }), t), n, shown, t);
    let row = |r: Rect| Rect { y: f.y + t.tile_bar, h: t.tab_row, ..r };
    if strip.more.is_some_and(|(r, _)| row(r).contains(x, y)) {
        return Part::More;
    }
    for (i, r) in strip.tabs.iter().enumerate() {
        let page = strip.first + i;
        if tabs::close_rect(*r, t).contains(x, y) {
            return Part::TabClose(page);
        }
        // The whole header height counts, so the strip is easy to hit.
        if row(*r).contains(x, y) {
            return Part::Tab(page);
        }
    }
    Part::Body
}

/// The edges of a floating frame under (x, y): `resize_margin` outside the frame, `border` inside it; near a
/// corner both edges.
pub fn resize_edges(f: Rect, t: &Theme, x: f64, y: f64) -> Option<Edges> {
    let m = t.resize_margin as f64;
    let inner = t.border as f64;
    let corner = t.button_size as f64;
    let (l, r, top, b) = (f.x as f64, (f.x + f.w) as f64, f.y as f64, (f.y + f.h) as f64);
    if x < l - m || x >= r + m || y < top - m || y >= b + m {
        return None;
    }
    let mut e = Edges { left: x < l + inner, right: x >= r - inner, top: y < top + inner, bottom: y >= b - inner };
    if e.left || e.right {
        e.top |= y < top + corner;
        e.bottom |= y >= b - corner;
    } else if e.top || e.bottom {
        e.left |= x < l + corner;
        e.right |= x >= r - corner;
    } else {
        return None;
    }
    Some(e)
}

/// The frame after dragging edges `e` by (dx, dy) from `start`; the opposite edges stay put.
pub fn resized(start: Rect, e: Edges, dx: f64, dy: f64, min: (i32, i32)) -> Rect {
    let (dx, dy) = (dx.round() as i32, dy.round() as i32);
    let mut r = start;
    if e.left {
        r.x = (start.x + dx).min(start.x + start.w - min.0);
        r.w = start.x + start.w - r.x;
    } else if e.right {
        r.w = (start.w + dx).max(min.0);
    }
    if e.top {
        r.y = (start.y + dy).min(start.y + start.h - min.1);
        r.h = start.y + start.h - r.y;
    } else if e.bottom {
        r.h = (start.h + dy).max(min.1);
    }
    r
}

/// WY-5c: two presses on a header at most this far apart in time make a double click.
pub const DOUBLE_CLICK: std::time::Duration = std::time::Duration::from_millis(400);

/// Whether a press at `pos` and `now` is the second of a double click after the press `prev` (time, place):
/// soon enough and no further away than `slack` logical pixels.
pub fn is_double(prev: Option<(std::time::Duration, (f64, f64))>, now: std::time::Duration, pos: (f64, f64), slack: f64) -> bool {
    prev.is_some_and(|(t, (x, y))| now >= t && now - t <= DOUBLE_CLICK && (pos.0 - x).hypot(pos.1 - y) <= slack)
}

/// The gap between tile `i` and `i + 1` under (x, y); the neighbouring borders count, so the gap is easy to hit.
pub fn gap_at(tiles: &[Rect], t: &Theme, x: f64, y: f64) -> Option<usize> {
    let slack = t.border as f64;
    tiles.windows(2).position(|p| {
        let (a, b) = (p[0], p[1]);
        x >= (a.x + a.w) as f64 - slack && x < b.x as f64 + slack && y >= a.y as f64 && y < (a.y + a.h) as f64
    })
}

/// Where a header dragged from tile `src` (a stack of `depth` pages) would land at (x, y): a tile's centre
/// stacks, its side zones place beside it, anything else gets its own tile at that position. Drops that
/// would change nothing are `Drop::None`.
pub fn drop_target(tiles: &[Rect], src: usize, depth: usize, t: &Theme, x: f64, y: f64) -> Drop {
    let d = match tiles.iter().position(|r| r.contains(x, y)) {
        Some(i) => {
            let r = tiles[i];
            let zone = (r.w as f32 * t.edge_zone) as f64;
            if x < r.x as f64 + zone {
                Drop::Beside(i, Side::Left)
            } else if x >= (r.x + r.w) as f64 - zone {
                Drop::Beside(i, Side::Right)
            } else {
                Drop::Stack(i)
            }
        }
        None => Drop::OwnTile(tiles.iter().filter(|r| r.x as f64 + r.w as f64 / 2.0 < x).count()),
    };
    let unchanged = match d {
        Drop::Stack(i) => i == src,
        _ => depth == 1 && d.insert_index().is_some_and(|i| i == src || i == src + 1),
    };
    if unchanged { Drop::None } else { d }
}

/// Where a tab of page `from` dragged along its own strip to `x` would go.
pub fn reorder_target(strip: &tabs::Strip, tile: usize, from: usize, x: f64) -> Drop {
    let slot = tabs::slot_at(strip, x);
    let p = strip.first + slot;
    let to = if p > from { p - 1 } else { p };
    if to == from { Drop::None } else { Drop::Reorder { tile, slot, to } }
}

/// The translucent rectangle shown while dragging: the whole tile for a stack, its half for beside, a bar at
/// the new tile's place otherwise.
pub fn drop_preview(tiles: &[Rect], d: Drop, area: Rect, gap: i32) -> Option<Rect> {
    let half = |r: Rect, right: bool| Rect { x: if right { r.x + r.w - r.w / 2 } else { r.x }, w: r.w / 2, ..r };
    match d {
        Drop::None | Drop::Reorder { .. } => None,
        Drop::Stack(i) => tiles.get(i).copied(),
        Drop::Beside(i, side) => tiles.get(i).map(|&r| half(r, side == Side::Right)),
        Drop::OwnTile(i) => {
            let x = match (tiles.get(i.wrapping_sub(1)), tiles.get(i)) {
                (Some(a), Some(b)) => (a.x + a.w + b.x) / 2,
                (None, Some(b)) => b.x - gap / 2,
                (Some(a), None) => a.x + a.w + gap / 2,
                (None, None) => area.x + area.w / 2,
            };
            let (y, h) = tiles.first().map_or((area.y, area.h), |r| (r.y, r.h));
            let x0 = (x - 40).max(area.x);
            Some(Rect { x: x0, y, w: (x + 40).min(area.x + area.w) - x0, h })
        }
    }
}

#[cfg(test)]
#[path = "zones_tests.rs"]
mod tests;
