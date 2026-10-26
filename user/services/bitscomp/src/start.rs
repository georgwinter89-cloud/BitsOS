//! The start page (Start.dc.html, WY-5) a workspace shows when it has no window: the date and a greeting,
//! "Letzte Sitzung wiederherstellen" (when a saved session is offered), "Neue Sitzung", the favourites as tiles,
//! recently used addresses, the building blocks with real values. Active contexts have no source yet: left out.
//! Layout in logical pixels on the mock-up's 12-column grid (gap 24, padding 40/80); one image of the work area.

use crate::layout::Rect;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Part {
    Restore,
    New,
    Fav(usize),
    Recent(usize),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Page {
    pub date: Rect,
    pub greet: Rect,
    pub restore: Rect,
    pub new: Rect,
    pub fav_head: Rect,
    pub favs: Vec<Rect>,
    pub recent_head: Rect,
    pub recents: Vec<Rect>,
    pub blocks_head: Rect,
    pub blocks: Vec<Rect>,
}

pub const HEAD_H: i32 = 18;
pub const FAV_H: i32 = 112;
pub const RECENT_H: i32 = 46;
pub const BLOCK_H: i32 = 96;
const GAP: i32 = 24;

/// The page in work area `w` for `favs` favourites, `recent` recent entries and `blocks` building blocks;
/// what does not fit above the bottom is left out (the lists show their first entries).
pub fn layout(w: Rect, favs: usize, recent: usize, blocks: usize) -> Page {
    let px = (w.w / 20).clamp(16, 80);
    let x0 = w.x + px;
    let inner = (w.w - 2 * px).max(120);
    let col = (inner - 11 * GAP) as f32 / 12.0;
    let span = |from: i32, n: i32| {
        let x = x0 + (from as f32 * (col + GAP as f32)).round() as i32;
        (x, (n as f32 * col + (n - 1) as f32 * GAP as f32).round() as i32)
    };
    let bottom = w.y + w.h - 24;
    let mut y = w.y + 40;
    let date = Rect { x: x0, y, w: inner, h: 18 };
    let greet = Rect { x: x0, y: y + 22, w: inner, h: 50 };
    y += 72 + GAP;
    let (lx, lw) = span(0, 7);
    let (rx, rw) = span(7, 5);
    let restore = Rect { x: lx, y, w: lw, h: 102 };
    let new = Rect { x: rx, y, w: rw, h: 102 };
    y += 102 + GAP;
    let fav_head = Rect { x: lx, y, w: lw, h: HEAD_H };
    let recent_head = Rect { x: rx, y, w: rw, h: HEAD_H };
    let top = y + HEAD_H + 12;
    let fw = (lw - 2 * 12) / 3;
    let mut fy = top;
    let mut fav_rects = Vec::new();
    for i in 0..favs {
        let (c, r) = (i as i32 % 3, i as i32 / 3);
        fy = top + r * (FAV_H + 12);
        if fy + FAV_H > bottom {
            break;
        }
        fav_rects.push(Rect { x: lx + c * (fw + 12), y: fy, w: fw, h: FAV_H });
    }
    let after_favs = if fav_rects.is_empty() { top } else { fy + FAV_H + GAP };
    let mut recents = Vec::new();
    for i in 0..recent {
        let ry = top + i as i32 * RECENT_H;
        if ry + RECENT_H > bottom {
            break;
        }
        recents.push(Rect { x: rx, y: ry, w: rw, h: RECENT_H });
    }
    let blocks_head = Rect { x: lx, y: after_favs, w: lw, h: HEAD_H };
    let by = after_favs + HEAD_H + 12;
    let blocks = (0..blocks as i32)
        .take_while(|_| by + BLOCK_H <= bottom)
        .filter(|i| *i < 3)
        .map(|i| Rect { x: lx + i * (fw + 12), y: by, w: fw, h: BLOCK_H })
        .collect();
    Page { date, greet, restore, new, fav_head, favs: fav_rects, recent_head, recents, blocks_head, blocks }
}

impl Page {
    pub fn hit(&self, x: f64, y: f64, can_restore: bool) -> Option<Part> {
        if can_restore && self.restore.contains(x, y) {
            return Some(Part::Restore);
        }
        if self.new.contains(x, y) {
            return Some(Part::New);
        }
        if let Some(i) = self.favs.iter().position(|r| r.contains(x, y)) {
            return Some(Part::Fav(i));
        }
        self.recents.iter().position(|r| r.contains(x, y)).map(Part::Recent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_page_fits_the_work_area_at_both_sizes() {
        for w in [Rect { x: 212, y: 90, w: 1536, h: 1030 }, Rect { x: 212, y: 90, w: 768, h: 678 }] {
            let p = layout(w, 7, 12, 2);
            let all = [p.date, p.greet, p.restore, p.new, p.fav_head, p.recent_head]
                .into_iter()
                .chain(p.favs.iter().copied())
                .chain(p.recents.iter().copied())
                .chain(p.blocks.iter().copied());
            for r in all {
                assert!(r.x >= w.x && r.x + r.w <= w.x + w.w && r.y >= w.y && r.y + r.h <= w.y + w.h, "{r:?} in {w:?}");
            }
            assert!(p.restore.x + p.restore.w < p.new.x, "restore left of new");
            assert!(!p.favs.is_empty() && !p.recents.is_empty());
            assert_eq!(p.hit((p.new.x + 5) as f64, (p.new.y + 5) as f64, true), Some(Part::New));
            assert_eq!(p.hit((p.restore.x + 5) as f64, (p.restore.y + 5) as f64, false), None);
        }
    }
}
