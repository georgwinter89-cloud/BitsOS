//! Host tests of the heap's scattered large blocks (`scatter`) with a fake hook over a host arena: when the
//! fallback is taken (above the costly order, page alignment, a context that may reclaim, the run missing),
//! realloc in place and moving, frees routed by address, refused bad frees, deferral under a spin lock and the
//! drain, and the statistics.
extern crate std;
use super::heap::{fill, heap, holds, host_buddy, host_memory, lay};
use crate::scatter::Scatter;
use crate::{Buddy, FrameAlloc, Kind, KernelHeap, Zone, PAGE};
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::boxed::Box;
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::vec::Vec;

const ARENA: usize = 64 << 20;

/// Blocks carved bottom-up from a leaked host buffer, a guard page before each; never reused.
struct Fake {
    lo: usize,
    next: AtomicUsize,
    live: Mutex<BTreeMap<usize, u64>>,
    fail: AtomicBool,
    frees: AtomicU64,
    kicks: AtomicU64,
}

impl Scatter for Fake {
    fn span(&self) -> (usize, usize) {
        (self.lo, self.lo + ARENA)
    }
    fn alloc(&self, pages: u64) -> Option<usize> {
        if self.fail.load(Relaxed) {
            return None;
        }
        let len = (pages + 1) as usize * PAGE as usize;
        let at = self.next.fetch_add(len, Relaxed);
        if at + len > self.lo + ARENA {
            return None;
        }
        let v = at + PAGE as usize;
        self.live.lock().unwrap().insert(v, pages);
        Some(v)
    }
    fn free(&self, v: usize, pages: u64) -> bool {
        let mut l = self.live.lock().unwrap();
        if l.get(&v) != Some(&pages) {
            return false;
        }
        l.remove(&v);
        self.frees.fetch_add(1, Relaxed);
        true
    }
    fn looks_live(&self, v: usize, pages: u64) -> bool {
        self.live.lock().unwrap().get(&v) == Some(&pages)
    }
    fn kick(&self) {
        self.kicks.fetch_add(1, Relaxed);
    }
}

fn fake() -> &'static Fake {
    let lo = host_memory(ARENA, PAGE as usize);
    Box::leak(Box::new(Fake {
        lo,
        next: AtomicUsize::new(lo),
        live: Mutex::new(BTreeMap::new()),
        fail: AtomicBool::new(false),
        frees: AtomicU64::new(0),
        kicks: AtomicU64::new(0),
    }))
}

/// A heap over a small buddy with the fake hook installed.
fn setup(mib: usize) -> (&'static Buddy, &'static KernelHeap, &'static Fake) {
    let b = host_buddy(mib);
    let h = heap(Some(b), None, 1);
    let f = fake();
    assert!(h.set_scatter(f));
    assert!(!h.set_scatter(f), "one hook only");
    (b, h, f)
}

/// Takes every free frame of `b` (biggest blocks first), so no run of any size is left.
fn exhaust(b: &Buddy) -> Vec<(u64, u32)> {
    let mut got = Vec::new();
    for order in (0..=crate::MAX_ORDER).rev() {
        while let Some(p) = b.alloc(order, Zone::Normal, Kind::Other) {
            got.push((p, order));
        }
    }
    got
}

fn give_back(b: &Buddy, taken: Vec<(u64, u32)>) {
    for (p, o) in taken {
        b.free(p, o, Kind::Other);
    }
}

fn in_span(f: &Fake, p: *mut u8) -> bool {
    let (lo, hi) = f.span();
    (lo..hi).contains(&(p as usize))
}

fn free_block(h: &KernelHeap, p: *mut u8, l: core::alloc::Layout) {
    // SAFETY: each test frees its blocks once, with their layouts (or hands in pointers the heap must refuse).
    unsafe { h.dealloc(p, l) };
}

#[test]
fn fallback_when_the_run_is_missing() {
    let (b, h, f) = setup(4);
    // A run that exists stays a run of the direct map.
    let l = lay(64 << 10, 8);
    let p = h.alloc(l);
    assert!(!p.is_null() && !in_span(f, p));
    free_block(h, p, l);
    assert_eq!(h.stats().scatter.allocs, 0);
    // Bigger than the whole buddy: single frames.
    let big = lay(8 << 20, 8);
    let p = h.alloc(big);
    assert!(in_span(f, p), "8 MiB from a 4 MiB buddy");
    assert_eq!(p as usize % PAGE as usize, 0);
    fill(p, big.size(), 0xA5);
    assert!(holds(p, big.size(), 0xA5));
    let taken = exhaust(b);
    // 32 KiB is order 3: a run first, and single frames only once reclaim found none (the late step).
    let l8 = lay(32 << 10, 8);
    let r8 = h.alloc(l8);
    assert!(in_span(f, r8), "a run up to the costly order falls back late");
    assert_eq!(f.live.lock().unwrap().get(&(r8 as usize)), Some(&8));
    free_block(h, r8, l8);
    // One page is a frame, never a mapping.
    assert!(h.alloc(lay(PAGE as usize, 8)).is_null());
    // One page more: the fallback.
    let l9 = lay((32 << 10) + 1, 8);
    let q = h.alloc(l9);
    assert!(in_span(f, q));
    assert_eq!(f.live.lock().unwrap().get(&(q as usize)), Some(&9));
    // Aligned above a page: never scattered.
    assert!(h.alloc(lay(64 << 10, 8192)).is_null());
    // Under a spin lock (no reclaim allowed): no fallback.
    {
        let _p = ksync::PreemptGuard::new();
        assert!(h.alloc(lay(64 << 10, 8)).is_null(), "no fallback where reclaim may not run");
    }
    // The fallback failing too: null after the reclaim rounds.
    f.fail.store(true, Relaxed);
    assert!(h.alloc(lay(64 << 10, 8)).is_null());
    f.fail.store(false, Relaxed);
    let s = h.stats();
    assert_eq!(s.scatter.allocs, 3);
    assert_eq!(s.scatter.held_bytes, (8 << 20) + 9 * PAGE);
    assert_eq!((s.large_blocks, s.large_bytes), (2, (8 << 20) + 9 * PAGE));
    assert_eq!(s.held_by_kind[Kind::Heap as usize], s.scatter.held_bytes, "scattered frames counted as the heap's");
    h.check().unwrap();
    free_block(h, p, big);
    free_block(h, q, l9);
    give_back(b, taken);
    let s = h.stats();
    assert_eq!((s.scatter.frees, s.scatter.held_bytes, s.large_blocks, s.large_bytes, s.requested_bytes), (3, 0, 0, 0, 0));
    assert!(f.live.lock().unwrap().is_empty());
    h.check().unwrap();
}

#[test]
fn realloc_in_place_grows_and_shrinks() {
    let (b, h, f) = setup(4);
    let taken = exhaust(b);
    let l = lay(60_000, 8);
    let p = h.alloc(l);
    assert!(in_span(f, p));
    fill(p, l.size(), 7);
    // SAFETY: `p` is this test's live block of layout `l`; every realloc below hands in the block the previous
    // one returned, with its layout.
    let same = unsafe { h.realloc(p, l, 61_000) };
    assert_eq!(same, p, "15 pages either way: in place");
    assert_eq!(h.allocated_bytes(), 61_000);
    let l2 = lay(61_000, 8);
    // SAFETY: as above.
    let grown = unsafe { h.realloc(same, l2, 3 << 20) };
    assert!(in_span(f, grown) && grown != p);
    assert!(holds(grown, 60_000, 7), "the content moved along");
    assert!(!f.live.lock().unwrap().contains_key(&(p as usize)), "the old block went back");
    give_back(b, taken);
    // SAFETY: as above.
    let small = unsafe { h.realloc(grown, lay(3 << 20, 8), 1000) };
    assert!(!small.is_null() && !in_span(f, small), "back into a slab");
    assert!(holds(small, 1000, 7));
    free_block(h, small, lay(1000, 8));
    let s = h.stats();
    assert_eq!((s.scatter.allocs, s.scatter.frees, s.scatter.held_bytes, s.requested_bytes), (2, 2, 0, 0));
    h.check().unwrap();
}

#[test]
fn bad_frees_of_scattered_blocks_are_refused() {
    let (b, h, f) = setup(4);
    let taken = exhaust(b);
    let l = lay(100 << 10, 8);
    let p = h.alloc(l);
    assert!(in_span(f, p));
    let before = (h.reports().heap_bad_frees.load(Relaxed), h.stats().large_bytes, h.allocated_bytes());
    free_block(h, p, lay(200 << 10, 8));
    // SAFETY: an interior pointer the heap must refuse without touching it.
    free_block(h, unsafe { p.add(PAGE as usize) }, l);
    free_block(h, (f.lo + ARENA - PAGE as usize) as *mut u8, l);
    {
        let _p = ksync::PreemptGuard::new();
        free_block(h, (p as usize + 3) as *mut u8, l);
        free_block(h, p, lay(200 << 10, 8));
    }
    assert_eq!(h.reports().heap_bad_frees.load(Relaxed), before.0 + 5);
    assert_eq!((h.stats().large_bytes, h.allocated_bytes()), (before.1, before.2), "a refused free changed the numbers");
    free_block(h, p, l);
    free_block(h, p, l);
    assert_eq!(h.reports().heap_bad_frees.load(Relaxed), before.0 + 6, "a double free is refused");
    give_back(b, taken);
    h.check().unwrap();
}

#[test]
fn frees_under_a_spin_lock_wait_for_the_drain() {
    let (b, h, f) = setup(4);
    let taken = exhaust(b);
    let ls: Vec<_> = (1..=3).map(|i| lay(i * (40 << 10), 8)).collect();
    let ps: Vec<_> = ls.iter().map(|&l| h.alloc(l)).collect();
    assert!(ps.iter().all(|&p| in_span(f, p)));
    let held = h.stats().scatter.held_bytes;
    {
        let _p = ksync::PreemptGuard::new();
        for (&p, &l) in ps.iter().zip(&ls) {
            free_block(h, p, l);
        }
        assert_eq!(h.drain_scattered(), 0, "no drain where reclaim may not run");
    }
    let s = h.stats();
    assert_eq!((s.scatter.deferred, s.scatter.frees, f.frees.load(Relaxed)), (3, 0, 0));
    assert_eq!((s.scatter.held_bytes, s.scatter.waiting_bytes), (held, held), "still held, all waiting");
    assert_eq!((s.large_blocks, s.requested_bytes), (0, 0), "the users gave them up");
    assert_eq!(f.kicks.load(Relaxed), 0, "no wake-up under a spin lock");
    h.check().unwrap();
    assert_eq!(h.drain_scattered(), 3);
    let s = h.stats();
    assert_eq!((s.scatter.frees, s.scatter.held_bytes, s.scatter.waiting_bytes), (3, 0, 0));
    assert!(f.live.lock().unwrap().is_empty());
    // The next scattered free in a good context drains what waits.
    let (a, c) = (h.alloc(ls[0]), h.alloc(ls[1]));
    {
        let _p = ksync::PreemptGuard::new();
        free_block(h, a, ls[0]);
    }
    free_block(h, c, ls[1]);
    let s = h.stats();
    assert_eq!((s.scatter.frees, s.scatter.waiting_bytes, s.scatter.held_bytes), (5, 0, 0));
    give_back(b, taken);
    h.check().unwrap();
}

#[test]
fn a_second_free_of_a_waiting_block_is_refused() {
    let (b, h, f) = setup(4);
    let taken = exhaust(b);
    let l = lay(100 << 10, 8);
    let p = h.alloc(l);
    assert!(in_span(f, p));
    let bad = h.reports().heap_bad_frees.load(Relaxed);
    {
        let _p = ksync::PreemptGuard::new();
        free_block(h, p, l);
        free_block(h, p, l);
    }
    assert_eq!(h.reports().heap_bad_frees.load(Relaxed), bad + 1, "pushed twice");
    // Still mapped and listed: freeing it now would leave the drain reading an unmapped block.
    free_block(h, p, l);
    assert_eq!(h.reports().heap_bad_frees.load(Relaxed), bad + 2, "freed while it waits");
    let s = h.stats();
    assert_eq!((s.scatter.deferred, s.scatter.frees, s.large_blocks), (1, 0, 0));
    assert_eq!(h.drain_scattered(), 1);
    free_block(h, p, l);
    assert_eq!(h.reports().heap_bad_frees.load(Relaxed), bad + 3, "freed after the drain");
    let s = h.stats();
    assert_eq!((s.scatter.frees, s.scatter.held_bytes, s.scatter.waiting_bytes), (1, 0, 0));
    assert!(f.live.lock().unwrap().is_empty());
    give_back(b, taken);
    h.check().unwrap();
}

/// A missing run notes its order for kswapd where nothing else serves it (a spin lock: null); a run that exists,
/// a single page, or a block single frames serve at once notes nothing (nobody has to make that run).
#[test]
fn a_missing_run_notes_its_order_only_where_it_is_needed() {
    let (b, h, f) = setup(4);
    let w = h.order_want();
    let l = lay(64 << 10, 8);
    let p = h.alloc(l);
    assert!(!in_span(f, p));
    free_block(h, p, l);
    assert_eq!((w.pending(), w.noted()), (0, 0), "the run was there");
    let taken = exhaust(b);
    let q = h.alloc(l);
    assert!(in_span(f, q), "scattered");
    assert_eq!((w.take(), w.noted()), (0, 0), "single frames served it: no run asked for");
    {
        let _p = ksync::PreemptGuard::new();
        assert!(h.alloc(lay(9 << 10, 8)).is_null());
        assert_eq!(w.pending(), 1 << 2, "noted under a spin lock too");
        assert!(h.alloc(l).is_null());
        assert!(h.alloc(lay(3000, 4096)).is_null());
    }
    assert_eq!((w.take(), w.noted()), (1 << 2 | 1 << 4, 2), "both orders; a single page is not counted");
    free_block(h, q, l);
    give_back(b, taken);
    h.check().unwrap();
}
