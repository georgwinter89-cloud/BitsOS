//! The background side of the HighAtomic reserve (`mm_core::buddy::highatomic`): kswapd's look asks whether a
//! zone's reserve is low (`check`), kcompactd makes it whole again (`refill`: wholly free pageblocks in, the
//! pageblocks that long-lived blocks settled in out, a free pageblock made by compaction where none is).
//!
//! Mini-PC 12.11.2026: the reserve was made once at boot; what was taken and kept shrank it for good. No
//! allocation waits here: a taker only rings (`mm_core`'s `refill::ask`), and a pass that finds nothing to make a
//! pageblock of backs off (the next passes are skipped, twice as many each time, at most `BACKOFF`).

use core::sync::atomic::{AtomicU64, Ordering::Relaxed};
use mm_core::PAGE;

/// Wakes of kcompactd a failed pass is not tried again for at most (kcompactd wakes at most twice a second).
const BACKOFF: u64 = 64;

/// Passes still to skip, and how many the next failure skips.
static SKIP: AtomicU64 = AtomicU64::new(0);
static NEXT_SKIP: AtomicU64 = AtomicU64::new(1);
static PASSES: AtomicU64 = AtomicU64::new(0);

/// kswapd's look: a low reserve rings kcompactd (two zone locks; nothing more while it is not low).
pub(crate) fn check() {
    if let Some(b) = mm_core::boot_buddy() {
        b.reserve_check();
    }
}

/// kcompactd's wake: one refill pass when a reserve is low.
pub(crate) fn refill() {
    let Some(b) = mm_core::boot_buddy() else { return };
    if !b.reserve_low() {
        NEXT_SKIP.store(1, Relaxed);
        SKIP.store(0, Relaxed);
        return;
    }
    if SKIP.load(Relaxed) > 0 {
        SKIP.fetch_sub(1, Relaxed);
        return;
    }
    let r = b.refill_reserve(&mut sched::yield_now);
    let s = b.reserve_stats();
    let first = PASSES.fetch_add(1, Relaxed) == 0;
    let (free_kb, pageblocks) = (s.free.iter().sum::<u64>() * PAGE / 1024, s.pageblocks.iter().sum::<u64>());
    if r.short > 0 {
        let skip = NEXT_SKIP.load(Relaxed);
        SKIP.store(skip, Relaxed);
        NEXT_SKIP.store((skip * 2).min(BACKOFF), Relaxed);
        klog::event!(Warn, "mm", "reserve.refill_short", reserved = r.reserved, handed_over = r.handed, free_kb = free_kb, pageblocks = pageblocks,
            skip = skip; "no free pageblock for the HighAtomic reserve and none could be made: the next passes are skipped");
    } else {
        NEXT_SKIP.store(1, Relaxed);
        if first {
            klog::event!(Info, "mm", "reserve.refill", reserved = r.reserved, handed_over = r.handed, free_kb = free_kb, pageblocks = pageblocks;
                "the HighAtomic reserve was low: free pageblocks joined it, the ones in long use left it");
        } else {
            klog::event!(Debug, "mm", "reserve.refill", reserved = r.reserved, handed_over = r.handed, free_kb = free_kb, pageblocks = pageblocks);
        }
    }
}
