//! `C: RamFrame`: the authoritative picture in normal RAM (0x00RRGGBB per pixel, `width * height`, no padding)
//! plus the damage since the last present, kept as a short list of rectangles so a present copies only what
//! changed (two far-apart changes stay two small copies, not their bounding box).
//!
//! Damage merging: a new rectangle is merged with every listed one it overlaps (so the list stays disjoint and
//! no pixel is copied twice) and with every one it touches when the bounding box wastes at most a quarter
//! (glyph after glyph becomes one line). Cost bound (hostile input, R1): merging is quadratic in the list
//! length, so once the list is longer than one entry per 16 rows (+16) it collapses into its bounding box.
//! Nothing is refused; the copy just gets coarser. The list's whole capacity is reserved in `new`, so `mark`
//! never allocates: it runs under `display.frame` and is reached from the panic/NMI path and klog sinks,
//! where re-entering the heap could deadlock.

use crate::Rect;
use alloc::vec;
use alloc::vec::Vec;
use devmodel::display::DisplayDevice;

pub struct RamFrame {
    pub width: u32,
    pub height: u32,
    pixels: Vec<u32>,
    damage: Vec<Rect>,
    /// Union of `damage`.
    bbox: Rect,
}

/// What one present copied.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PresentStats {
    pub rects: u32,
    pub pixels: u64,
    /// Rectangles the device refused (they are dropped: a retry would be refused again).
    pub errors: u32,
}

impl RamFrame {
    /// `new` that returns None instead of failing when the pixels cannot be allocated (GX-4: a mode switch at run
    /// time must not abort on a full heap).
    pub fn try_new(width: u32, height: u32) -> Option<Self> {
        let n = (width as usize).checked_mul(height as usize)?;
        let mut pixels = Vec::new();
        pixels.try_reserve_exact(n).ok()?;
        pixels.resize(n, 0);
        let mut damage = Vec::new();
        damage.try_reserve_exact(collapse_at(height) + 1).ok()?;
        Some(RamFrame { width, height, pixels, damage, bbox: Rect::default() })
    }

    pub fn new(width: u32, height: u32) -> Self {
        RamFrame {
            width,
            height,
            pixels: vec![0; (width as usize) * (height as usize)],
            damage: Vec::with_capacity(collapse_at(height) + 1),
            bbox: Rect::default(),
        }
    }
    pub fn pixels(&self) -> &[u32] {
        &self.pixels
    }
    /// Direct pixel access; the caller marks what it changed (`mark`), or the change stays off screen.
    pub fn pixels_mut(&mut self) -> &mut [u32] {
        &mut self.pixels
    }
    /// The bounding box of the damage.
    pub fn damage(&self) -> Rect {
        self.bbox
    }
    /// The damaged rectangles (disjoint, inside the frame).
    pub fn damage_rects(&self) -> &[Rect] {
        &self.damage
    }
    pub fn full(&self) -> Rect {
        Rect::new(0, 0, self.width, self.height)
    }
    pub fn mark_all(&mut self) {
        let r = self.full();
        self.mark(r);
    }

    pub fn mark(&mut self, r: Rect) {
        let mut r = r.clip(self.width, self.height);
        if r.is_empty() {
            return;
        }
        self.bbox = self.bbox.union(r);
        let mut i = 0;
        while i < self.damage.len() {
            let e = self.damage[i];
            if worth_merging(e, r) {
                r = e.union(r);
                self.damage.swap_remove(i);
                // The bigger rectangle may now reach ones already passed.
                i = 0;
            } else {
                i += 1;
            }
        }
        if self.damage.len() >= collapse_at(self.height) {
            self.collapse();
        } else {
            self.damage.push(r);
        }
    }

    /// All damage becomes its bounding box (within the capacity `new` reserved: no allocation).
    fn collapse(&mut self) {
        self.damage.clear();
        if !self.bbox.is_empty() {
            self.damage.push(self.bbox);
        }
    }

    pub fn fill(&mut self, r: Rect, color: u32) {
        let r = r.clip(self.width, self.height);
        let w = self.width as usize;
        for row in r.y as usize..(r.y + r.h) as usize {
            let start = row * w + r.x as usize;
            self.pixels[start..start + r.w as usize].fill(color);
        }
        self.mark(r);
    }
    /// One pixel, without marking it (callers draw a glyph and mark its box once).
    pub fn set(&mut self, x: u32, y: u32, color: u32) {
        if x < self.width && y < self.height {
            self.pixels[y as usize * self.width as usize + x as usize] = color;
        }
    }
    /// Moves the picture up by `by` rows (console scrolling), fills the freed rows and marks all of it damaged.
    pub fn scroll_up(&mut self, by: u32, fill: u32) {
        let by = by.min(self.height);
        let w = self.width as usize;
        self.pixels.copy_within(by as usize * w.., 0);
        let start = (self.height - by) as usize * w;
        self.pixels[start..].fill(fill);
        self.mark_all();
    }
    /// Takes the damage (its bounding box) and clears it.
    pub fn take_damage(&mut self) -> Rect {
        self.damage.clear();
        core::mem::take(&mut self.bbox)
    }

    /// Copies every damaged rectangle to `dev` and clears the damage.
    pub fn present_to(&mut self, dev: &dyn DisplayDevice) -> PresentStats {
        let mut st = PresentStats::default();
        for r in self.damage.drain(..) {
            match dev.present(&self.pixels, self.width, (r.x, r.y, r.w, r.h)) {
                Ok(_) => {
                    st.rects += 1;
                    st.pixels += r.area();
                }
                Err(_) => st.errors += 1,
            }
        }
        self.bbox = Rect::default();
        st
    }
}

/// The list length at which the damage collapses into its bounding box.
fn collapse_at(height: u32) -> usize {
    height as usize / 16 + 16
}

fn worth_merging(a: Rect, b: Rect) -> bool {
    a.intersects(b) || (a.touches(b) && a.union(b).area() * 4 <= (a.area() + b.area()) * 5)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::Recorder;

    #[test]
    fn try_new_sizes_or_refuses_without_a_panic() {
        let f = RamFrame::try_new(30, 20).unwrap();
        assert_eq!((f.width, f.height, f.pixels().len()), (30, 20, 600));
        assert!(f.pixels().iter().all(|&p| p == 0));
        assert!(RamFrame::try_new(u32::MAX, u32::MAX).is_none(), "an impossible size is refused, not a panic");
    }

    #[test]
    fn far_apart_stay_apart_neighbours_merge() {
        let mut f = RamFrame::new(100, 100);
        f.mark(Rect::new(0, 0, 8, 16));
        f.mark(Rect::new(8, 0, 8, 16));
        assert_eq!(f.damage_rects(), &[Rect::new(0, 0, 16, 16)], "glyphs on a line merge");
        f.mark(Rect::new(80, 80, 4, 4));
        assert_eq!(f.damage_rects().len(), 2, "a far rectangle stays separate");
        assert_eq!(f.damage(), Rect::new(0, 0, 84, 84));
        f.mark(Rect::new(10, 10, 80, 80));
        assert_eq!(f.damage_rects(), &[Rect::new(0, 0, 90, 90)], "overlaps always merge, transitively");
    }

    #[test]
    fn list_stays_disjoint_and_clipped() {
        let mut f = RamFrame::new(64, 48);
        let mut seed = 7u32;
        for _ in 0..500 {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
            let r = Rect::new(seed % 70, (seed >> 8) % 50, (seed >> 16) % 9 + 1, (seed >> 20) % 7 + 1);
            f.mark(r);
            let d = f.damage_rects();
            assert!(d.len() <= 48 / 16 + 16);
            for (i, a) in d.iter().enumerate() {
                assert!(a.x + a.w <= 64 && a.y + a.h <= 48 && !a.is_empty());
                for b in &d[i + 1..] {
                    assert!(!a.intersects(*b), "{a:?} overlaps {b:?}");
                }
            }
        }
    }

    #[test]
    fn hostile_many_rects_collapse() {
        let mut f = RamFrame::new(1000, 160);
        for i in 0..200 {
            f.mark(Rect::new(i * 5, (i * 7) % 160, 1, 1));
        }
        assert!(f.damage_rects().len() <= 160 / 16 + 16);
    }

    #[test]
    fn mark_never_allocates() {
        let mut f = RamFrame::new(4000, 160);
        let cap = f.damage.capacity();
        assert!(cap > 160 / 16 + 16);
        for round in 0..3 {
            // Far more disjoint rectangles than the list holds, so it grows to the limit and collapses.
            for i in 0..100 {
                f.mark(Rect::new(i * 40 + round, (i * 7) % 160, 1, 1));
                assert_eq!(f.damage.capacity(), cap, "mark reallocated at rectangle {i}");
            }
            let rec = Recorder { rects: ksync::SpinLock::named("display.test_rec3", alloc::vec::Vec::new()) };
            f.present_to(&rec);
            assert_eq!(f.damage.capacity(), cap, "present_to reallocated");
        }
    }

    #[test]
    fn present_copies_each_rect_once_and_clears() {
        let mut f = RamFrame::new(50, 40);
        f.fill(Rect::new(1, 1, 2, 2), 0x123456);
        f.fill(Rect::new(40, 30, 3, 3), 0xABCDEF);
        let rec = Recorder { rects: ksync::SpinLock::named("display.test_rec2", alloc::vec::Vec::new()) };
        let st = f.present_to(&rec);
        assert_eq!(st, PresentStats { rects: 2, pixels: 13, errors: 0 });
        let mut got = rec.rects.lock().clone();
        got.sort();
        assert_eq!(got, alloc::vec![(1, 1, 2, 2), (40, 30, 3, 3)]);
        assert!(f.damage().is_empty() && f.damage_rects().is_empty());
        assert_eq!(f.present_to(&rec).rects, 0);
    }

    #[test]
    fn take_damage_contract() {
        let mut f = RamFrame::new(10, 4);
        f.fill(Rect::new(1, 1, 2, 1), 7);
        f.fill(Rect::new(8, 3, 9, 9), 5);
        assert_eq!(f.take_damage(), Rect::new(1, 1, 9, 3));
        assert!(f.damage_rects().is_empty());
        f.scroll_up(1, 0);
        assert_eq!(f.pixels()[1], 7);
        assert_eq!(f.pixels()[39], 0);
        assert_eq!(f.damage_rects(), &[f.full()]);
    }
}
