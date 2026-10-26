//! Exact page runs (Linux `alloc_pages_exact`): the heap's large allocations take `pages` frames, not the next
//! power of two. A block of the covering order is split in place and the tail pieces go back at once; what
//! stays is the binary decomposition of `pages` at rising addresses (e.g. 5 pages = a 4-page block, then a
//! 1-page block), each piece an allocated block; the pieces after the first carry `TAG_TAIL`, so only a free
//! of the whole run from its start can free them. `free_exact` first claims every piece (tag -> `TAG_BUSY`,
//! one compare-and-swap each) and only then frees them, so a wrong page count, a pointer to a later piece or
//! to a slab, or two CPUs freeing the same run at once are refused before anything changes.
//!
//! Slabs of the heap are blocks tagged `TAG_SLAB` (`alloc_tagged`, `free_tagged`): the heap checks a freed
//! pointer's slab against that tag before it reads the slab header (`is_slab`).

use super::Buddy;
use crate::meta::{Meta, State, TAG_BUSY, TAG_NONE, TAG_TAIL};
use crate::{FrameAlloc, Kind, Zone, MAX_ORDER, PAGE};

/// Smallest order whose block holds `pages` frames aligned to `align_pages` (a power of two); None above
/// MAX_ORDER.
pub fn exact_order(pages: u64, align_pages: u64) -> Option<u32> {
    if pages == 0 || !align_pages.is_power_of_two() {
        return None;
    }
    let order = (64 - (pages - 1).leading_zeros()).max(align_pages.trailing_zeros());
    (order <= MAX_ORDER).then_some(order)
}

/// The pieces (phys, order) of an exact run of `pages` starting at `phys`, in address order.
pub fn pieces(phys: u64, pages: u64) -> impl Iterator<Item = (u64, u32)> {
    let mut at = phys;
    (0..64u32).rev().filter(move |b| pages & (1 << b) != 0).map(move |b| {
        let p = at;
        at += PAGE << b;
        (p, b)
    })
}

impl Buddy {
    /// `pages` contiguous frames aligned to `align_pages` frames (a power of two), charged to `kind`.
    pub fn alloc_exact(&self, pages: u64, align_pages: u64, zone: Zone, kind: Kind) -> Option<u64> {
        let order = exact_order(pages, align_pages)?;
        let phys = self.alloc(order, zone, kind)?;
        self.make_run(phys, order, pages, kind)
    }

    /// `alloc_exact` for a run that ends at or below `limit` (bytes, exclusive; DMA address masks). The run
    /// lies at the start of its covering block, so it never crosses a multiple of that block's size.
    pub fn alloc_exact_below(&self, pages: u64, align_pages: u64, limit: u64, kind: Kind) -> Option<u64> {
        let order = exact_order(pages, align_pages)?;
        let phys = self.alloc_below(order, limit, kind)?;
        self.make_run(phys, order, pages, kind)
    }

    /// Turns the just-allocated block (phys, order) into the exact run of `pages` frames at its start.
    fn make_run(&self, phys: u64, order: u32, pages: u64, kind: Kind) -> Option<u64> {
        self.trim(phys, order, pages, kind);
        let r = self.region_of(phys / PAGE)?;
        for (p, o) in pieces(phys, pages).skip(1) {
            // The run is the caller's (just allocated): only its own piece heads are written.
            self.set_meta(r, p / PAGE, Meta::new(State::Alloc, o, kind as usize).with_tag(TAG_TAIL));
        }
        Some(phys)
    }

    /// Frees a run from `alloc_exact` (the alignment does not matter here: the pieces depend only on `pages`).
    /// False, with nothing changed, unless [phys, phys + pages) is exactly such a run allocated to `kind` that
    /// nobody else is freeing.
    pub fn free_exact(&self, phys: u64, pages: u64, kind: Kind) -> bool {
        if !phys.is_multiple_of(PAGE) || pages == 0 {
            return false;
        }
        let want = |i: usize| if i == 0 { TAG_NONE } else { TAG_TAIL };
        let undo = |n: usize| {
            for (j, (q, k)) in pieces(phys, pages).take(n).enumerate() {
                self.retag(q, k, kind, TAG_BUSY, want(j));
            }
        };
        let n = pieces(phys, pages).count();
        for (i, (p, o)) in pieces(phys, pages).enumerate() {
            if !self.retag(p, o, kind, want(i), TAG_BUSY) {
                undo(i);
                return false;
            }
        }
        // A tail piece right behind the last one belongs to this run (runs are contiguous and start with an
        // untagged piece): the run is longer than `pages`.
        if self.tail_at(phys + pages * PAGE) {
            undo(n);
            return false;
        }
        for (p, o) in pieces(phys, pages) {
            self.free_tagged(p, o, kind, TAG_BUSY);
        }
        true
    }

    /// Is [phys, phys + pages) an exact run that is allocated to `kind` right now (a lock-free look, tests)?
    pub fn run_allocated(&self, phys: u64, pages: u64, kind: Kind) -> bool {
        phys.is_multiple_of(PAGE)
            && pages > 0
            && pieces(phys, pages).enumerate().all(|(i, (p, o))| {
                self.head(p, o).is_some_and(|m| m == Meta::new(State::Alloc, o, kind as usize).with_tag(if i == 0 { TAG_NONE } else { TAG_TAIL }))
            })
    }

    /// A block of `order` for `kind` carrying owner tag `tag` from the start (the heap's slabs: `TAG_SLAB`).
    pub(crate) fn alloc_tagged(&self, order: u32, zone: Zone, kind: Kind, tag: u8) -> Option<u64> {
        let phys = self.alloc(order, zone, kind)?;
        if self.retag(phys, order, kind, TAG_NONE, tag) {
            return Some(phys);
        }
        // Only if someone freed the block in between with a stale pointer: it is not ours any more.
        None
    }

    /// Is `phys` the head of an allocated block of `order` for `kind` carrying `tag`? Reads only this
    /// allocator's own state, so it is safe for any address (holes and MMIO are outside every region).
    pub(crate) fn is_tagged(&self, phys: u64, order: u32, kind: Kind, tag: u8) -> bool {
        self.head(phys, order).is_some_and(|m| m == Meta::new(State::Alloc, order, kind as usize).with_tag(tag))
    }

    /// Is the frame at `phys` the head of a later piece of a run?
    fn tail_at(&self, phys: u64) -> bool {
        let pfn = phys / PAGE;
        self.region_of(pfn).is_some_and(|r| {
            let m = self.get_meta(r, pfn);
            m.state == State::Alloc && m.tag == TAG_TAIL
        })
    }

    /// The state of the block head at `phys` for a block of `order`, if such a block fits a region there.
    fn head(&self, phys: u64, order: u32) -> Option<Meta> {
        if order > crate::MAX_ORDER || !phys.is_multiple_of(PAGE << order) {
            return None;
        }
        let pfn = phys / PAGE;
        let r = self.region_of(pfn).filter(|r| r.holds(pfn, order))?;
        Some(self.get_meta(r, pfn))
    }

    /// Changes the owner tag of the allocated block (phys, order, kind) from `from` to `to` in one
    /// compare-and-swap; false if the block is not in that state.
    fn retag(&self, phys: u64, order: u32, kind: Kind, from: u8, to: u8) -> bool {
        if self.head(phys, order).is_none() {
            return false;
        }
        let (pfn, base) = (phys / PAGE, Meta::new(State::Alloc, order, kind as usize));
        self.region_of(pfn).is_some_and(|r| self.cas_meta(r, pfn, base.with_tag(from), base.with_tag(to)).is_ok())
    }

    /// Keeps the first `keep` frames of the allocated block (phys, order) as the pieces `pieces` names and
    /// frees the rest. The block is the caller's (just allocated), so its frames' states change without the
    /// zone lock: only the head and the new piece heads are written, and the merge code only looks at heads
    /// that read `Free`.
    fn trim(&self, phys: u64, order: u32, keep: u64, kind: Kind) {
        let (mut pfn, mut o, mut rem) = (phys / PAGE, order, keep);
        let Some(r) = self.region_of(pfn) else { return };
        while rem < 1 << o {
            o -= 1;
            let half = 1u64 << o;
            self.set_meta(r, pfn, Meta::new(State::Alloc, o, kind as usize));
            self.set_meta(r, pfn + half, Meta::new(State::Alloc, o, kind as usize));
            if rem <= half {
                self.free((pfn + half) * PAGE, o, kind);
            } else {
                rem -= half;
                pfn += half;
            }
        }
    }
}
