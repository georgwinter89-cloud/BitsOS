//! INT-D1 (CR DS-5-3): the first take since boot shows what the console showed (the boot screen) instead of fb0's
//! never written memory, which QEMU showed as black for 0.22-0.26 s until bitscomp's first store. The console's
//! frame is copied into fb0's visible window before the take's refresh, so the refresh presents the same picture.
//!
//! A store is never overwritten: before the first take no program holds the screen, so damage reports are off and
//! every store faults first (the page is write-protected), and the fault marks the page under `fbdev.dirty`
//! before it hands out a writable page. The seed writes each page piece under that same lock and skips marked
//! pages. write(2) writes before it marks, so it ends the seeding window under `fbdev.screen` first (`unfresh`).

use super::*;

impl Fb {
    /// write(2) is about to store: no seed may run after this point (the store is not marked yet when it lands).
    /// Waits for a seed in progress (under `fbdev.screen`); a no-op once the first take has happened.
    pub(super) fn unfresh(&self) {
        if self.fresh.load(Ordering::Acquire) {
            let _scr = self.screen.lock();
            self.fresh.store(false, Ordering::Release);
        }
    }

    /// Called by `take` under `fbdev.screen`, before the refresh; only the first take since boot seeds.
    pub(super) fn seed(&self, scr: &mut Screen) {
        if !self.fresh.swap(false, Ordering::AcqRel) {
            return;
        }
        let g = self.g();
        let (w, h) = (g.width as usize, g.height as usize);
        let frame = &mut scr.frame;
        let copied = display::with_frame(|cf| {
            if (cf.width as usize, cf.height as usize) != (w, h) || (frame.width as usize, frame.height as usize) != (w, h) {
                return false;
            }
            frame.pixels_mut().copy_from_slice(cf.pixels());
            true
        });
        if copied != Some(true) {
            klog::event!(Info, "fbdev", "seed.skipped", width = w, height = h; "the console frame has another size (or none)");
            return;
        }
        let mut row: Vec<u8> = Vec::new();
        if row.try_reserve_exact(w * 4).is_err() {
            klog::event!(Warn, "fbdev", "seed.no_memory", bytes = w * 4);
            return;
        }
        let (line, top) = (self.line() as usize, self.yoffset.load(Ordering::Acquire) as usize);
        let (mut seeded, mut kept) = (0u64, 0u64);
        let t = time::now_ns();
        for r in 0..h {
            row.clear();
            for p in &frame.pixels()[r * w..(r + 1) * w] {
                row.extend_from_slice(&p.to_ne_bytes());
            }
            let base = (top + r) * line;
            let mut at = 0;
            while at < row.len() {
                let off = base + at;
                let n = (4096 - off % 4096).min(row.len() - at);
                let dirty = self.shared.dirty.lock();
                if dirty.is_set((off / 4096) as u64) {
                    kept += n as u64 / 4;
                } else if self.mem.write(off, &row[at..at + n]) {
                    seeded += n as u64 / 4;
                }
                drop(dirty);
                at += n;
            }
        }
        klog::event!(Info, "fbdev", "screen.seeded", pixels = seeded, kept = kept, us = (time::now_ns() - t) / 1000;
            "fb0 starts with the console's picture");
    }
}
