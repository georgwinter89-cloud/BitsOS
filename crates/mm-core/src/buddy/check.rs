//! The consistency walk over every region (tests, and later the MCP check).

use super::{Buddy, ORDERS, ZONES};
use crate::meta::State;
use crate::{FrameAlloc, MAX_ORDER};
use core::sync::atomic::Ordering::Relaxed;

impl Buddy {
    /// Walks every region and checks the lists, the frame states and the counters against each other.
    /// Expensive (reads every frame's state); for tests and the MCP consistency check.
    pub fn check(&self) -> Result<(), &'static str> {
        let _ = self.drain_all();
        let mut alloc_frames = 0u64;
        for zone in 0..ZONES {
            let zs = self.zones[zone].lock();
            let (mut frames, mut blocks) = (0u64, [0u64; ORDERS]);
            let mut by_type = [[0u64; ORDERS]; super::MIGRATE_TYPES];
            for r in self.zone_regions(zone) {
                let mut pfn = r.start;
                while pfn < r.end {
                    let m = self.get_meta(r, pfn);
                    match m.state {
                        State::None => pfn += 1,
                        State::Cached => return Err("cached frame after a full drain"),
                        State::Free | State::Alloc => {
                            if pfn % (1 << m.order) != 0 || !r.holds(pfn, m.order) {
                                return Err("block misaligned or outside its region");
                            }
                            for t in pfn + 1..pfn + (1 << m.order) {
                                if self.get_meta(r, t).state != State::None {
                                    return Err("state inside a block");
                                }
                            }
                            let mt = self.pb_type(r, pfn);
                            let bit = r.map(m.order, mt).test(self.blob(), r.bit(pfn, m.order));
                            if bit != (m.state == State::Free) {
                                return Err("bitmap and frame state disagree");
                            }
                            // W35 MM-1: a free block lies in its pageblock type's bitmap only.
                            if (m.order as usize) < super::PB
                                && super::MigrateType::ALL.iter().any(|&o| o != mt && r.map(m.order, o).test(self.blob(), r.bit(pfn, m.order)))
                            {
                                return Err("a block in the bitmap of another migrate type");
                            }
                            if m.state == State::Free {
                                by_type[mt as usize][m.order as usize] += 1;
                            }
                            if m.state == State::Free {
                                frames += 1 << m.order;
                                blocks[m.order as usize] += 1;
                                let buddy = pfn ^ (1 << m.order);
                                // The reserve's halves stay apart (`free_block`).
                                let top = if mt == super::MigrateType::HighAtomic { super::PAGEBLOCK_ORDER - 1 } else { MAX_ORDER };
                                if m.order > top {
                                    return Err("a free pageblock of the HighAtomic reserve");
                                }
                                if m.order < top && r.holds(buddy, m.order) && self.get_meta(r, buddy) == m {
                                    return Err("two free buddies not merged");
                                }
                            } else {
                                alloc_frames += 1 << m.order;
                            }
                            pfn += 1 << m.order;
                        }
                    }
                }
            }
            if frames != zs.free_frames || blocks != zs.free_blocks {
                return Err("zone counters disagree with the lists");
            }
            if by_type != zs.by_type {
                return Err("per-type counters disagree with the lists");
            }
            let reserved: u64 = by_type[super::MigrateType::HighAtomic as usize].iter().enumerate().map(|(o, n)| n << o).sum();
            if reserved != zs.free_highatomic {
                return Err("the HighAtomic reserve's free count disagrees with the lists");
            }
            let pbs: u64 = self.zone_regions(zone).map(|r| super::pageblocks(r.start, r.end)).sum();
            if zs.pageblocks.iter().sum::<u64>() != pbs {
                return Err("pageblock counts disagree with the regions");
            }
        }
        let used: u64 = self.used.iter().map(|u| u.load(Relaxed)).sum();
        // The carved metadata is counted as used (Other) but is not a block in any list.
        if alloc_frames + self.meta_frames != used {
            return Err("allocated blocks disagree with used_by_kind");
        }
        let s = self.stats();
        if s.free_bytes + s.used_by_kind.iter().sum::<u64>() + s.leaked_bytes != s.total_bytes {
            return Err("free + used + leaked != total");
        }
        Ok(())
    }
}
