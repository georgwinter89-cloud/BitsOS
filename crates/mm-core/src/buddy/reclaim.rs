//! MM-9: direct reclaim in the allocator's slow path (`__alloc_pages_direct_reclaim`,
//! linux-6.6/mm/page_alloc.c:3639) and the reserve that keeps it from starving the kernel.
//!
//! The reserve: once a reclaimer is installed, an allocation of program or cache memory
//! (`reclaim::direct_allowed`) does not take the last frames of a zone (its share of the min watermark) when it
//! refills a per-CPU list or takes a block; it fails over into `alloc_reclaiming` instead, which reclaims and
//! then may take them (`deep`). So heap, page-table and DMA allocations, which cannot reclaim, still find frames
//! while the cache holds the rest (Linux keeps ALLOC_WMARK_MIN for the same reason). Cost on the per-CPU hit
//! path: none; on a per-CPU refill (once per `pcp::BATCH` single frames) and on every multi-frame allocation:
//! one relaxed load and compare under the zone lock already held.
//!
//! The rounds: a light one, the allocation retried after every round that freed pages; then thorough ones,
//! repeated (up to `reclaim::ROUNDS` in all) while a cache was passed over because its lock was taken. Only when
//! a thorough round found nothing with no cache busy (the rest of the cache is mapped or dirty), or the rounds
//! ran out, may the allocation take the reserve (as before MM-9: there is no OOM killer, so the last frames are
//! better used than refused). After a thorough round that found nothing the next `SKIP` hits go to the reserve
//! at once, so a cache that holds nothing to take is not scanned on every refill (the background thread keeps
//! looking). Freed frames go back through `free` on
//! this CPU (its per-CPU list takes them), so the retry finds them first.

use super::{Buddy, ZoneState, ZONES};
use crate::reclaim::{self, DirectStats, Reclaim};
use crate::{Kind, Zone};
use core::sync::atomic::Ordering::Relaxed;

/// Reserve hits that skip the light round after one found nothing.
const SKIP: u64 = 16;
impl Buddy {
    /// Installs the page cache's reclaimer (once) and sets each zone's reserve to its share of the min
    /// watermark. False: one was installed already.
    pub fn set_reclaimer(&self, r: &'static dyn Reclaim) -> bool {
        if !self.reclaimer.set(r) {
            return false;
        }
        let managed: [u64; ZONES] = core::array::from_fn(|z| self.managed[z].load(Relaxed));
        let total: u64 = managed.iter().sum();
        let min = reclaim::watermarks(total).min;
        for (z, m) in managed.iter().enumerate() {
            let share = if total == 0 { 0 } else { (min as u128 * *m as u128 / total as u128) as u64 };
            self.reserve[z].store(share, Relaxed);
        }
        true
    }

    /// This allocator's direct reclaim counts.
    pub fn direct_reclaim(&self) -> DirectStats {
        self.direct.get()
    }

    /// The reserve of each zone in frames.
    pub fn reserve_frames(&self) -> [u64; ZONES] {
        core::array::from_fn(|z| self.reserve[z].load(Relaxed))
    }

    /// Whether an allocation of `kind` must leave zone `zone`'s last frames (the zone lock is held).
    #[inline]
    pub(super) fn below_reserve(&self, zone: usize, zs: &ZoneState, kind: Kind) -> bool {
        zs.usable_free() < self.reserve[zone].load(Relaxed) && (reclaim::direct_allowed(kind) || (kind == Kind::Heap && reclaim::keeps_reserve()))
    }

    /// FS-STALL (the Mini-PC's panic: a kernel heap allocation of 75600 bytes failed beside 20 GB of clean page
    /// cache): the heap's step between two tries of an allocation that found no frames. Only where the caller's
    /// context allows it (`reclaim::context_allows`: no spin lock held, no interrupt handler) and not inside reclaim
    /// already (`reclaim::enter`). It takes clean cache pages under the `Reclaim` contract (try-locks only), waits
    /// for their grace period within `grace` (one budget for all tries of the allocation) and puts the per-CPU
    /// lists back into the buddy (try-locks), so freed frames merge. For a block of several frames it first
    /// compacts (W35 MM-3, `compact_direct_inside`; it replaced the cursor scan that dropped cache-only blocks):
    /// memory may be free but in pieces. True: something was freed or a block made (or a cache was busy), the heap
    /// tries again.
    #[cold]
    pub fn reclaim_for_heap(&self, order: u32, grace: &mut crate::grace::WaitBudget) -> bool {
        let Some(r) = self.reclaimer.get() else { return false };
        if !reclaim::context_allows() {
            return false;
        }
        // The heap noted its order under its lock: kswapd's wake waited for a context like this one.
        reclaim::wake_deferred();
        let Some(_inside) = reclaim::enter() else { return false };
        if order > 0 && self.compact_direct_inside(order, grace) {
            if self.global_reports {
                reclaim::HEAP_CLEARED.inc();
            }
            // The block exists, but below the reserve the heap is refused it (`below_reserve`): frames are short
            // too, so cache pages are taken as well. Without this the heap's rounds were compaction runs only,
            // hundreds of thousands in a minute, each under the zone lock (Mini-PC 11.11.2026, generation 114).
            if !self.all_below_reserve() {
                return true;
            }
        }
        let want = reclaim::CLUSTER.max(64u64 << order.min(14));
        let (mut freed, mut busy) = (0, false);
        for round in 0..reclaim::ROUNDS {
            let o = r.reclaim(want - freed.min(want - 1), round > 0);
            freed += o.freed;
            busy = o.busy;
            if freed >= want || (o.freed == 0 && !o.busy) {
                break;
            }
        }
        if freed > 0 {
            crate::grace::expedite_with(grace);
            if order > 0 {
                self.try_drain_all();
            }
        }
        reclaim::HEAP_RUNS.inc();
        reclaim::HEAP_PAGES.add(freed);
        freed > 0 || busy
    }

    /// Every zone has less usable free memory than its reserve.
    fn all_below_reserve(&self) -> bool {
        (0..ZONES).all(|z| self.zones[z].lock().usable_free() < self.reserve[z].load(Relaxed))
    }

    /// `drain_all` with try-locks: a list another CPU holds is left for later (the heap's step never waits).
    pub(super) fn try_drain_all(&self) -> u64 {
        let mut moved = 0;
        for cpu in self.pcp.iter_existing() {
            for (zone, lists) in cpu.lists.iter().enumerate() {
                for l in lists {
                    if let Some(mut list) = l.try_lock() {
                        let n = list.len();
                        moved += self.pcp_return(&mut list, zone, n);
                    }
                }
            }
        }
        moved
    }

    #[cold]
    pub(super) fn alloc_reclaiming(&self, order: u32, zone: Zone, kind: Kind, budget: &mut crate::grace::WaitBudget) -> Option<u64> {
        if !reclaim::direct_allowed(kind) {
            return None;
        }
        let r = self.reclaimer.get()?;
        reclaim::wake_deferred();
        let Some(_inside) = reclaim::enter() else { return self.grab(order, zone, kind, true) };
        if self.skip.try_update(Relaxed, Relaxed, |s| s.checked_sub(1)).is_ok() {
            self.direct.dips.fetch_add(1, Relaxed);
            return self.grab(order, zone, kind, true);
        }
        let want = reclaim::direct_target(order);
        let mut freed = 0;
        let mut got = None;
        for round in 0..reclaim::ROUNDS {
            let thorough = round > 0;
            let o = r.reclaim(want, thorough);
            freed += o.freed;
            if o.freed > 0 {
                // PF-5: reclaimed pages wait for a grace period (lockless readers may still copy from them);
                // the allocation waits for it (bounded, once per allocation: `budget`), so reclaim frees what it
                // reports instead of reclaiming more in the next round.
                crate::grace::expedite_with(budget);
                got = self.grab(order, zone, kind, false);
                if got.is_some() {
                    break;
                }
            } else if thorough && !o.busy {
                self.skip.store(SKIP, Relaxed);
                break;
            }
        }
        // W35 MM-3: a block of several frames may exist in pieces only (Linux `__alloc_pages_direct_compact`
        // after reclaim for the orders that may sleep).
        if got.is_none() && order > 0 && reclaim::context_allows() && self.compact_direct_inside(order, budget) {
            got = self.grab(order, zone, kind, false);
        }
        if got.is_none() {
            self.direct.dips.fetch_add(1, Relaxed);
            got = self.grab(order, zone, kind, true);
        }
        self.direct.runs.fetch_add(1, Relaxed);
        self.direct.pages.fetch_add(freed, Relaxed);
        let counter = if got.is_some() { &self.direct.rescued } else { &self.direct.failed };
        counter.fetch_add(1, Relaxed);
        if self.global_reports {
            reclaim::count_global(0, None);
            reclaim::count_global(freed, Some(got.is_some()));
        }
        got
    }
}
