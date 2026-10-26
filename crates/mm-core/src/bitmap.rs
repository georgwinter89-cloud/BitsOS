//! A hierarchical bitmap over a slice of words: level 0 has one bit per block, every level above one bit per
//! non-zero word of the level below, up to a single word. Set, clear and "find the lowest set bit" touch one
//! word per level, so finding a free block in a 20 GiB region costs four loads instead of a scan.
//!
//! The words live in the allocator's metadata blob (`AtomicU64`, accessed with relaxed ordering under the
//! zone lock); this type only knows the offsets.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering::Relaxed};

#[derive(Clone, Debug)]
pub struct HBitmap {
    bits: u64,
    /// (offset into the blob, words) per level; level 0 = the blocks.
    levels: Vec<(usize, usize)>,
}

/// Words needed by a bitmap of `bits` bits (all levels).
pub fn words_for(bits: u64) -> usize {
    let mut total = 0;
    let mut n = bits;
    while n > 0 {
        let w = n.div_ceil(64);
        total += w as usize;
        if w == 1 {
            break;
        }
        n = w;
    }
    total
}

impl HBitmap {
    /// A bitmap of `bits` bits whose words start at `off` in the blob (`words_for(bits)` words).
    pub fn new(bits: u64, off: usize) -> HBitmap {
        let mut levels = Vec::new();
        let mut n = bits;
        let mut at = off;
        while n > 0 {
            let w = n.div_ceil(64) as usize;
            levels.push((at, w));
            at += w;
            if w == 1 {
                break;
            }
            n = w as u64;
        }
        HBitmap { bits, levels }
    }

    pub fn test(&self, blob: &[AtomicU64], i: u64) -> bool {
        if i >= self.bits {
            return false;
        }
        let (off, _) = self.levels[0];
        blob[off + (i / 64) as usize].load(Relaxed) & (1 << (i % 64)) != 0
    }

    pub fn set(&self, blob: &[AtomicU64], i: u64) {
        if i >= self.bits {
            return;
        }
        let mut idx = i;
        for &(off, _) in &self.levels {
            let w = &blob[off + (idx / 64) as usize];
            let old = w.load(Relaxed);
            w.store(old | 1 << (idx % 64), Relaxed);
            if old != 0 {
                return; // the levels above already know this word is non-empty
            }
            idx /= 64;
        }
    }

    pub fn clear(&self, blob: &[AtomicU64], i: u64) {
        if i >= self.bits {
            return;
        }
        let mut idx = i;
        for &(off, _) in &self.levels {
            let w = &blob[off + (idx / 64) as usize];
            let new = w.load(Relaxed) & !(1 << (idx % 64));
            w.store(new, Relaxed);
            if new != 0 {
                return;
            }
            idx /= 64;
        }
    }

    /// The lowest set bit, if any.
    pub fn first(&self, blob: &[AtomicU64]) -> Option<u64> {
        let top = self.levels.len().checked_sub(1)?;
        let mut idx = 0u64; // word index at the current level
        for lvl in (0..=top).rev() {
            let (off, words) = self.levels[lvl];
            if idx as usize >= words {
                return None;
            }
            let w = blob[off + idx as usize].load(Relaxed);
            if w == 0 {
                return None;
            }
            idx = idx * 64 + w.trailing_zeros() as u64;
        }
        (idx < self.bits).then_some(idx)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec::Vec;

    fn blob(n: usize) -> Vec<AtomicU64> {
        (0..n).map(|_| AtomicU64::new(0)).collect()
    }

    #[test]
    fn sizes() {
        assert_eq!(words_for(0), 0);
        assert_eq!(words_for(1), 1);
        assert_eq!(words_for(64), 1);
        assert_eq!(words_for(65), 2 + 1);
        assert_eq!(words_for(64 * 64), 64 + 1);
        assert_eq!(words_for(64 * 64 + 1), 65 + 2 + 1);
    }

    #[test]
    fn set_clear_first_over_levels() {
        for bits in [1u64, 63, 64, 65, 4096, 4097, 300_000] {
            let b = blob(words_for(bits) + 3);
            let h = HBitmap::new(bits, 3);
            assert_eq!(h.first(&b), None);
            let picks = [bits - 1, bits / 2, bits / 3, 0];
            for &p in &picks {
                h.set(&b, p);
                assert!(h.test(&b, p));
            }
            assert_eq!(h.first(&b), Some(0));
            h.clear(&b, 0);
            let mut rest: Vec<u64> = picks.iter().copied().filter(|&p| p != 0).collect();
            rest.sort();
            rest.dedup();
            for p in rest {
                assert_eq!(h.first(&b), Some(p), "bits={bits}");
                h.clear(&b, p);
            }
            assert_eq!(h.first(&b), None);
            assert!(b[..3].iter().all(|w| w.load(Relaxed) == 0), "words before the offset untouched");
            h.set(&b, bits); // out of range: ignored
            assert_eq!(h.first(&b), None);
        }
    }
}
