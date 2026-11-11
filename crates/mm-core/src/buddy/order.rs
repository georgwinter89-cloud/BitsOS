//! kswapd's high-order balance (Linux 7.x mm/vmscan.c: `pgdat_balanced` asks `zone_watermark_ok_safe(zone, order,
//! ...)`, which for order > 0 wants a free block of at least that order; `balance_pgdat` reclaims for it, bounded,
//! and falls back to order 0). W35 MM-3: an order still short once the count is balanced goes to kcompactd
//! (Linux `kswapd_try_to_sleep` -> `wakeup_kcompactd`), which makes the block by compaction (`compact`).

use super::{zone_index, Buddy, ZONES};
use crate::{reclaim, Zone, MAX_ORDER};
use core::sync::atomic::Ordering::Relaxed;

impl Buddy {
    /// W35 MM-1: zone `zone`'s share of the machine's watermarks (Linux `__setup_per_zone_wmarks` gives each zone its
    /// managed share of `min_free_kbytes`; the gaps scale the same way).
    pub fn zone_watermarks(&self, zone: usize) -> reclaim::Watermarks {
        let managed: [u64; ZONES] = core::array::from_fn(|z| self.managed[z].load(Relaxed));
        let total: u64 = managed.iter().sum();
        let w = reclaim::watermarks(total);
        let share = |v: u64| if total == 0 { 0 } else { (v as u128 * managed[zone] as u128 / total as u128) as u64 };
        reclaim::Watermarks { min: share(w.min), low: share(w.low), high: share(w.high) }
    }

    /// W35 MM-1: Linux 7.x `__zone_watermark_ok`: zone `zone` has more than `mark` free frames once a block of
    /// `order` is taken, and (order > 0) a free block of `order` or more of any migrate type. Per-CPU lists do not
    /// count (Linux's NR_FREE_PAGES neither), nor does the HighAtomic reserve (`__zone_watermark_unusable_free`).
    pub fn zone_watermark_ok(&self, zone: usize, order: u32, mark: u64) -> bool {
        let order = order.min(MAX_ORDER);
        let zs = self.zones[zone].lock();
        if zs.usable_free().saturating_sub((1 << order) - 1) <= mark {
            return false;
        }
        order == 0 || zs.has_usable_block(order)
    }

    /// W35 MM-1: Linux `pgdat_balanced`: some zone with memory meets its high watermark for `order`.
    pub fn order_balanced(&self, order: u32) -> bool {
        (0..ZONES).any(|z| self.managed[z].load(Relaxed) > 0 && self.zone_watermark_ok(z, order, self.zone_watermarks(z).high))
    }

    /// W35 MM-1: the slow path of the boot allocator (Linux `wake_all_kswapds` at the start of
    /// `__alloc_pages_slowpath`, `wakeup_kswapd` returning early when `pgdat_balanced`): an allocation that found
    /// nothing notes its order and wakes kswapd. Takes the zone locks: never with one held. While a wake is pending
    /// that kswapd has not taken (`kswapd_kicked`), the order is noted without the zone-lock check: a burst of
    /// failures costs atomics, not two zone locks each (kswapd drops an order that is balanced already).
    pub(super) fn want_kswapd(&self, order: u32) {
        if !self.global_reports || (!reclaim::kswapd_kicked() && self.order_balanced(order)) {
            return;
        }
        if order > 0 {
            reclaim::FRAME_WANT.note(order);
        } else {
            reclaim::wake_kswapd();
        }
    }

    /// Whether a free block of `order` or more lies in a buddy list of Normal or Dma32 (the zones a heap run is
    /// taken from). Per-CPU lists hold single frames only and are not counted; the HighAtomic reserve is not either.
    pub fn has_free_block(&self, order: u32) -> bool {
        let from = order.min(MAX_ORDER);
        [Zone::Normal, Zone::Dma32].iter().any(|&z| self.zones[zone_index(z)].lock().has_usable_block(from))
    }
}
