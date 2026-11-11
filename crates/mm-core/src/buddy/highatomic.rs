//! The HighAtomic reserve (Linux 7.x mm/page_alloc.c `reserve_highatomic_pageblock`, `rmqueue_buddy` with
//! `ALLOC_HIGHATOMIC` and `ALLOC_NON_BLOCK`, `__zone_watermark_unusable_free`, `nr_free_highatomic`).
//!
//! Mini-PC 11.11.2026: once the page cache has filled memory, free memory sits between the low and the high
//! watermark in pieces of order 0..2. An allocation that may not reclaim (a spin lock held, an interrupt handler)
//! and needs frames in a row then finds none: 1688 failed frame allocations within minutes of an 8-reader load, two
//! kernel panics (heap blocks of 36 KiB and 17 KiB under a spin lock). Callers that may wait reclaim and compact
//! (`reclaim`, `compact`, the patient heap); for the others a few pageblocks are kept apart.
//!
//! Made once, when the boot allocator is built and memory is whole (`reserve_highatomic`). Linux retypes a
//! pageblock only after a high-order atomic allocation succeeded; that needs a free block at the moment there is
//! none, and a failure there is a dropped packet, here a panic.
//!
//! A pageblock of the reserve has type `MigrateType::HighAtomic`: its free blocks lie in that type's bitmaps, and
//! merging stops below the pageblock order (`Buddy::free_block`), so it never joins the untyped lists of free
//! pageblocks that any type may take. No type's `fallbacks` name it (no claim, no steal), its freed frames skip the
//! per-CPU lists (`free_tagged`; Linux `free_frozen_pages` puts them on the Movable list, from where any
//! allocation takes them), compaction passes it (`chunk_reserved`), and its free frames do not count where free
//! memory is judged (`ZoneState::usable_free`, `FrameStats::usable_free_bytes`): kswapd would otherwise see
//! the reserve as memory above its high watermark and stop reclaiming.
//!
//! Who takes from it (`alloc_highatomic`, the last step of `FrameAlloc::alloc`, after the ordinary lists, the
//! per-CPU drain and the grace wait found nothing): an unmovable kind where the caller may not reclaim, any order
//! below the pageblock order, a single frame included (Linux lets `ALLOC_NON_BLOCK` order 0 in the same way:
//! failing now is worse than failing a larger one later). Never program or cache memory.
//!
//! Not done here: giving the reserve back when everything else failed (Linux `unreserve_highatomic_pageblock`) and
//! making it again after long-lived blocks settled in it. `mm.frames.highatomic_free` shows what is left.

use super::{Buddy, MigrateType, Region, ZoneState, ORDERS, PAGEBLOCK_FRAMES, PAGEBLOCK_ORDER, ZONES};
use crate::meta::{Meta, State};
use crate::{reclaim, Kind, Zone, MAX_ORDER, PAGE};
use core::sync::atomic::Ordering::Relaxed;

klog::counter!(pub(super) PAGEBLOCK_HIGHATOMIC, "mm.frames.pageblock_highatomic");
klog::counter!(HIGHATOMIC_ALLOCS, "mm.frames.highatomic_allocs");
// Free frames of the reserve now: it goes down and up (as the pageblock counters do).
klog::counter!(HIGHATOMIC_FREE, "mm.frames.highatomic_free");

/// A zone below this many pageblocks (128 MiB) has no reserve.
const MIN_ZONE_PAGEBLOCKS: u64 = 64;
/// The reserve's share of a zone's frames: 1/256 (24 GiB: 96 MiB).
const SHARE: u64 = 256;

/// Pageblocks of the reserve for a zone of `managed` frames: its share rounded to pageblocks, at least 2, at most
/// Linux's cap (`reserve_highatomic_pageblock`: 1 % of the zone, rounded up to a pageblock).
pub fn reserve_pageblocks(managed: u64) -> u64 {
    if managed / PAGEBLOCK_FRAMES < MIN_ZONE_PAGEBLOCKS {
        return 0;
    }
    let share = (managed / SHARE + PAGEBLOCK_FRAMES / 2) / PAGEBLOCK_FRAMES;
    share.max(2).min((managed / 100).div_ceil(PAGEBLOCK_FRAMES))
}

/// Whether an allocation of `kind` made now may take from the reserve. The heap calls with its own locks held, so
/// it says what its caller may do: it holds a `reclaim::keep_reserve` slot while it may still reclaim, and none
/// under a spin lock or on its last try, where the next answer is null.
fn may_take(kind: Kind) -> bool {
    match kind {
        Kind::Heap => !reclaim::keeps_reserve(),
        k => MigrateType::of(k) == MigrateType::Unmovable && !reclaim::context_allows(),
    }
}

impl ZoneState {
    /// Linux `__zone_watermark_unusable_free`: the free frames an ordinary allocation can get.
    #[inline]
    pub(super) fn usable_free(&self) -> u64 {
        self.free_frames - self.free_highatomic
    }

    /// `free_blocks` without the reserve's.
    pub(super) fn usable_blocks(&self) -> [u64; ORDERS] {
        let reserved = &self.by_type[MigrateType::HighAtomic as usize];
        core::array::from_fn(|o| self.free_blocks[o] - reserved[o])
    }

    /// A free block of `order` or more outside the reserve.
    pub(super) fn has_usable_block(&self, order: u32) -> bool {
        let reserved = &self.by_type[MigrateType::HighAtomic as usize];
        (order as usize..ORDERS).any(|o| self.free_blocks[o] > reserved[o])
    }
}

impl Buddy {
    /// Makes each zone's reserve (`reserve_pageblocks` of its frames); returns the pageblocks each zone has then.
    /// For a fresh allocator: only wholly free pageblocks are taken.
    pub fn reserve_highatomic(&self) -> [u64; ZONES] {
        core::array::from_fn(|z| self.reserve_highatomic_in(z, reserve_pageblocks(self.managed[z].load(Relaxed))))
    }

    /// Brings zone `zone`'s reserve up to `pageblocks` wholly free pageblocks, from the top of the zone down: the
    /// lowest free block is handed out first (`first_free`), so the top is used last, and address limits (DMA) are
    /// low. Returns the zone's reserve in pageblocks afterwards.
    pub fn reserve_highatomic_in(&self, zone: usize, pageblocks: u64) -> u64 {
        let mut zs = self.zones[zone].lock();
        for r in self.regions.iter().rev().filter(|r| r.zone == zone) {
            let mut pb = r.end & !(PAGEBLOCK_FRAMES - 1);
            while zs.pageblocks[MigrateType::HighAtomic as usize] < pageblocks && pb >= r.start + PAGEBLOCK_FRAMES {
                pb -= PAGEBLOCK_FRAMES;
                self.reserve_pageblock(r, &mut zs, pb);
            }
        }
        zs.pageblocks[MigrateType::HighAtomic as usize]
    }

    /// The pageblock at `pb`, if it lies in one free block, becomes HighAtomic: the block is split around it
    /// (`expand`'s way) and the pageblock goes back as its two halves, which `free_block` never merges again.
    fn reserve_pageblock(&self, r: &Region, zs: &mut ZoneState, pb: u64) -> bool {
        if !r.holds_pageblock(pb) || self.pb_type(r, pb) == MigrateType::HighAtomic {
            return false;
        }
        let head = |o: u32| pb & !((1u64 << o) - 1);
        let Some(mut o) = (PAGEBLOCK_ORDER..=MAX_ORDER).find(|&o| r.holds(head(o), o) && self.get_meta(r, head(o)) == Meta::new(State::Free, o, 0)) else {
            return false;
        };
        self.remove_free(r, zs, head(o), o);
        while o > PAGEBLOCK_ORDER {
            o -= 1;
            self.insert_free(r, zs, head(o) ^ (1 << o), o);
        }
        self.set_pb_type(r, zs, pb, MigrateType::HighAtomic);
        for half in [pb, pb + PAGEBLOCK_FRAMES / 2] {
            self.insert_free(r, zs, half, PAGEBLOCK_ORDER - 1);
        }
        true
    }

    /// The reserve's free frames changed by `frames` (zone lock held; Linux `nr_free_highatomic`).
    #[cold]
    pub(super) fn reserve_moved(&self, zs: &mut ZoneState, frames: u64, back: bool) {
        if back {
            zs.free_highatomic += frames;
        } else {
            zs.free_highatomic -= frames;
        }
        if self.global_reports {
            HIGHATOMIC_FREE.add(if back { frames } else { frames.wrapping_neg() });
        }
    }

    /// Linux `rmqueue_buddy`'s last step (`__rmqueue_smallest(zone, order, MIGRATE_HIGHATOMIC)` for
    /// `ALLOC_HIGHATOMIC`, and for `ALLOC_NON_BLOCK` once `__rmqueue` found nothing): a block of `order` out of the
    /// reserve for a caller that `may_take`, the zones in `take_free`'s order.
    #[cold]
    pub(super) fn alloc_highatomic(&self, order: u32, zone: Zone, kind: Kind) -> Option<u64> {
        if order >= PAGEBLOCK_ORDER || !may_take(kind) {
            return None;
        }
        let tries: &[usize] = match zone {
            Zone::Dma32 => &[0],
            Zone::Normal => &[1, 0],
        };
        for &z in tries {
            // As in `take_free`: the block's state and `used` move together.
            let counted = ksync::PreemptGuard::new();
            let mut zs = self.zones[z].lock();
            let Some((ri, pfn)) = self.smallest_typed(z, &mut zs, order, MigrateType::HighAtomic, u64::MAX) else { continue };
            self.set_meta(&self.regions[ri], pfn, Meta::new(State::Alloc, order, kind as usize));
            drop(zs);
            self.used[kind as usize].fetch_add(1 << order, Relaxed);
            drop(counted);
            if self.global_reports {
                HIGHATOMIC_ALLOCS.inc();
            }
            return Some(pfn * PAGE);
        }
        None
    }

    /// Whether [at, at + chunk) (inside `r`) touches a pageblock of the reserve.
    pub(super) fn chunk_reserved(&self, r: &Region, at: u64, chunk: u64) -> bool {
        (at..at + chunk).step_by(PAGEBLOCK_FRAMES as usize).any(|p| self.pb_type(r, p) == MigrateType::HighAtomic)
    }
}
