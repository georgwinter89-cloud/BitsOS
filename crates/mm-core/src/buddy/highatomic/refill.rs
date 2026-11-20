//! Making the reserve whole again (Linux has `reserve_highatomic_pageblock` grow it after every successful
//! high-order atomic allocation and `unreserve_highatomic_pageblock` shrink it under pressure, at most 1 % of a
//! zone; here it has a fixed size, made at boot, and this keeps it at that size and free).
//!
//! What an atomic caller takes may stay for good: a slab page, a driver's buffer. Until 12.11.2026 nothing gave
//! the reserve such frames back: whatever settled in it shrank it for the rest of the boot. When a zone's reserve
//! has less free than `LOW` of its size (or misses a pageblock), a pass brings in wholly free pageblocks, one for
//! each pageblock it hands over: the reserve pageblock with the least free goes to the type living in it
//! (Unmovable: only such kinds take), its free blocks with it, so the number of reserve pageblocks never passes the
//! zone's target. A pass stops at `HIGH`.
//!
//! No allocation waits for this: a take below the low mark only rings kcompactd (`ask`), which runs the pass
//! (`Buddy::refill_reserve`, crate mm's `highatomic`); where no pageblock is free it makes one by compaction, as
//! for a huge-page fault.

use super::super::compact::{Control, How, Status, SLICE};
use super::super::{Buddy, MigrateType, Region, ZoneState, PAGEBLOCK_FRAMES, PAGEBLOCK_ORDER, ZONES};
use super::stats::{HANDED_OVER, REFILLS, REFILL_SHORT};
use crate::meta::State;
use crate::{reclaim, MAX_ORDER};
use core::sync::atomic::Ordering::Relaxed;

/// A pass starts when less than 3/4 of a zone's reserve is free.
const LOW: (u64, u64) = (3, 4);
/// A pass stops when all but 1/16 is free: frames that come back by themselves need no new pageblock.
const HIGH: (u64, u64) = (15, 16);

/// Asks the background for a pass: kcompactd is rung, with no order to make a block for (a few atomics; the
/// wake itself is deferred where the scheduler may not be called).
pub(super) fn ask() {
    crate::compact::ring_for_flush();
}

/// What one pass did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Refill {
    /// Pageblocks made part of the reserve.
    pub reserved: u64,
    /// Reserve pageblocks handed to the type living in them.
    pub handed: u64,
    /// Zones left short: no pageblock was free and none could be made.
    pub short: u64,
}

/// What one try to bring a pageblock in did.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Step {
    Replaced { handed: bool },
    /// No pageblock of the zone is wholly free.
    NoneFree,
    /// The reserve has its pageblocks and none of them holds a frame in use (any more).
    Whole,
}

impl Buddy {
    /// Frames of zone `zone`'s reserve at its full size.
    fn reserve_size(&self, zone: usize) -> u64 {
        self.ha.target[zone].load(Relaxed) * PAGEBLOCK_FRAMES
    }

    /// Below this many free frames zone `zone`'s reserve is refilled.
    pub(super) fn reserve_low_mark(&self, zone: usize) -> u64 {
        self.reserve_size(zone) * LOW.0 / LOW.1
    }

    /// (pageblocks, free frames) of zone `zone`'s reserve now.
    fn reserve_now(&self, zone: usize) -> (u64, u64) {
        let zs = self.zones[zone].lock();
        (zs.pageblocks[MigrateType::HighAtomic as usize], zs.free_highatomic)
    }

    /// Whether some zone's reserve misses a pageblock or is below its low mark (two zone locks: the background's
    /// question, never an allocation's).
    pub fn reserve_low(&self) -> bool {
        (0..ZONES).any(|z| {
            let (pbs, free) = self.reserve_now(z);
            pbs < self.ha.target[z].load(Relaxed) || free < self.reserve_low_mark(z)
        })
    }

    /// kswapd's look: a low reserve rings kcompactd for a pass (a take rang already, unless its wake was lost or
    /// the pass before found nothing to make a pageblock of). True: it is low.
    pub fn reserve_check(&self) -> bool {
        let low = self.reserve_low();
        if low {
            ask();
        }
        low
    }

    /// One pass over every zone whose reserve is low: up to a quarter of the reserve (at least one pageblock) is
    /// replaced, so a pass is bounded and the next wake does the rest. `between` is called between the slices of
    /// a compaction run (kcompactd yields there). For a background thread: it may reclaim and compact.
    pub fn refill_reserve(&self, between: &mut dyn FnMut()) -> Refill {
        let mut out = Refill::default();
        for zone in 0..ZONES {
            let want = self.ha.target[zone].load(Relaxed);
            let (pbs, free) = self.reserve_now(zone);
            if want == 0 || (pbs >= want && free >= self.reserve_low_mark(zone)) {
                continue;
            }
            for _ in 0..want / 4 + 1 {
                let (pbs, free) = self.reserve_now(zone);
                if pbs >= want && free >= self.reserve_size(zone) * HIGH.0 / HIGH.1 {
                    break;
                }
                let mut step = self.replace_pageblock(zone, want);
                if step == Step::NoneFree && self.make_pageblock(zone, between) {
                    step = self.replace_pageblock(zone, want);
                }
                match step {
                    Step::Replaced { handed } => {
                        out.reserved += 1;
                        out.handed += u64::from(handed);
                    }
                    Step::Whole => break,
                    Step::NoneFree => {
                        out.short += 1;
                        if self.global_reports {
                            REFILL_SHORT.inc();
                        }
                        break;
                    }
                }
            }
        }
        self.ha.refilled.fetch_add(out.reserved, Relaxed);
        self.ha.handed.fetch_add(out.handed, Relaxed);
        if self.global_reports {
            REFILLS.add(out.reserved);
            HANDED_OVER.add(out.handed);
        }
        out
    }

    /// A wholly free pageblock of `zone` joins the reserve. A reserve that has its `want` pageblocks takes one
    /// only for one that leaves in the same hold of the zone lock: the reserve pageblock with the least free.
    fn replace_pageblock(&self, zone: usize, want: u64) -> Step {
        // Looked for without the lock (a walk over every pageblock's type): a hint, read again under it.
        let hint = self.most_used(zone);
        let mut zs = self.zones[zone].lock();
        let leaving = if zs.pageblocks[MigrateType::HighAtomic as usize] >= want {
            let still = |&(r, pb): &(&Region, u64)| self.pb_type(r, pb) == MigrateType::HighAtomic && self.pageblock_use(r, pb).0 < PAGEBLOCK_FRAMES;
            match hint.filter(still) {
                Some(l) => Some(l),
                None => return Step::Whole,
            }
        } else {
            None
        };
        let Some((r, pb)) = self.free_pageblock(zone, &zs) else { return Step::NoneFree };
        if !self.reserve_pageblock(r, &mut zs, pb) {
            return Step::NoneFree;
        }
        if let Some((r, pb)) = leaving {
            self.hand_over(r, &mut zs, pb);
        }
        Step::Replaced { handed: leaving.is_some() }
    }

    /// The topmost pageblock of a free block of the pageblock order or more, highest region first (the reserve
    /// sits high: address limits are low).
    fn free_pageblock(&self, zone: usize, zs: &ZoneState) -> Option<(&Region, u64)> {
        for r in self.regions.iter().rev().filter(|r| r.zone == zone) {
            for o in PAGEBLOCK_ORDER..=MAX_ORDER {
                if zs.free_blocks[o as usize] == 0 {
                    continue;
                }
                if let Some(b) = r.map(o, MigrateType::Movable).first(self.blob()) {
                    let head = ((r.start >> o) + b) << o;
                    return Some((r, head + (1 << o) - PAGEBLOCK_FRAMES));
                }
            }
        }
        None
    }

    /// (free frames, an unmovable frame is allocated) of the pageblock at `pb`. Exact under the zone lock.
    fn pageblock_use(&self, r: &Region, pb: u64) -> (u64, bool) {
        let (mut free, mut unmovable, mut at) = (0, false, pb);
        while at < pb + PAGEBLOCK_FRAMES {
            let m = self.get_meta(r, at);
            let n = if m.state == State::None { 1 } else { 1u64 << m.order };
            match m.state {
                State::Free => free += n,
                State::Alloc => unmovable |= MigrateType::of_index(m.kind) == MigrateType::Unmovable,
                _ => {}
            }
            at += n;
        }
        (free.min(PAGEBLOCK_FRAMES), unmovable)
    }

    /// The reserve pageblock of `zone` with the least free frames, if one has frames in use.
    fn most_used(&self, zone: usize) -> Option<(&Region, u64)> {
        let mut best: Option<(&Region, u64, u64)> = None;
        for r in self.regions.iter().filter(|r| r.zone == zone) {
            let mut pb = r.start.next_multiple_of(PAGEBLOCK_FRAMES);
            while pb + PAGEBLOCK_FRAMES <= r.end {
                if self.pb_type(r, pb) == MigrateType::HighAtomic {
                    let free = self.pageblock_use(r, pb).0;
                    if free < PAGEBLOCK_FRAMES && best.is_none_or(|b| free < b.2) {
                        best = Some((r, pb, free));
                    }
                }
                pb += PAGEBLOCK_FRAMES;
            }
        }
        best.map(|(r, pb, _)| (r, pb))
    }

    /// The reserve pageblock at `pb` becomes a pageblock of the type living in it, its free blocks with it (Linux
    /// `unreserve_highatomic_pageblock`: `move_freepages_block` to the asker's type; there is no asker here).
    /// Nothing happens unless it still is a reserve pageblock with frames in use (zone lock held).
    fn hand_over(&self, r: &Region, zs: &mut ZoneState, pb: u64) {
        if self.pb_type(r, pb) != MigrateType::HighAtomic {
            return;
        }
        let (free, unmovable) = self.pageblock_use(r, pb);
        if free == PAGEBLOCK_FRAMES {
            return;
        }
        let to = if unmovable { MigrateType::Unmovable } else { MigrateType::Movable };
        self.move_free(r, zs, pb, MigrateType::HighAtomic, to);
        self.reserve_moved(zs, free, false);
        self.set_pb_type(r, zs, pb, to);
    }

    /// A free pageblock in `zone` by compaction (one run, in slices); true: one exists now. Only where the
    /// caller may reclaim.
    fn make_pageblock(&self, zone: usize, between: &mut dyn FnMut()) -> bool {
        let has = |b: &Buddy| b.zones[zone].lock().has_usable_block(PAGEBLOCK_ORDER);
        if !reclaim::context_allows() {
            return has(self);
        }
        let Some(_inside) = reclaim::enter() else { return has(self) };
        let mut c = Control::new(Some(PAGEBLOCK_ORDER), How::Daemon, zone);
        let st = loop {
            let st = self.compact_zone(&mut c, SLICE);
            if st != Status::Continue {
                break st;
            }
            between();
        };
        self.count_run(&c, None);
        if st != Status::Success {
            // Frames stranded on per-CPU lists may complete the block (as `kcompactd_work` does).
            self.drain_all();
        }
        has(self)
    }
}
