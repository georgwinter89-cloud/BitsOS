//! The slab header in slab memory: the only place the slab allocator dereferences raw addresses.
//!
//! Layout at `base + geo.hdr_off`: `Hdr`, then `words` u64 of the `taken` bitmap (objects out of the slab:
//! held by a user or parked in a per-CPU magazine; changed under the cache lock), then `words` u64 of the
//! `live` bitmap (objects a user holds; changed with atomic bit operations without a lock, which is what
//! makes a double free exact even for objects that go to a magazine), then the magic in the last 8 bytes of
//! the slab. Bits past `count` in `taken` are set at creation, so a search never returns an object outside
//! the slab.
//!
//! The magic sits at the same place in every slab of an order, whatever the object size, so for a block the
//! page source confirmed as a slab of that order (`PageSrc::owns_slab`) the word read there is always a
//! slab's magic, never object data: that is what makes "which cache owns this slab" exact.

use super::geo::{Geometry, HDR_BASE};
use crate::PAGE;
use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering::Relaxed};

#[repr(C)]
pub struct Hdr {
    /// Neighbours in the cache's list (slab base addresses, 0 = none). Checked before use (`Cache::neighbour`):
    /// a stray write past the last object can reach them.
    pub prev: AtomicUsize,
    pub next: AtomicUsize,
    /// Objects out of the slab (= set bits of `taken` below `count`).
    pub taken: AtomicU32,
    /// Which list of the cache the slab is on (`cache::PARTIAL/FULL/EMPTY`).
    pub list: AtomicU32,
}

/// The header plus the magic word at the slab's end.
const _: () = assert!(core::mem::size_of::<Hdr>() + 8 == HDR_BASE);

pub const SLAB_MAGIC: u64 = 0x534C_4142_4D4D_3321;

/// A slab's header and bitmaps.
#[derive(Clone, Copy)]
pub struct Slab {
    pub base: usize,
    pub hdr: &'static Hdr,
    /// `SLAB_MAGIC ^ cache address` in the slab's last 8 bytes: ties the slab to its cache.
    pub magic: &'static AtomicU64,
    pub taken: &'static [AtomicU64],
    pub live: &'static [AtomicU64],
}

impl Slab {
    /// The magic word of the slab of `order` at `base`.
    ///
    /// # Safety
    /// `[base, base + PAGE << order)` is a slab block of `order` the page source handed out to a cache and has
    /// not taken back (`PageSrc::owns_slab`): its last 8 bytes are a magic word in every geometry.
    pub unsafe fn magic_at(base: usize, order: u32) -> &'static AtomicU64 {
        let m = base + ((PAGE as usize) << order) - 8;
        // SAFETY: the caller's contract; 8-aligned, and any bit pattern is a valid atomic.
        unsafe { &*(m as *const AtomicU64) }
    }

    /// # Safety
    /// `[base, base + geo.slab_bytes())` is a slab of geometry `geo` that this cache owns (created by `init`
    /// and not yet returned): checked with `PageSrc::owns_slab` and the word at `magic_at`, or a base taken
    /// from the cache's own lists or objects.
    pub unsafe fn at(base: usize, geo: &Geometry) -> Slab {
        let h = base + geo.hdr_off;
        let bits = h + core::mem::size_of::<Hdr>();
        // SAFETY: the caller's contract: header and bitmaps lie inside the slab in front of the magic,
        // 8-aligned (hdr_off is a multiple of 8, base is page-aligned) and hold plain integers.
        unsafe {
            Slab {
                base,
                hdr: &*(h as *const Hdr),
                magic: Slab::magic_at(base, geo.order),
                taken: core::slice::from_raw_parts(bits as *const AtomicU64, geo.words),
                live: core::slice::from_raw_parts((bits + 8 * geo.words) as *const AtomicU64, geo.words),
            }
        }
    }

    /// Writes a fresh header for a new slab: no object taken or live, padding bits taken.
    ///
    /// # Safety
    /// `[base, base + geo.slab_bytes())` was just handed to the cache by the page source, aligned to its
    /// size, and nobody else uses it.
    pub unsafe fn init(base: usize, geo: &Geometry, magic: u64, list: u32) -> Slab {
        let len = geo.slab_bytes() - geo.hdr_off;
        // SAFETY: the caller's contract; header, bitmaps and magic fill [hdr_off, slab end) (geometry::fit).
        unsafe { core::ptr::write_bytes((base + geo.hdr_off) as *mut u8, 0, len) };
        // SAFETY: as above, now initialised.
        let s = unsafe { Slab::at(base, geo) };
        s.magic.store(magic, Relaxed);
        s.hdr.list.store(list, Relaxed);
        let tail = geo.count % 64;
        if tail != 0 {
            s.taken[geo.words - 1].store(!0u64 << tail, Relaxed);
        }
        s
    }

    /// Index of the first object not taken, if any (cache lock held).
    pub fn first_free(&self) -> Option<usize> {
        self.taken.iter().enumerate().find_map(|(w, word)| {
            let v = word.load(Relaxed);
            (v != !0).then(|| w * 64 + v.trailing_ones() as usize)
        })
    }

    /// Sets (true) or clears a bit; returns its old value.
    pub fn flip(map: &[AtomicU64], idx: usize, set: bool) -> bool {
        let bit = 1u64 << (idx % 64);
        let old = if set { map[idx / 64].fetch_or(bit, Relaxed) } else { map[idx / 64].fetch_and(!bit, Relaxed) };
        old & bit != 0
    }

    pub fn test(map: &[AtomicU64], idx: usize) -> bool {
        map[idx / 64].load(Relaxed) & (1 << (idx % 64)) != 0
    }

    /// Set bits below `count` (the padding bits of `taken` are not objects).
    pub fn count_bits(map: &[AtomicU64], count: usize) -> usize {
        map.iter()
            .enumerate()
            .map(|(w, word)| {
                let v = word.load(Relaxed);
                let valid = if (w + 1) * 64 <= count { !0u64 } else { (1u64 << (count - w * 64)) - 1 };
                (v & valid).count_ones() as usize
            })
            .sum()
    }
}
