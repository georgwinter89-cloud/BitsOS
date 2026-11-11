//! W35 MM-1: grouping by mobility (Linux 7.x mm/page_alloc.c `__rmqueue`, `__rmqueue_smallest`, `__rmqueue_claim`,
//! `try_to_claim_block`, `__rmqueue_steal`, `find_suitable_fallback`, `fallbacks[]`, `__move_freepages_block`,
//! `set_pageblock_migratetype`; include/linux/mmzone.h `enum migratetype`, include/linux/pageblock-flags.h).
//!
//! Every 2 MiB pageblock has a migrate type in a side array of the region (2 bits each, in the metadata blob,
//! sized from the memory map). Free blocks below the pageblock order lie in one bitmap per order and type, the type
//! of their pageblock; whole free pageblocks and larger blocks lie in one bitmap per order with no type: a free
//! pageblock holds nothing, so it may become any type without mixing (Linux keeps it on a typed list and claims it
//! whole on a fallback, `try_to_claim_block` with `current_order >= pageblock_order`: the same result). A block
//! taken from those becomes the asker's type, only the pageblocks it covers (MAX_ORDER is 18 here: a 1 GiB block
//! split for one frame changes one pageblock, the rest stays free and typeless).
//!
//! An allocation of type T takes, in this order (`rmqueue`): the smallest free block of T, a free pageblock (or
//! larger), then a block of another type by `fallbacks[T]`: from the largest order down it claims the block's
//! whole pageblock for T when the order is at least half the pageblock order, or T is not Movable, and at least
//! half the pageblock is free or alike (`try_to_claim_block`); else (Movable below order 4, or no claim worked) it
//! steals the smallest block of the other type and leaves the pageblock as it was (`__rmqueue_steal`). So Movable
//! memory falls into an Unmovable pageblock only after its own, the free pageblocks and Reclaimable are used up.
//!
//! A fourth type, HighAtomic, is the reserve for allocations that cannot reclaim (`highatomic`): no kind maps to
//! it, no type falls back into it, and its pageblocks never join the untyped lists.
//!
//! The side array is rebuilt at every boot and Reload with the buddy: the blob starts zeroed, which reads Movable
//! (Linux's `memmap_init` sets MIGRATE_MOVABLE too), and nothing survives a Reload but boot modules (reserved,
//! never typed), so there is no state to hand over; kernel allocations claim their pageblocks again as they come.

use super::{Buddy, Region, ZoneState};
use crate::meta::State;
use crate::{Kind, MAX_ORDER};

/// Linux `pageblock_order` on x86-64 (HPAGE_PMD_ORDER): 2 MiB.
pub const PAGEBLOCK_ORDER: u32 = 9;
pub(crate) const PB: usize = PAGEBLOCK_ORDER as usize;
/// Frames of a pageblock.
pub const PAGEBLOCK_FRAMES: u64 = 1 << PAGEBLOCK_ORDER;
/// The types kept apart here (CMA and Isolate do not exist in BitsOS).
pub const MIGRATE_TYPES: usize = 4;
/// Linux `MIGRATE_PCPTYPES`: the types with per-CPU lists (HighAtomic has none: `Buddy::free_tagged`).
pub const PCP_TYPES: usize = 3;
/// Pageblock types per side-array word.
const PER_WORD: u64 = 32;

klog::counter!(FALLBACK_STEAL, "mm.frames.fallback_steal");
klog::counter!(FALLBACK_WHOLE_BLOCK, "mm.frames.fallback_whole_block");
klog::counter!(PAGEBLOCK_UNMOVABLE, "mm.frames.pageblock_unmovable");
klog::counter!(PAGEBLOCK_MOVABLE, "mm.frames.pageblock_movable");
klog::counter!(PAGEBLOCK_RECLAIMABLE, "mm.frames.pageblock_reclaimable");

/// Linux `enum migratetype` (the first four, in Linux's order: the /proc/pagetypeinfo rows).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum MigrateType {
    Unmovable = 0,
    Movable = 1,
    /// No kind maps here today: the kernel heap is one heap of shared slabs, none of them a cache whose objects
    /// can be given back by frame (Linux's dentry and inode caches are what fills Reclaimable).
    Reclaimable = 2,
    /// The reserve for allocations that cannot reclaim (`highatomic`). No kind maps here either.
    HighAtomic = 3,
}

impl MigrateType {
    pub const ALL: [MigrateType; MIGRATE_TYPES] = [MigrateType::Unmovable, MigrateType::Movable, MigrateType::Reclaimable, MigrateType::HighAtomic];

    /// User memory and the page cache can be moved or dropped (MM-2/MM-3); everything the kernel holds by address
    /// (heap slabs and runs, the scatter fallback's single frames, page tables, stacks, DMA, drivers) cannot.
    pub fn of(kind: Kind) -> MigrateType {
        match kind {
            Kind::User | Kind::PageCache => MigrateType::Movable,
            Kind::Heap | Kind::PageTable | Kind::Stack | Kind::Dma | Kind::Driver | Kind::Other => MigrateType::Unmovable,
        }
    }

    /// Linux `migratetype_names`.
    pub fn name(self) -> &'static str {
        ["Unmovable", "Movable", "Reclaimable", "HighAtomic"][self as usize]
    }

    /// Linux 7.x `fallbacks[]`: HighAtomic is in no row, so no type claims or steals from the reserve.
    fn fallbacks(self) -> &'static [MigrateType] {
        match self {
            MigrateType::Unmovable => &[MigrateType::Reclaimable, MigrateType::Movable],
            MigrateType::Movable => &[MigrateType::Reclaimable, MigrateType::Unmovable],
            MigrateType::Reclaimable => &[MigrateType::Unmovable, MigrateType::Movable],
            MigrateType::HighAtomic => &[],
        }
    }

    /// The side array's code: 0 is Movable, so a zeroed blob has every pageblock Movable.
    fn code(self) -> u64 {
        match self {
            MigrateType::Movable => 0,
            MigrateType::Unmovable => 1,
            MigrateType::Reclaimable => 2,
            MigrateType::HighAtomic => 3,
        }
    }

    fn from_code(c: u64) -> MigrateType {
        match c {
            1 => MigrateType::Unmovable,
            2 => MigrateType::Reclaimable,
            3 => MigrateType::HighAtomic,
            _ => MigrateType::Movable,
        }
    }
}

/// Linux 7.x `should_try_claim_block`: a fallback of this order may take the whole pageblock.
fn should_try_claim(order: u32, mt: MigrateType) -> bool {
    order >= PAGEBLOCK_ORDER / 2 || mt != MigrateType::Movable
}

/// Pageblocks of the region [s, e) (pfn), counted from the one holding `s`.
pub(crate) fn pageblocks(s: u64, e: u64) -> u64 {
    crate::map::blocks(s, e, PAGEBLOCK_ORDER)
}

/// Side-array words of the region [s, e): the pageblock types, then (W35 MM-3) one compaction skip bit per
/// pageblock (`compact`).
pub(crate) fn side_words(s: u64, e: u64) -> usize {
    type_words(s, e) + pageblocks(s, e).div_ceil(64) as usize
}

/// Words of the pageblock types alone (the skip bits follow them).
pub(crate) fn type_words(s: u64, e: u64) -> usize {
    pageblocks(s, e).div_ceil(PER_WORD) as usize
}

/// How `rmqueue` found its block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Took {
    /// The asker's own type or a free pageblock: nothing mixed.
    Own,
    /// Another type's pageblock taken over whole (`fallback_whole_block`).
    Claimed,
    /// A block out of another type's pageblock, which stays that type (`fallback_steal`).
    Stolen,
}

impl Region {
    /// The free-block bitmap of `order` for type `mt` (orders from the pageblock order up have one, untyped).
    pub(super) fn map(&self, order: u32, mt: MigrateType) -> &crate::bitmap::HBitmap {
        let o = order as usize;
        if o < PB { &self.typed[o * MIGRATE_TYPES + mt as usize] } else { &self.maps[o - PB] }
    }

    fn pb_index(&self, pfn: u64) -> u64 {
        (pfn >> PAGEBLOCK_ORDER) - (self.start >> PAGEBLOCK_ORDER)
    }

    /// The pageblock holding `pfn` lies wholly inside the region (blocks never cross a region: an edge pageblock
    /// can hold no free pageblock-sized block, and is not claimed whole).
    pub(super) fn holds_pageblock(&self, pfn: u64) -> bool {
        let s = pfn & !(PAGEBLOCK_FRAMES - 1);
        s >= self.start && s + PAGEBLOCK_FRAMES <= self.end
    }
}

impl Buddy {
    /// The migrate type of the pageblock holding `pfn` (a relaxed load: changes happen under the zone lock).
    pub(super) fn pb_type(&self, r: &Region, pfn: u64) -> MigrateType {
        let i = r.pb_index(pfn);
        let w = self.blob()[r.pb_off + (i / PER_WORD) as usize].load(core::sync::atomic::Ordering::Relaxed);
        MigrateType::from_code((w >> ((i % PER_WORD) * 2)) & 3)
    }

    /// Sets the pageblock's type (zone lock held: the only writers); counts it.
    pub(super) fn set_pb_type(&self, r: &Region, zs: &mut ZoneState, pfn: u64, mt: MigrateType) {
        let old = self.pb_type(r, pfn);
        if old == mt {
            return;
        }
        let i = r.pb_index(pfn);
        let (w, sh) = (&self.blob()[r.pb_off + (i / PER_WORD) as usize], (i % PER_WORD) * 2);
        let v = w.load(core::sync::atomic::Ordering::Relaxed);
        w.store((v & !(3 << sh)) | mt.code() << sh, core::sync::atomic::Ordering::Relaxed);
        zs.pageblocks[old as usize] -= 1;
        zs.pageblocks[mt as usize] += 1;
        if self.global_reports {
            pageblock_counter(old).add(1u64.wrapping_neg());
            pageblock_counter(mt).inc();
        }
    }

    /// Counts a region's pageblocks as Movable (the zeroed side array) when the allocator is built.
    pub(super) fn count_pageblocks(&self) {
        for r in &self.regions {
            let n = pageblocks(r.start, r.end);
            self.zones[r.zone].lock().pageblocks[MigrateType::Movable as usize] += n;
            if self.global_reports {
                PAGEBLOCK_MOVABLE.add(n);
            }
        }
    }

    /// Linux 7.x `__rmqueue`: a free block of exactly `order` for type `mt` that ends at or below `limit_pfn` (zone
    /// lock held); its head reads `NONE`.
    /// `steal`: may take a block out of another type's pageblock (a per-CPU refill does that only for its first
    /// frame: a frame it holds already serves the asker without mixing).
    pub(super) fn rmqueue(&self, zone: usize, zs: &mut ZoneState, order: u32, mt: MigrateType, limit_pfn: u64, steal: bool) -> Option<(usize, u64, Took)> {
        if let Some((ri, pfn)) = self.smallest(zone, zs, order, mt, limit_pfn) {
            return Some((ri, pfn, Took::Own));
        }
        if order as usize >= PB {
            return None;
        }
        if let Some((ri, pfn)) = self.claim_fallback(zone, zs, order, mt, limit_pfn) {
            self.reports().whole_block();
            return Some((ri, pfn, Took::Claimed));
        }
        if !steal {
            return None;
        }
        let (ri, pfn) = self.steal(zone, zs, order, mt, limit_pfn)?;
        self.reports().steal();
        Some((ri, pfn, Took::Stolen))
    }

    /// The lowest free block of `order` in map (order, mt) of the zone whose first 2^`want` frames end at or below
    /// `limit_pfn`. Each region's bitmap gives its lowest block, so a region whose lowest one ends above the limit
    /// has none below it.
    fn first_free(&self, zone: usize, order: u32, mt: MigrateType, want: u32, limit_pfn: u64) -> Option<(usize, u64)> {
        for (ri, r) in self.regions.iter().enumerate().filter(|(_, r)| r.zone == zone && r.start < limit_pfn) {
            let Some(b) = r.map(order, mt).first(self.blob()) else { continue };
            let pfn = ((r.start >> order) + b) << order;
            if pfn + (1 << want) <= limit_pfn {
                return Some((ri, pfn));
            }
        }
        None
    }

    /// Linux `expand`: the block (pfn, from) taken, its upper halves down to `order` go back (to the type of their
    /// pageblock, or untyped from the pageblock order up).
    pub(super) fn expand(&self, r: &Region, zs: &mut ZoneState, pfn: u64, order: u32, mut from: u32) {
        while from > order {
            from -= 1;
            self.insert_free(r, zs, pfn + (1 << from), from);
        }
    }

    /// Linux `__rmqueue_smallest` for `mt` below the pageblock order: the smallest block of `mt`'s own pageblocks.
    pub(super) fn smallest_typed(&self, zone: usize, zs: &mut ZoneState, order: u32, mt: MigrateType, limit_pfn: u64) -> Option<(usize, u64)> {
        for o in order..PAGEBLOCK_ORDER {
            if zs.by_type[mt as usize][o as usize] == 0 {
                continue;
            }
            if let Some((ri, pfn)) = self.first_free(zone, o, mt, order, limit_pfn) {
                let r = &self.regions[ri];
                self.remove_free(r, zs, pfn, o);
                self.expand(r, zs, pfn, order, o);
                return Some((ri, pfn));
            }
        }
        None
    }

    /// `smallest_typed`, then a free pageblock or larger block, which becomes `mt` where the allocation lands.
    fn smallest(&self, zone: usize, zs: &mut ZoneState, order: u32, mt: MigrateType, limit_pfn: u64) -> Option<(usize, u64)> {
        if let Some(got) = self.smallest_typed(zone, zs, order, mt, limit_pfn) {
            return Some(got);
        }
        for o in order.max(PAGEBLOCK_ORDER)..=MAX_ORDER {
            if zs.free_blocks[o as usize] == 0 {
                continue;
            }
            if let Some((ri, pfn)) = self.first_free(zone, o, mt, order, limit_pfn) {
                let r = &self.regions[ri];
                self.remove_free(r, zs, pfn, o);
                // Only the pageblocks the allocation covers change type; the halves split off stay free.
                let mut at = pfn;
                while at < pfn + (1u64 << order).max(PAGEBLOCK_FRAMES) {
                    self.set_pb_type(r, zs, at, mt);
                    at += PAGEBLOCK_FRAMES;
                }
                self.expand(r, zs, pfn, order, o);
                return Some((ri, pfn));
            }
        }
        None
    }

    /// Linux 7.x `__rmqueue_claim`: the largest free block of a fallback type whose pageblock can be taken over
    /// (`try_to_claim_block`), then the block from the asker's own lists.
    fn claim_fallback(&self, zone: usize, zs: &mut ZoneState, order: u32, mt: MigrateType, limit_pfn: u64) -> Option<(usize, u64)> {
        for co in (order..PAGEBLOCK_ORDER).rev() {
            if !should_try_claim(co, mt) {
                break;
            }
            let Some(f) = mt.fallbacks().iter().copied().find(|f| zs.by_type[*f as usize][co as usize] > 0) else { continue };
            let Some((ri, pfn)) = self.first_free(zone, co, f, order, limit_pfn) else { continue };
            if self.try_claim(&self.regions[ri], zs, pfn, mt, f) {
                return self.smallest(zone, zs, order, mt, limit_pfn);
            }
        }
        None
    }

    /// Linux 7.x `try_to_claim_block` below the pageblock order: the pageblock of `pfn` (type `from`) becomes `mt`
    /// when at least half of it is free or alike (Movable: movable frames; a non-movable type out of a Movable
    /// pageblock: the frames that are neither free nor movable), its free blocks moved along.
    fn try_claim(&self, r: &Region, zs: &mut ZoneState, pfn: u64, mt: MigrateType, from: MigrateType) -> bool {
        if !r.holds_pageblock(pfn) {
            return false;
        }
        let start = pfn & !(PAGEBLOCK_FRAMES - 1);
        let (mut free, mut movable, mut at) = (0u64, 0u64, start);
        while at < start + PAGEBLOCK_FRAMES {
            let m = self.get_meta(r, at);
            let n = if m.state == State::None { 1 } else { 1u64 << m.order };
            match m.state {
                State::Free => free += n,
                State::Alloc if MigrateType::of_index(m.kind) == MigrateType::Movable => movable += n,
                _ => {}
            }
            at += n;
        }
        let alike = match (mt, from) {
            (MigrateType::Movable, _) => movable,
            (_, MigrateType::Movable) => PAGEBLOCK_FRAMES.saturating_sub(free + movable),
            _ => 0,
        };
        if free + alike < PAGEBLOCK_FRAMES / 2 {
            return false;
        }
        self.move_free(r, zs, start, from, mt);
        self.set_pb_type(r, zs, start, mt);
        true
    }

    /// Linux `__move_freepages_block`: every free block of the pageblock at `start` moves from `from`'s bitmaps to
    /// `to`'s (they are all below the pageblock order: a free pageblock would be one untyped block).
    fn move_free(&self, r: &Region, zs: &mut ZoneState, start: u64, from: MigrateType, to: MigrateType) {
        let mut at = start;
        while at < start + PAGEBLOCK_FRAMES {
            let m = self.get_meta(r, at);
            if m.state != State::Free || m.order >= PAGEBLOCK_ORDER {
                at += 1;
                continue;
            }
            let bit = r.bit(at, m.order);
            r.map(m.order, from).clear(self.blob(), bit);
            r.map(m.order, to).set(self.blob(), bit);
            zs.by_type[from as usize][m.order as usize] -= 1;
            zs.by_type[to as usize][m.order as usize] += 1;
            at += 1 << m.order;
        }
    }

    /// Linux 7.x `__rmqueue_steal`: the smallest block of a fallback type; its pageblock keeps its type.
    fn steal(&self, zone: usize, zs: &mut ZoneState, order: u32, mt: MigrateType, limit_pfn: u64) -> Option<(usize, u64)> {
        for co in order..PAGEBLOCK_ORDER {
            for &f in mt.fallbacks() {
                if zs.by_type[f as usize][co as usize] == 0 {
                    continue;
                }
                if let Some((ri, pfn)) = self.first_free(zone, co, f, order, limit_pfn) {
                    let r = &self.regions[ri];
                    self.remove_free(r, zs, pfn, co);
                    self.expand(r, zs, pfn, order, co);
                    return Some((ri, pfn));
                }
            }
        }
        None
    }
}

impl MigrateType {
    /// The type of a recorded kind number (frame metadata).
    pub(super) fn of_index(kind: usize) -> MigrateType {
        match kind {
            k if k == Kind::User as usize || k == Kind::PageCache as usize => MigrateType::Movable,
            _ => MigrateType::Unmovable,
        }
    }
}

fn pageblock_counter(mt: MigrateType) -> &'static klog::metrics::Counter {
    match mt {
        MigrateType::Unmovable => &PAGEBLOCK_UNMOVABLE,
        MigrateType::Movable => &PAGEBLOCK_MOVABLE,
        MigrateType::Reclaimable => &PAGEBLOCK_RECLAIMABLE,
        MigrateType::HighAtomic => &super::highatomic::PAGEBLOCK_HIGHATOMIC,
    }
}

/// Global counters of the fallbacks (`Reports::steal`, `Reports::whole_block`).
pub(crate) fn count_steal() {
    FALLBACK_STEAL.inc();
}
pub(crate) fn count_whole_block() {
    FALLBACK_WHOLE_BLOCK.inc();
}
