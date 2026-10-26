//! Blocks below an address limit (DMA masks narrower than the zones, e.g. a 36-bit or 30-bit device).
//! Linux has only ZONE_DMA/DMA32 and retries; the buddy bitmaps here give each region's lowest free block per
//! order directly, so a limit costs one comparison per region and order.

use super::{Buddy, DMA32_END_PFN, ZONES};
use crate::meta::{Meta, State};
use crate::{FrameAlloc, Kind, Zone, MAX_ORDER, PAGE};
use core::sync::atomic::Ordering::Relaxed;

impl Buddy {
    /// One past the highest frame any region holds (bytes).
    pub fn top(&self) -> u64 {
        self.regions.iter().map(|r| r.end).max().unwrap_or(0) * PAGE
    }

    /// 2^order frames (aligned to their size) that end at or below `limit` (bytes, exclusive), charged to
    /// `kind`. NORMAL first when the limit reaches above 4 GiB (DMA32 stays for the devices that need it),
    /// then DMA32. Without a real limit it is the ordinary `alloc`.
    pub fn alloc_below(&self, order: u32, limit: u64, kind: Kind) -> Option<u64> {
        if limit >= self.top() {
            return self.alloc(order, Zone::Normal, kind);
        }
        let limit_pfn = limit / PAGE;
        let tries: &[usize] = if limit_pfn > DMA32_END_PFN { &[1, 0] } else { &[0] };
        if order > MAX_ORDER || limit_pfn == 0 {
            self.reports().alloc_fail(order.min(MAX_ORDER), tries[0]);
            return None;
        }
        debug_assert!(tries.iter().all(|&z| z < ZONES));
        for attempt in 0..2 {
            for &z in tries {
                let mut zs = self.zones[z].lock();
                if let Some((ri, pfn)) = self.take_block_below(z, &mut zs, order, limit_pfn) {
                    self.set_meta(&self.regions[ri], pfn, Meta::new(State::Alloc, order, kind as usize));
                    drop(zs);
                    self.used[kind as usize].fetch_add(1 << order, Relaxed);
                    return Some(pfn * PAGE);
                }
            }
            // Frames parked in the per-CPU lists may be the ones below the limit.
            if attempt == 0 {
                if self.drain_all() == 0 {
                    break;
                }
                self.reports().drain();
            }
        }
        self.reports().alloc_fail(order, tries[0]);
        None
    }
}
