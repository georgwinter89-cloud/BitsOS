//! What holds the aligned blocks of one order (the answer to "why is no 2 MiB block free?"): every block counted as
//! free, cache-only (what compaction's drop-if-clean can clear, W35 MM-3) or pinned, with the kinds of frames that
//! pin them. Linux answers the same with /proc/pagetypeinfo (blocks per migrate type) and page_owner; until MM-2 moves
//! pages, a block pinned by one heap or anonymous frame stays pinned.
//!
//! A look without the zone locks (frame states are atomics): a hint for a failure message, not a contract.

use super::{zone_index, Buddy, Region};
use crate::meta::{State, TAG_SLAB};
use crate::{Kind, Zone, MAX_ORDER, PAGE};
use core::fmt;

/// A pinned block whose pinning frames are at most this many (a few scattered frames keep the whole block).
pub const FEW: u64 = 8;

/// What holds a frame that keeps its block from being freed or cleared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum BlockHolder {
    /// A slab of the kernel heap.
    HeapSlab = 0,
    /// Any other heap frame: a large run or one of a scattered block's single frames.
    Heap = 1,
    PageTable = 2,
    User = 3,
    Dma = 4,
    Stack = 5,
    Driver = 6,
    Other = 7,
    /// Page cache compaction cannot drop: no owner noted (tmpfs) or a block of several frames.
    CacheKept = 8,
    /// Inside a region but never handed out (boot modules, the allocator's own tables).
    Reserved = 9,
}

pub const HOLDERS: usize = 10;
const HOLDER_NAMES: [&str; HOLDERS] = ["heap-slab", "heap", "table", "user", "dma", "stack", "driver", "other", "cache-kept", "reserved"];

impl BlockHolder {
    fn of_kind(kind: usize, tag: u8) -> BlockHolder {
        match kind {
            k if k == Kind::Heap as usize && tag == TAG_SLAB => BlockHolder::HeapSlab,
            k if k == Kind::Heap as usize => BlockHolder::Heap,
            k if k == Kind::PageTable as usize => BlockHolder::PageTable,
            k if k == Kind::User as usize => BlockHolder::User,
            k if k == Kind::Dma as usize => BlockHolder::Dma,
            k if k == Kind::Stack as usize => BlockHolder::Stack,
            k if k == Kind::Driver as usize => BlockHolder::Driver,
            k if k == Kind::PageCache as usize => BlockHolder::CacheKept,
            _ => BlockHolder::Other,
        }
    }
}

/// One frame's part in its block.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Part {
    Free,
    /// Free, parked in a per-CPU list (merges after a drain).
    Pcp,
    /// A clean page-cache frame with an owner: clearable.
    Cache,
    Pinned(BlockHolder),
}

/// The census of one order (Normal and Dma32, the zones a heap run or a huge page comes from).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BlockCensus {
    pub order: u32,
    /// Free frames, buddy and per-CPU lists.
    pub free_frames: u64,
    /// The largest order with a free block in the buddy lists (None: not one free frame there).
    pub largest_free: Option<u32>,
    /// Aligned blocks of `order` looked at.
    pub blocks: u64,
    /// Every frame free in the buddy lists.
    pub free: u64,
    /// Every frame free, some in a per-CPU list.
    pub free_pcp: u64,
    /// Free frames and clearable cache only (at least one cache frame).
    pub cache_only: u64,
    /// At least one frame of a `BlockHolder`.
    pub pinned: u64,
    /// Of `pinned`: at most `FEW` pinning frames.
    pub pinned_few: u64,
    /// Pinned blocks holding a frame of each holder (a block may count under several).
    pub holders: [u64; HOLDERS],
    /// Pinned blocks whose pinning frames are all of one holder.
    pub sole: [u64; HOLDERS],
}

impl fmt::Display for BlockCensus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kib = (PAGE << self.order) >> 10;
        write!(f, "free {} pages, largest free order ", self.free_frames)?;
        match self.largest_free {
            Some(o) => write!(f, "{o}")?,
            None => write!(f, "none")?,
        }
        write!(f, "; {} KiB blocks {}: free {} (+{} after a pcp drain), cache-only {}, pinned {} (by <= {FEW} frames: {})",
            kib, self.blocks, self.free, self.free_pcp, self.cache_only, self.pinned, self.pinned_few)?;
        let mut first = true;
        for (i, n) in self.holders.iter().enumerate().filter(|(_, n)| **n > 0) {
            write!(f, "{}{}={n}", if first { "; holding " } else { " " }, HOLDER_NAMES[i])?;
            if self.sole[i] > 0 {
                write!(f, "(only {})", self.sole[i])?;
            }
            first = false;
        }
        Ok(())
    }
}

/// One block's frames as they are added up.
#[derive(Default)]
struct Acc {
    pcp: u64,
    cache: u64,
    pinning: u64,
    present: u16,
}

impl Acc {
    fn add(&mut self, p: Part) {
        match p {
            Part::Free => {}
            Part::Pcp => self.pcp += 1,
            Part::Cache => self.cache += 1,
            Part::Pinned(h) => {
                self.pinning += 1;
                self.present |= 1 << h as usize;
            }
        }
    }

    fn close(&mut self, c: &mut BlockCensus) {
        c.blocks += 1;
        if self.pinning > 0 {
            c.pinned += 1;
            c.pinned_few += u64::from(self.pinning <= FEW);
            for h in 0..HOLDERS {
                if self.present & 1 << h != 0 {
                    c.holders[h] += 1;
                    c.sole[h] += u64::from(self.present == 1 << h);
                }
            }
        } else if self.cache > 0 {
            c.cache_only += 1;
        } else if self.pcp > 0 {
            c.free_pcp += 1;
        } else {
            c.free += 1;
        }
        *self = Acc::default();
    }
}

impl Buddy {
    /// The census of the aligned blocks of `order` (clamped to `MAX_ORDER`) in Normal and Dma32.
    pub fn census(&self, order: u32) -> BlockCensus {
        let order = order.min(MAX_ORDER);
        let mut c = BlockCensus { order, ..Default::default() };
        for z in [Zone::Normal, Zone::Dma32] {
            let i = self.zone_info(zone_index(z));
            c.free_frames += i.free_frames + i.cached_frames;
            // Not the HighAtomic reserve's blocks: the allocation this explains could not take them.
            let reserved = &i.free_by_type[super::MigrateType::HighAtomic as usize];
            if let Some(o) = (0..i.free_blocks.len()).rev().find(|&o| i.free_blocks[o] > reserved[o]) {
                c.largest_free = c.largest_free.max(Some(o as u32));
            }
        }
        for r in self.zone_regions(zone_index(Zone::Normal)).chain(self.zone_regions(zone_index(Zone::Dma32))) {
            self.census_region(r, order, &mut c);
        }
        c
    }

    /// Walks every frame of `r` once: a block head covers its frames (a free block of a higher order covers several
    /// blocks of `order`), a frame no head covers was never handed out.
    fn census_region(&self, r: &Region, order: u32, c: &mut BlockCensus) {
        let size = 1u64 << order;
        let first = r.start.next_multiple_of(size);
        let n = r.end.saturating_sub(first) / size;
        if n == 0 {
            return;
        }
        let end = first + n * size;
        let (mut cover_end, mut cover) = (0u64, Part::Pinned(BlockHolder::Reserved));
        let mut acc = Acc::default();
        for pfn in r.start..end {
            let m = self.get_meta(r, pfn);
            let part = match m.state {
                State::Free => {
                    (cover_end, cover) = (pfn + (1 << m.order), Part::Free);
                    Part::Free
                }
                State::Cached => Part::Pcp,
                State::Alloc => {
                    let clearable = m.kind == Kind::PageCache as usize && m.order == 0 && self.owner(pfn * PAGE) != 0;
                    let p = if clearable { Part::Cache } else { Part::Pinned(BlockHolder::of_kind(m.kind, m.tag)) };
                    (cover_end, cover) = (pfn + (1 << m.order), p);
                    p
                }
                State::None if pfn < cover_end => cover,
                State::None => Part::Pinned(BlockHolder::Reserved),
            };
            if pfn < first {
                continue;
            }
            acc.add(part);
            if (pfn + 1).is_multiple_of(size) {
                acc.close(c);
            }
        }
    }
}
