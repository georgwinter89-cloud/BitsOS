//! Held-left-button drags: a header or tab click that may become a move, floating moves and resizes, tile drags
//! with a drop target (or a new place in the own tab strip), gap drags.

use smithay::desktop::Window;
use smithay::input::pointer::CursorIcon;
use smithay::utils::{Logical, Point};

use crate::layout::{Place, Rect};
use crate::state::App;
use crate::tabs;
use crate::zones::{self, Drop, Edges, Part};

/// What a held left button is doing.
pub enum Grab {
    /// Pressed on a header: still a click until the pointer moves past `drag_threshold`.
    Pending {
        window: Window,
        start: Point<f64, Logical>,
    },
    MoveFloat {
        window: Window,
        grab: Point<f64, Logical>,
    },
    MoveTile {
        window: Window,
        drop: Drop,
    },
    Resize {
        window: Window,
        edges: Edges,
        start: Point<f64, Logical>,
        rect: Rect,
    },
    Split {
        gap: usize,
    },
    /// Pressed on a header button: it acts on release over the same button.
    Button {
        window: Window,
        part: Part,
    },
}

impl Grab {
    pub fn window(&self) -> Option<&Window> {
        match self {
            Grab::Pending { window, .. }
            | Grab::MoveFloat { window, .. }
            | Grab::MoveTile { window, .. }
            | Grab::Resize { window, .. }
            | Grab::Button { window, .. } => Some(window),
            Grab::Split { .. } => None,
        }
    }
}

pub fn resize_icon(e: Edges) -> CursorIcon {
    match (e.left, e.right, e.top, e.bottom) {
        (true, _, true, _) => CursorIcon::NwResize,
        (_, true, true, _) => CursorIcon::NeResize,
        (true, _, _, true) => CursorIcon::SwResize,
        (_, true, _, true) => CursorIcon::SeResize,
        (true, _, _, _) => CursorIcon::WResize,
        (_, true, _, _) => CursorIcon::EResize,
        (_, _, true, _) => CursorIcon::NResize,
        _ => CursorIcon::SResize,
    }
}

impl App {
    /// A maximised floating window at `index` gets its old size back, placed under the pointer at (px, py) as
    /// `layout::restored_at` says; others stay as they are.
    pub(crate) fn unmax_at(&mut self, index: usize, px: f64, py: f64) {
        let full = self.area_full();
        let header = self.theme.header_height;
        let Some(f) = self.ws.floating.get_mut(index) else { return };
        let Some(old) = f.restore.take() else { return };
        let grab_y = (py - f.rect.y as f64).clamp(0.0, header as f64 - 1.0);
        f.rect = crate::layout::clamp_float(crate::layout::restored_at(old, full, px, py, grab_y), full, header);
        eprintln!("bitscomp: maximised window restored by a drag");
        self.layout_dirty = true;
    }

    pub(crate) fn grab_motion(&mut self, g: Grab, pos: Point<f64, Logical>) -> Grab {
        let t = self.theme.clone();
        let t = &*t;
        match g {
            Grab::Pending { window, start } => {
                let d = pos - start;
                if d.x.hypot(d.y) < t.drag_threshold {
                    return Grab::Pending { window, start };
                }
                self.cursor_deco = Some(CursorIcon::Grabbing);
                let next = match self.ws.find(&window) {
                    Some(Place::Floating { index }) => {
                        // WY-5c: dragging a maximised window's header gives it its old size under the pointer.
                        self.unmax_at(index, start.x, start.y);
                        let r = self.ws.floating[index].rect;
                        Grab::MoveFloat { window, grab: start - Point::from((r.x as f64, r.y as f64)) }
                    }
                    _ => Grab::MoveTile { window, drop: Drop::None },
                };
                self.grab_motion(next, pos)
            }
            Grab::MoveFloat { window, grab } => {
                if let Some(Place::Floating { index }) = self.ws.find(&window) {
                    let full = self.area_full();
                    let r = &mut self.ws.floating[index].rect;
                    let moved = Rect { x: (pos.x - grab.x).round() as i32, y: (pos.y - grab.y).round() as i32, ..*r };
                    // WY-5c: anywhere on the screen, but `REACH` of the header stays on it.
                    *r = crate::layout::clamp_float(moved, full, t.header_height);
                    self.layout_dirty = true;
                }
                Grab::MoveFloat { window, grab }
            }
            Grab::MoveTile { window, drop } => {
                let new = match self.ws.find(&window) {
                    Some(Place::Tiled { tile, index, depth, top }) => {
                        let frames = self.ws.tile_frames(self.area(), t.gap);
                        let f = frames[tile];
                        if depth > 1 && f.contains(pos.x, pos.y) && pos.y < (f.y + t.header_height) as f64 {
                            zones::reorder_target(&tabs::strip(f, depth, top, t), tile, index, pos.x)
                        } else {
                            zones::drop_target(&frames, tile, depth, t, pos.x, pos.y)
                        }
                    }
                    _ => Drop::None,
                };
                if new != drop {
                    self.needs_frame = true;
                }
                Grab::MoveTile { window, drop: new }
            }
            Grab::Resize { window, edges, start, rect } => {
                if let Some(Place::Floating { index }) = self.ws.find(&window) {
                    let d = pos - start;
                    let r = zones::resized(rect, edges, d.x, d.y, t.min_float);
                    let full = self.area_full();
                    self.ws.floating[index].rect = crate::layout::clamp_resized(r, full, t.header_height);
                    self.layout_dirty = true;
                }
                Grab::Resize { window, edges, start, rect }
            }
            Grab::Split { gap } => {
                if self.ws.set_split(gap, pos.x, self.area(), t.gap, t.min_tile_width) {
                    self.layout_dirty = true;
                }
                Grab::Split { gap }
            }
            g @ Grab::Button { .. } => g,
        }
    }
}
