//! Slab geometry and the heap's size classes (pure math, host-tested).
//!
//! A slab is a buddy block of `PAGE << order` bytes aligned to its size. Objects sit at the start, one every
//! `size` bytes, so object `i` is at `base + i * size` and keeps the alignment of the lowest set bit of
//! `size`; the header (`raw::Hdr`), two bitmaps (`taken`, `live`) and the magic fill the end of the slab. The allocator never
//! writes into object memory (no free list inside objects), so a use-after-free write cannot corrupt it.

use crate::PAGE;

/// Bytes of `raw::Hdr` (before the two bitmaps) plus the magic word at the slab's end.
pub const HDR_BASE: usize = 32;
/// A slab should hold at least this many objects (fewer slab creations, less header waste); a tuning value.
pub const MIN_OBJECTS: usize = 8;
/// Largest slab: 128 KiB. Objects that do not fit at least once are not slab objects (the heap sends
/// everything above 2048 bytes to whole pages anyway).
pub const MAX_SLAB_ORDER: u32 = 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
    /// Object stride (size rounded up to the alignment, at least 8).
    pub size: usize,
    /// Slab = `PAGE << order` bytes.
    pub order: u32,
    /// Objects per slab.
    pub count: usize,
    /// u64 words of each bitmap.
    pub words: usize,
    /// Offset of the header from the slab start; header, bitmaps and magic fill the slab to its end, so the
    /// magic sits at `slab_bytes - 8` for every geometry of an order.
    pub hdr_off: usize,
}

impl Geometry {
    pub const fn slab_bytes(&self) -> usize {
        (PAGE as usize) << self.order
    }
}

/// (count, words, hdr_off) for objects of `size` in a slab of `slab` bytes; count 0 if none fits.
const fn fit(slab: usize, size: usize) -> (usize, usize, usize) {
    if slab < HDR_BASE + 16 + size {
        return (0, 0, 0);
    }
    let mut n = (slab - HDR_BASE) / size;
    while n > 0 {
        let words = n.div_ceil(64);
        let hdr = HDR_BASE + 16 * words;
        if hdr <= slab {
            let off = slab - hdr; // both multiples of 8
            if n * size <= off {
                return (n, words, off);
            }
            let m = off / size;
            n = if m < n { m } else { n - 1 };
        } else {
            n -= 1;
        }
    }
    (0, 0, 0)
}

/// The geometry for objects of `size` bytes aligned to `align` (a power of two up to a page), or None if
/// the alignment is invalid or the object does not fit a slab of MAX_SLAB_ORDER.
pub const fn geometry(size: usize, align: usize) -> Option<Geometry> {
    if !align.is_power_of_two() || align > PAGE as usize || size == 0 {
        return None;
    }
    let base = if size < 8 { 8 } else { size };
    let size = (base + align - 1) & !(align - 1);
    let mut order = 0;
    let mut best: Option<Geometry> = None;
    while order <= MAX_SLAB_ORDER {
        let (count, words, hdr_off) = fit((PAGE as usize) << order, size);
        if count > 0 {
            let g = Geometry { size, order, count, words, hdr_off };
            if count >= MIN_OBJECTS {
                return Some(g);
            }
            if best.is_none() {
                best = Some(g);
            }
        }
        order += 1;
    }
    best
}

/// The heap's size classes (Linux kmalloc's set): object sizes of the class caches.
pub const CLASSES: [usize; 11] = [8, 16, 32, 64, 96, 128, 192, 256, 512, 1024, 2048];
pub const NCLASSES: usize = CLASSES.len();
/// Largest small allocation; anything bigger (or aligned above it) is whole pages from the buddy allocator.
pub const SMALL_MAX: usize = 2048;

/// Smallest class for each size in 8-byte steps (index = size.div_ceil(8)).
const SIZE_TO_CLASS: [u8; SMALL_MAX / 8 + 1] = {
    let mut t = [0u8; SMALL_MAX / 8 + 1];
    let mut i = 0;
    while i < t.len() {
        let mut c = 0;
        while CLASSES[c] < i * 8 {
            c += 1;
        }
        t[i] = c as u8;
        i += 1;
    }
    t
};

/// The class serving (size, align), or None for a large allocation.
pub fn class_of(size: usize, align: usize) -> Option<usize> {
    if size > SMALL_MAX || align > SMALL_MAX {
        return None;
    }
    let mut c = SIZE_TO_CLASS[size.max(1).div_ceil(8)] as usize;
    // An object of class size s is aligned to the lowest set bit of s (96 -> 32, 192 -> 64).
    while 1 << CLASSES[c].trailing_zeros() < align {
        c += 1;
    }
    Some(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn class_geometries() {
        let want = [(8, 0, 492), (16, 0, 250), (32, 0, 126), (64, 0, 63), (96, 0, 42), (128, 0, 31), (192, 0, 21), (256, 0, 15), (512, 1, 15), (1024, 2, 15), (2048, 3, 15)];
        for (i, &(size, order, count)) in want.iter().enumerate() {
            let g = geometry(CLASSES[i], 8).unwrap();
            assert_eq!((g.size, g.order, g.count), (size, order, count), "class {size}");
            assert!(g.count * g.size <= g.hdr_off && g.hdr_off + HDR_BASE + 16 * g.words == g.slab_bytes());
            assert_eq!(g.hdr_off % 8, 0);
            assert!(g.words * 64 >= g.count);
        }
    }

    #[test]
    fn geometry_every_size_and_alignment() {
        for size in 1..=40_000usize {
            for align in [1usize, 8, 16, 64, 512, 4096] {
                let g = geometry(size, align).unwrap();
                assert!(g.size >= size && g.size.is_multiple_of(align) && g.count >= 1);
                assert!(g.count * g.size <= g.hdr_off);
                assert!(g.hdr_off + HDR_BASE + 16 * g.words == g.slab_bytes(), "magic at the slab end");
                assert!(g.count >= MIN_OBJECTS || g.order == MAX_SLAB_ORDER || geometry_order_ok(g));
            }
        }
        assert!(geometry(8, 3).is_none() && geometry(8, 8192).is_none() && geometry(0, 8).is_none());
        assert!(geometry(200_000, 8).is_none(), "bigger than the largest slab");
    }

    /// A geometry below MIN_OBJECTS is only chosen when no order up to the maximum reaches it.
    fn geometry_order_ok(g: Geometry) -> bool {
        (g.order..=MAX_SLAB_ORDER).all(|o| fit((PAGE as usize) << o, g.size).0 < MIN_OBJECTS)
    }

    #[test]
    fn classes_for_sizes_and_alignments() {
        assert_eq!(class_of(1, 1), Some(0));
        assert_eq!(class_of(8, 8), Some(0));
        assert_eq!(class_of(8, 16), Some(1));
        assert_eq!(class_of(9, 8), Some(1));
        assert_eq!(class_of(65, 8), Some(4)); // 96
        assert_eq!(class_of(65, 64), Some(5)); // 96 is only 32-aligned -> 128
        assert_eq!(class_of(150, 64), Some(6)); // 192 is 64-aligned
        assert_eq!(class_of(150, 128), Some(7));
        assert_eq!(class_of(2048, 2048), Some(10));
        assert_eq!(class_of(2049, 8), None);
        assert_eq!(class_of(16, 4096), None);
        for size in 1..=SMALL_MAX {
            for align in [1, 2, 4, 8, 16, 32, 64, 128, 256, 512, 1024, 2048] {
                let c = class_of(size, align).unwrap();
                assert!(CLASSES[c] >= size && CLASSES[c].is_multiple_of(align));
                assert!(c == 0 || CLASSES[c - 1] < size || !CLASSES[c - 1].is_multiple_of(align), "{size}/{align}: smaller class fits");
            }
        }
    }
}
