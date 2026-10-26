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
                            let bit = r.maps[m.order as usize].test(self.blob(), r.bit(pfn, m.order));
                            if bit != (m.state == State::Free) {
                                return Err("bitmap and frame state disagree");
                            }
                            if m.state == State::Free {
                                frames += 1 << m.order;
                                blocks[m.order as usize] += 1;
                                let buddy = pfn ^ (1 << m.order);
                                if m.order < MAX_ORDER && r.holds(buddy, m.order) && self.get_meta(r, buddy) == m {
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
