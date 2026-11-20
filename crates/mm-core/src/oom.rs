//! MEMCRASH row 2: allocations too small to fail, and the OOM killer's hook (linux-7.3.1/mm/page_alloc.c:4784
//! `__alloc_pages_slowpath`, :4633 `should_reclaim_retry` with MAX_RECLAIM_RETRIES, :4044 `__alloc_pages_may_oom`;
//! mm/oom_kill.c:1117 `out_of_memory`).
//!
//! A kernel heap block or a program/cache frame run of order <= `COSTLY_ORDER` (Linux `PAGE_ALLOC_COSTLY_ORDER`)
//! asked for where the caller may sleep does not fail. Once its normal tries are spent it loops (`Stall`): kswapd
//! and kcompactd are woken, the caller sleeps a millisecond, reclaims again and tries again. After
//! `MAX_RECLAIM_RETRIES` rounds in a row that freed nothing the OOM killer (crate proc installs it) picks the
//! largest program and ends it; while that victim is still going away the loop waits for it. Only when there is
//! nothing to kill does the allocation answer null, which Linux answers with a panic too ("System is deadlocked on
//! memory"). Mini-PC 13.11.2026: a 22 KiB copy of a program's VMA list gave up after 100 ms of reclaim, its caller
//! held `mm.as` while others waited (`mm.heap.patience_cut`), and the null was a kernel panic beside 22 GB of page
//! cache.
//!
//! Callers that may not sleep (a spin lock, an interrupt handler) keep the old bounded path and the reserves, as
//! Linux's atomic allocations do; so do the memory helpers (inside reclaim, kswapd, kcompactd, the OOM reaper:
//! Linux PF_MEMALLOC, `mark_memalloc`) and allocations that handle a refusal themselves (`reclaim::hurry`,
//! `no_retry`: Linux __GFP_NORETRY). The rounds keep the min watermark and HighAtomic (`reclaim::Keep::hold`);
//! a thread whose program is being ended takes them (Linux ALLOC_OOM).

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering::*};
use ksync::OnceRef;

/// Linux `PAGE_ALLOC_COSTLY_ORDER`: blocks up to 32 KiB never fail where the caller may sleep.
pub const COSTLY_ORDER: u32 = 3;
/// Linux `MAX_RECLAIM_RETRIES`: rounds without progress before the OOM killer is asked.
pub const MAX_RECLAIM_RETRIES: u32 = 16;
/// The sleep between two rounds (Linux sleeps a tick in `__alloc_pages_may_oom` and throttles reclaim).
const PAUSE_NS: u64 = 1_000_000;

/// What the OOM killer did (`Killer::out_of_memory`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// A program was chosen and is being ended now.
    Killed,
    /// A program chosen earlier is still going away (Linux: an OOM victim without `MMF_OOM_SKIP` ends the scan).
    Pending,
    /// Nothing that may be killed is left.
    NoVictim,
}

/// The OOM killer (crate proc: it knows the programs; mm-core only the frames).
pub trait Killer: Sync {
    /// Linux `out_of_memory` for an allocation of 2^`order` frames. Called where the caller may sleep, with no
    /// spin lock held; must not allocate from the kernel heap (the caller is the heap's slow path).
    fn out_of_memory(&self, order: u32) -> Verdict;
    /// The calling thread's program is being ended (a frame allocation of it may fail then: the program never
    /// sees the result, Linux `tsk_is_oom_victim` / fatal signal in the slow path).
    fn current_dying(&self) -> bool;
}

static KILLER: OnceRef<dyn Killer> = OnceRef::new();
/// Linux `oom_lock`: one caller asks the killer at a time; the others count it as progress and wait.
static OOM_LOCK: AtomicBool = AtomicBool::new(false);
static PAUSE: ksync::WaitQueue = ksync::WaitQueue::new();

klog::counter!(pub NOFAIL_ALLOCS, "mm.alloc.nofail_allocs");
klog::counter!(pub NOFAIL_ROUNDS, "mm.alloc.nofail_rounds");
klog::counter!(pub NOFAIL_FAILED, "mm.alloc.nofail_failed");
// The longest any one allocation spent in these rounds (a measurement: two at once may leave the smaller).
klog::counter!(pub NOFAIL_MAX_US, "mm.alloc.nofail_max_us");
klog::counter!(pub OOM_CALLS, "mm.oom.calls");
klog::counter!(pub OOM_KILLS, "mm.oom.kills");
klog::counter!(pub OOM_NO_VICTIM, "mm.oom.no_victim");

/// Installs the OOM killer (once; false if one is installed).
pub fn set_killer(k: &'static dyn Killer) -> bool {
    KILLER.set(k)
}

/// Whether an allocation of 2^`order` frames by the calling thread must not fail: small enough, and the caller may
/// sleep, is not one that handles refusals itself (`hurried`, `no_retry`) and is not a memory helper
/// (`memalloc`: inside reclaim, kswapd, kcompactd, the OOM reaper).
pub fn may_nofail(order: u32) -> bool {
    order <= COSTLY_ORDER
        && ksync::thread_id() != 0
        && crate::reclaim::context_allows()
        && !memalloc()
        && !crate::reclaim::hurried()
        && !in_table(&NO_RETRY, ksync::thread_id())
}

/// Threads that free memory for others for their whole life (Linux PF_MEMALLOC on kswapd; the OOM reaper and
/// kcompactd here too): their allocations take the reserves at once, are not patient and never loop in
/// `Stall`. Checker 13.11.2026: the OOM reaper allocated on the never-failing path, asked the killer, got
/// `Pending` for its own victim and held that victim's `mm.as` meanwhile: the machine stood still.
static MEMALLOC: [AtomicU64; 8] = [const { AtomicU64::new(0) }; 8];
/// Threads inside a `no_retry` scope (Linux __GFP_NORETRY / GFP_NOWAIT): their allocations handle a refusal.
static NO_RETRY: [AtomicU64; 64] = [const { AtomicU64::new(0) }; 64];

fn in_table(t: &[AtomicU64], me: u64) -> bool {
    me != 0 && t.iter().any(|s| s.load(Relaxed) == me)
}

/// Marks the calling thread as a memory helper for the rest of its life (`may_nofail`). False: no slot or no
/// thread id (the thread then allocates as any other).
pub fn mark_memalloc() -> bool {
    let me = ksync::thread_id();
    me != 0 && (in_table(&MEMALLOC, me) || MEMALLOC.iter().any(|s| s.compare_exchange(0, me, Relaxed, Relaxed).is_ok()))
}

/// Whether the calling thread frees memory for others: inside reclaim, or marked (`mark_memalloc`).
pub fn memalloc() -> bool {
    crate::reclaim::reclaiming() || in_table(&MEMALLOC, ksync::thread_id())
}

/// Allocations of the calling thread until the guard drops handle a refusal (try_reserve, a grace block, an
/// index node): they keep their bounded path and never enter the never-failing loop or the OOM killer. Before
/// MEMCRASH such a refusal had a fallback (drain and wait, leak-free); a loop there could kill a program while
/// this very thread was freeing memory. Nested scopes and a full table: no guard, the outer scope (if any) holds.
pub fn no_retry() -> Option<NoRetry> {
    let me = ksync::thread_id();
    if me == 0 || in_table(&NO_RETRY, me) {
        return None;
    }
    NO_RETRY.iter().position(|s| s.compare_exchange(0, me, Relaxed, Relaxed).is_ok()).map(NoRetry)
}

pub struct NoRetry(usize);

impl Drop for NoRetry {
    fn drop(&mut self) {
        NO_RETRY[self.0].store(0, Relaxed);
    }
}

/// Whether the calling thread's program is being ended by a kill (`Killer::current_dying`).
pub fn current_dying() -> bool {
    KILLER.get().is_some_and(|k| k.current_dying())
}

/// Linux `__alloc_pages_may_oom`: asks the killer once (the caller found nothing for `MAX_RECLAIM_RETRIES` rounds).
pub fn out_of_memory(order: u32) -> Verdict {
    if order > COSTLY_ORDER {
        return Verdict::NoVictim;
    }
    let Some(k) = KILLER.get() else {
        OOM_NO_VICTIM.inc();
        return Verdict::NoVictim;
    };
    if OOM_LOCK.swap(true, Acquire) {
        // Someone else asks the killer now: that is progress for this caller too.
        return Verdict::Pending;
    }
    OOM_CALLS.inc();
    let v = k.out_of_memory(order);
    OOM_LOCK.store(false, Release);
    match v {
        Verdict::Killed => OOM_KILLS.inc(),
        Verdict::NoVictim => OOM_NO_VICTIM.inc(),
        Verdict::Pending => {}
    }
    v
}

/// One allocation's loop once its normal tries are spent (`Stall::next` after each try that found nothing).
pub struct Stall {
    order: u32,
    no_progress: u32,
    since: Option<u64>,
}

impl Drop for Stall {
    fn drop(&mut self) {
        let (Some(t0), Some(t1)) = (self.since, ksync::now_ns()) else { return };
        let us = t1.saturating_sub(t0) / 1000;
        let cur = NOFAIL_MAX_US.get();
        if us > cur {
            NOFAIL_MAX_US.add(us - cur);
        }
    }
}

impl Stall {
    pub fn new(order: u32) -> Stall {
        NOFAIL_ALLOCS.inc();
        Stall { order, no_progress: 0, since: ksync::now_ns() }
    }

    /// The next round asks the OOM killer unless it makes progress: its try may take every reserve first (Linux
    /// `should_reclaim_retry` unreserves the HighAtomic pageblocks before the OOM killer).
    pub fn oom_next(&self) -> bool {
        self.no_progress >= MAX_RECLAIM_RETRIES
    }

    /// Between two tries: `progress` is whether this round's reclaim freed anything (Linux `did_some_progress`).
    /// True: sleep done, try again. False: nothing to kill was left (`Verdict::NoVictim`): the caller fails.
    pub fn next(&mut self, progress: bool) -> bool {
        NOFAIL_ROUNDS.inc();
        self.no_progress = if progress { 0 } else { self.no_progress + 1 };
        if self.no_progress > MAX_RECLAIM_RETRIES {
            match out_of_memory(self.order) {
                Verdict::Killed | Verdict::Pending => self.no_progress = 0,
                Verdict::NoVictim => {
                    NOFAIL_FAILED.inc();
                    return false;
                }
            }
        }
        self.pause();
        true
    }

    /// kswapd and kcompactd are asked for the block, and the caller sleeps a millisecond while they work.
    fn pause(&self) {
        crate::reclaim::wake_kswapd();
        crate::compact::wake_kcompactd(self.order);
        crate::reclaim::wake_deferred();
        PAUSE.wait_until_timeout(|| false, PAUSE_NS);
    }
}

/// Linux `OOM_SCORE_ADJ_MIN`: a program with this `oom_score_adj` is never chosen.
pub const SCORE_ADJ_MIN: i32 = -1000;

/// Linux `oom_badness` (mm/oom_kill.c:201): the pages a kill would give back (resident pages and page tables; V2
/// has no swap) plus `oom_score_adj` thousandths of all memory (`total` pages). None: never to be chosen.
pub fn badness(rss: u64, page_tables: u64, score_adj: i32, total: u64) -> Option<i64> {
    if score_adj <= SCORE_ADJ_MIN {
        return None;
    }
    let points = i64::try_from(rss.saturating_add(page_tables)).unwrap_or(i64::MAX);
    let adj = i64::from(score_adj).saturating_mul(i64::try_from(total / 1000).unwrap_or(i64::MAX));
    Some(points.saturating_add(adj))
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;

    #[test]
    fn badness_is_memory_plus_the_adjustment() {
        assert_eq!(badness(100, 3, 0, 1_000_000), Some(103));
        // +500 is half of all memory on top, -500 half below (a negative score still counts, as in Linux).
        assert_eq!(badness(100, 0, 500, 1_000_000), Some(100 + 500_000));
        assert_eq!(badness(100, 0, -500, 1_000_000), Some(100 - 500_000));
        assert_eq!(badness(u64::MAX, 0, 0, 0), Some(i64::MAX));
    }

    #[test]
    fn minus_thousand_is_never_chosen() {
        assert_eq!(badness(1 << 30, 0, SCORE_ADJ_MIN, 1 << 20), None);
    }

    /// Checker 13.11.2026: a freeing path's fallible allocation and a memory helper (the OOM reaper, kswapd) never
    /// enter the never-failing loop; the scope ends with its guard, the mark with the thread.
    #[test]
    fn no_retry_scopes_and_memory_helpers_keep_the_bounded_path() {
        std::thread::spawn(|| {
            assert!(may_nofail(0) && may_nofail(COSTLY_ORDER) && !may_nofail(COSTLY_ORDER + 1));
            {
                let g = no_retry();
                assert!(g.is_some() && !may_nofail(0));
                assert!(no_retry().is_none(), "a nested scope is the outer one's");
                assert!(!may_nofail(0));
            }
            assert!(may_nofail(0), "the scope ended with its guard");
            assert!(!memalloc());
            assert!(mark_memalloc() && mark_memalloc(), "marking twice keeps one slot");
            assert!(memalloc() && !may_nofail(0));
            assert_eq!(MEMALLOC.iter().filter(|s| s.load(Relaxed) == ksync::thread_id()).count(), 1);
            MEMALLOC.iter().filter(|s| s.load(Relaxed) == ksync::thread_id()).for_each(|s| s.store(0, Relaxed));
        })
        .join()
        .unwrap();
    }

    #[test]
    fn only_small_blocks_reach_the_killer() {
        assert_eq!(out_of_memory(COSTLY_ORDER + 1), Verdict::NoVictim);
    }
}
