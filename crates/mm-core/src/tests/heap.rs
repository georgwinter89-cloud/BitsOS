//! Host tests of the slab heap (MM-3) on real host memory: a staged buddy allocator over a leaked host buffer
//! ("physical" = host address), simulated CPUs for the magazines, the seed path, exact page runs, refused
//! bad frees, reclaim on exhaustion, many threads with cross-CPU frees and CPU changes, and a random run
//! against a model with `check()` and the statistics compared to it.
use super::*;
use crate::mags::MagSlots;
use crate::pages::PageSrc;
use crate::slab::geo::{CLASSES, NCLASSES};
use crate::{Buddy, KernelHeap};
use core::alloc::Layout;
use core::cell::Cell;
use core::sync::atomic::Ordering::Relaxed;
use std::boxed::Box;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

std::thread_local! {
    static CPU: Cell<usize> = const { Cell::new(0) };
}
pub(super) fn cpu() -> usize {
    CPU.with(|c| c.get())
}
pub(super) fn set_cpu(n: usize) {
    CPU.with(|c| c.set(n));
}

/// `bytes` of host memory aligned to `align`, never freed.
pub(super) fn host_memory(bytes: usize, align: usize) -> usize {
    // SAFETY: a non-zero size and a power-of-two alignment.
    let p = unsafe { std::alloc::alloc(Layout::from_size_align(bytes, align).unwrap()) };
    assert!(!p.is_null());
    p as usize
}

/// A buddy allocator over `mib` MiB of host memory.
pub(super) fn host_buddy(mib: usize) -> &'static Buddy {
    let m = host_memory(mib << 20, 2 << 20) as u64;
    let b = crate::BuddyFrames::staged("heap-test", &[free(m, m + ((mib as u64) << 20))], crate::Storage::Heap).unwrap();
    let b: &'static crate::BuddyFrames = Box::leak(Box::new(b));
    b
}

pub(super) fn heap(buddy: Option<&'static Buddy>, seed: Option<(usize, usize)>, cpus: usize) -> &'static KernelHeap {
    let mags = if cpus == 0 { MagSlots::Off } else { MagSlots::host(cpus, cpu) };
    KernelHeap::for_test(PageSrc::for_test(buddy, seed), mags)
}

pub(super) fn lay(size: usize, align: usize) -> Layout {
    Layout::from_size_align(size, align).unwrap()
}

pub(super) fn fill(p: *mut u8, len: usize, tag: u8) {
    // SAFETY: `p` is a live block of at least `len` bytes the test owns.
    unsafe { core::ptr::write_bytes(p, tag, len) };
}

pub(super) fn holds(p: *mut u8, len: usize, tag: u8) -> bool {
    // SAFETY: as `fill`.
    unsafe { core::slice::from_raw_parts(p, len) }.iter().all(|&b| b == tag)
}

pub(super) fn free_block(h: &KernelHeap, p: *mut u8, l: Layout) {
    // SAFETY: the tests free each block once with the layout it was allocated with.
    unsafe { h.dealloc(p, l) };
}

pub(super) fn heap_frames(b: &Buddy) -> u64 {
    b.stats().used_by_kind[Kind::Heap as usize]
}

#[test]
fn every_class_and_alignment() {
    let b = host_buddy(64);
    let h = heap(Some(b), None, 2);
    let mut live = Vec::new();
    for size in (1..=5000).step_by(7).chain(CLASSES) {
        for align in [1, 8, 16, 64, 256, 2048, 4096, 16384] {
            let l = lay(size, align);
            let p = h.alloc(l);
            assert!(!p.is_null(), "{size}/{align}");
            assert_eq!(p as usize % align, 0, "{size}/{align} misaligned");
            fill(p, size, (size ^ align) as u8);
            live.push((p, l));
        }
    }
    h.check().unwrap();
    for &(p, l) in &live {
        assert!(holds(p, l.size(), (l.size() ^ l.align()) as u8), "block overwritten");
    }
    for (p, l) in live {
        free_block(h, p, l);
    }
    h.check().unwrap();
    assert_eq!(h.allocated_bytes(), 0);
    h.shrink();
    h.check().unwrap();
    let s = h.stats();
    assert_eq!(s.slab_bytes, 0, "every slab back");
    assert_eq!((s.large_bytes, s.large_blocks, s.held_by_kind[Kind::Heap as usize]), (0, 0, 0));
    assert_eq!(heap_frames(b), 0, "the buddy allocator has everything back");
    b.check().unwrap();
}

#[test]
fn exact_page_runs() {
    let b = host_buddy(16);
    let h = heap(Some(b), None, 1);
    for pages in 1..=40usize {
        let l = lay(pages * 4096 - 100, 8);
        let p = h.alloc(l);
        assert_eq!(heap_frames(b), pages as u64 * PAGE, "{pages} pages charged exactly");
        free_block(h, p, l);
        assert_eq!(heap_frames(b), 0);
    }
    // Aligned above a page: 1 page aligned to 64 KiB.
    let l = lay(100, 65536);
    let p = h.alloc(l);
    assert_eq!(p as usize % 65536, 0);
    assert_eq!(heap_frames(b), PAGE);
    free_block(h, p, l);
    b.check().unwrap();
    assert_eq!(b.reports().bad_frees.load(Relaxed), 0);
    assert_eq!(crate::buddy::exact_order(5, 1), Some(3));
    assert_eq!(crate::buddy::exact_order(1, 16), Some(4));
    assert_eq!(crate::buddy::exact_order(1 << 18, 1), Some(18));
    assert_eq!(crate::buddy::exact_order((1 << 18) + 1, 1), None);
}

#[test]
fn bad_frees_are_refused() {
    let b = host_buddy(16);
    let h = heap(Some(b), None, 2);
    let keep: Vec<_> = (0..100).map(|i| (h.alloc(lay(40, 8)), i as u8)).collect();
    for &(p, t) in &keep {
        fill(p, 40, t);
    }
    assert_eq!(h.probe_bad_frees(), Ok(crate::PROBE_REFUSALS));
    h.check().unwrap();
    assert!(keep.iter().all(|&(p, t)| holds(p, 40, t)), "a refused free touched another block");
    assert_eq!(b.reports().bad_frees.load(Relaxed), 0, "refused before the buddy allocator saw it");
    // A double free straight to the cache (no magazine on this CPU).
    let p = h.alloc(lay(24, 8));
    set_cpu(7);
    free_block(h, p, lay(24, 8));
    free_block(h, p, lay(24, 8));
    set_cpu(0);
    assert_eq!(h.reports().heap_bad_frees.load(Relaxed), crate::PROBE_REFUSALS + 1);
    h.check().unwrap();
}

#[test]
fn empty_slabs_go_back() {
    let b = host_buddy(32);
    let h = heap(Some(b), None, 1);
    let l = lay(64, 8);
    let v: Vec<_> = (0..20_000).map(|_| h.alloc(l)).collect();
    let full = heap_frames(b);
    assert!(full >= 20_000 * 64, "{full}");
    for &p in &v {
        free_block(h, p, l);
    }
    // Without a shrink: at most the magazine's objects keep slabs, plus one empty slab per class.
    let left = heap_frames(b);
    assert!(left <= 3 * 4096, "slabs not returned: {left} bytes still held");
    h.shrink();
    assert_eq!(heap_frames(b), 0);
    h.check().unwrap();
}

#[test]
fn seed_serves_before_the_buddy_exists() {
    let seed = host_memory(4 << 20, 4 << 20);
    let h = heap(None, Some((seed, 4 << 20)), 1);
    let mut v = Vec::new();
    for i in 0..2000 {
        let l = lay(1 + i % 700, 8);
        let p = h.alloc(l);
        assert!((seed..seed + (4 << 20)).contains(&(p as usize)));
        v.push((p, l));
    }
    let big = lay(3 * 4096, 4096);
    let pb = h.alloc(big);
    assert!(!pb.is_null() && (pb as usize) >= seed);
    h.check().unwrap();
    for (p, l) in v.drain(..) {
        free_block(h, p, l);
    }
    free_block(h, pb, big);
    h.shrink();
    let s = h.stats();
    assert_eq!(s.seed.lost, 4 * PAGE, "a seed large block and its header page are not reused");
    assert_eq!(s.seed.free, s.seed.carved - 4 * PAGE, "all slabs back on the seed lists");
    assert_eq!(h.reports().heap_bad_frees.load(Relaxed), 0);
    // A seed run freed twice, or with the wrong size, is refused.
    let r = h.alloc(lay(5000, 8));
    free_block(h, r, lay(9000, 8));
    free_block(h, r, lay(5000, 8));
    free_block(h, r, lay(5000, 8));
    assert_eq!(h.reports().heap_bad_frees.load(Relaxed), 2);
    assert_eq!(h.stats().large_bytes, 0);
    // The seed lists serve again: allocating the same again carves nothing new.
    let carved = h.stats().seed.carved;
    let v: Vec<_> = (0..2000).map(|i| (h.alloc(lay(1 + i % 700, 8)), lay(1 + i % 700, 8))).collect();
    assert_eq!(h.stats().seed.carved, carved);
    for (p, l) in v {
        free_block(h, p, l);
    }
    // Exhaust the seed with the smallest class (its slabs split any block): null, counted, nothing broken.
    let mut all = Vec::new();
    loop {
        let p = h.alloc(lay(8, 8));
        if p.is_null() {
            break;
        }
        all.push(p);
    }
    assert!(all.len() > 400_000, "only {} objects from 4 MiB", all.len());
    assert!(h.reports().heap_alloc_fails.load(Relaxed) >= 1 && h.reports().heap_reclaims.load(Relaxed) >= 1);
    h.check().unwrap();
    for p in all {
        free_block(h, p, lay(8, 8));
    }
    h.check().unwrap();
}

#[test]
fn seed_first_then_buddy() {
    let b = host_buddy(16);
    let seed = host_memory(256 << 10, 1 << 20);
    let h = heap(Some(b), Some((seed, 256 << 10)), 1);
    let small: Vec<_> = (0..4000).map(|_| h.alloc(lay(256, 8))).collect();
    assert!(small.iter().any(|&p| (p as usize) < seed || (p as usize) >= seed + (256 << 10)), "buddy after the seed");
    assert!(small.iter().any(|&p| (seed..seed + (256 << 10)).contains(&(p as usize))), "seed first");
    let big = h.alloc(lay(10_000, 8));
    assert!(!((seed..seed + (256 << 10)).contains(&(big as usize))), "large blocks from the buddy");
    h.check().unwrap();
    for p in small {
        free_block(h, p, lay(256, 8));
    }
    free_block(h, big, lay(10_000, 8));
    h.shrink();
    assert_eq!(heap_frames(b), 0);
    h.check().unwrap();
}

#[test]
fn exhaustion_reclaims_other_cpus_magazines() {
    let b = host_buddy(4);
    let h = heap(Some(b), None, 4);
    // CPU 1 allocates and frees: its magazine and an empty slab keep memory.
    set_cpu(1);
    let l = lay(2048, 8);
    let v: Vec<_> = (0..200).map(|_| h.alloc(l)).collect();
    for &p in &v {
        free_block(h, p, l);
    }
    assert!(h.stats().magazine_objects[10] > 0);
    // CPU 0 takes everything: the last slabs only come free through the reclaim.
    set_cpu(0);
    let mut all = Vec::new();
    loop {
        let p = h.alloc(l);
        if p.is_null() {
            break;
        }
        all.push(p);
    }
    assert!(h.reports().heap_reclaims.load(Relaxed) >= 1);
    assert_eq!(h.stats().magazine_objects[10], 0, "CPU 1's magazine was emptied");
    assert!(b.stats().free_bytes < 32 << 10, "{} bytes left unused", b.stats().free_bytes);
    assert_eq!(h.reports().heap_alloc_fails.load(Relaxed), 1);
    for p in all {
        free_block(h, p, l);
    }
    h.check().unwrap();
}

#[test]
fn realloc_in_place_and_moving() {
    let b = host_buddy(16);
    let h = heap(Some(b), None, 1);
    let l = lay(20, 8);
    let p = h.alloc(l);
    fill(p, 20, 9);
    // SAFETY: p is ours with layout l; every realloc result is used with its new size.
    let q = unsafe { h.realloc(p, l, 30) };
    assert_eq!(p, q, "same class (32): in place");
    let r = unsafe { h.realloc(q, lay(30, 8), 3000) };
    assert_ne!(r, q);
    assert!(holds(r, 20, 9));
    let s = unsafe { h.realloc(r, lay(3000, 8), 4000) };
    assert_eq!(s, r, "same page run: in place");
    assert_eq!(h.allocated_bytes(), 4000);
    free_block(h, s, lay(4000, 8));
    assert_eq!(h.allocated_bytes(), 0);
    h.check().unwrap();
}

#[test]
fn threads_on_many_cpus_with_cross_frees_and_migration() {
    let b = host_buddy(128);
    let h = heap(Some(b), None, 8);
    let shared: Arc<Mutex<Vec<(usize, usize, u8)>>> = Arc::new(Mutex::new(Vec::new()));
    let threads: Vec<_> = (0..8)
        .map(|t| {
            let shared = shared.clone();
            std::thread::spawn(move || {
                let mut rng = Rng(0x9E37_79B9_7F4A_7C15 ^ (t as u64 + 1));
                set_cpu(t);
                let mut mine: Vec<(usize, usize, u8)> = Vec::new();
                for step in 0..30_000 {
                    if rng.below(50) == 0 {
                        set_cpu(rng.below(9) as usize); // 8 = a CPU without magazines
                    }
                    match rng.below(10) {
                        0..=3 => {
                            let size = match rng.below(8) {
                                0 => 2049 + rng.below(20_000) as usize,
                                _ => 1 + rng.below(2048) as usize,
                            };
                            let p = h.alloc(lay(size, 8));
                            assert!(!p.is_null());
                            let tag = step as u8;
                            fill(p, size.min(64), tag);
                            mine.push((p as usize, size, tag));
                        }
                        4..=6 if !mine.is_empty() => {
                            let i = rng.below(mine.len() as u64) as usize;
                            let (p, size, tag) = mine.swap_remove(i);
                            assert!(holds(p as *mut u8, size.min(64), tag), "block changed under its owner");
                            free_block(h, p as *mut u8, lay(size, 8));
                        }
                        7 if !mine.is_empty() => shared.lock().unwrap().push(mine.pop().unwrap()),
                        8 => {
                            let got = shared.lock().unwrap().pop();
                            if let Some((p, size, tag)) = got {
                                assert!(holds(p as *mut u8, size.min(64), tag));
                                free_block(h, p as *mut u8, lay(size, 8));
                            }
                        }
                        _ => {}
                    }
                }
                for (p, size, _) in mine {
                    free_block(h, p as *mut u8, lay(size, 8));
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    for (p, size, _) in shared.lock().unwrap().drain(..) {
        free_block(h, p as *mut u8, lay(size, 8));
    }
    h.check().unwrap();
    assert_eq!(h.allocated_bytes(), 0);
    assert_eq!(h.reports().heap_bad_frees.load(Relaxed), 0);
    assert_eq!(h.reports().heap_corrupt.load(Relaxed), 0);
    h.shrink();
    assert_eq!(heap_frames(b), 0);
    b.check().unwrap();
}

#[test]
fn random_run_against_a_model() {
    let b = host_buddy(64);
    let h = heap(Some(b), None, 3);
    let mut rng = Rng(0x0DDB_A11C_AFE0_1234);
    let mut live: BTreeMap<usize, (Layout, u8)> = BTreeMap::new();
    let mut requested = 0u64;
    for step in 0..60_000u32 {
        set_cpu(rng.below(3) as usize);
        if live.is_empty() || rng.below(100) < 55 {
            let size = match rng.below(10) {
                0..=6 => 1 + rng.below(256) as usize,
                7 | 8 => 1 + rng.below(2048) as usize,
                _ => 1 + rng.below(70_000) as usize,
            };
            let align = 1 << rng.below(13);
            let l = lay(size, align);
            let p = h.alloc(l) as usize;
            assert!(p != 0 && p.is_multiple_of(align));
            if let Some((&q, &(ql, _))) = live.range(..p + size).next_back() {
                assert!(q + ql.size() <= p, "step {step}: {p:#x} overlaps live {q:#x}");
            }
            let tag = rng.next() as u8;
            fill(p as *mut u8, size, tag);
            live.insert(p, (l, tag));
            requested += size as u64;
        } else {
            let k = *live.keys().nth(rng.below(live.len() as u64) as usize).unwrap();
            let (l, tag) = live.remove(&k).unwrap();
            assert!(holds(k as *mut u8, l.size(), tag), "step {step}: block {k:#x} changed");
            free_block(h, k as *mut u8, l);
            requested -= l.size() as u64;
        }
        if step % 5000 == 0 {
            h.check().unwrap();
            let s = h.stats();
            assert_eq!(s.requested_bytes, requested);
            let small: u64 = live.values().filter(|(l, _)| crate::slab::geo::class_of(l.size(), l.align()).is_some()).count() as u64;
            assert_eq!(s.classes.iter().map(|c| c.live).sum::<u64>(), small);
            assert_eq!(s.held_by_kind[Kind::Heap as usize], heap_frames(b));
        }
    }
    for (p, (l, _)) in live {
        free_block(h, p as *mut u8, l);
    }
    h.shrink();
    h.check().unwrap();
    assert_eq!(heap_frames(b), 0);
    assert_eq!(h.stats().classes.iter().map(|c| c.slabs).sum::<u64>(), 0);
    assert_eq!(NCLASSES, h.stats().classes.len());
}

#[test]
fn without_magazines() {
    let b = host_buddy(16);
    let h = heap(Some(b), None, 0);
    let v: Vec<_> = (0..5000).map(|i| (h.alloc(lay(1 + i % 2048, 8)), lay(1 + i % 2048, 8))).collect();
    h.check().unwrap();
    for (p, l) in v {
        free_block(h, p, l);
    }
    assert_eq!(h.stats().magazine_cpus, 0);
    assert!(heap_frames(b) <= (NCLASSES as u64) * 32 * 1024, "at most one empty slab per class kept");
    h.shrink();
    assert_eq!(heap_frames(b), 0);
}

#[test]
fn buddy_exact_runs_merge_back() {
    let b = staged(&[free(16 * MIB, 144 * MIB)]);
    let initial = b.zone_info(0).free_blocks;
    let mut runs = Vec::new();
    for pages in 1..=150u64 {
        let align = 1 << (pages % 5);
        let p = b.alloc_exact(pages, align, Zone::Dma32, Kind::Heap).unwrap();
        assert_eq!(p % (align * PAGE), 0);
        assert!(b.run_allocated(p, pages, Kind::Heap));
        assert!(!b.run_allocated(p, pages + 1, Kind::Heap) && !b.run_allocated(p, pages, Kind::User));
        runs.push((p, pages));
    }
    let blocks: Vec<(u64, u32)> = runs.iter().flat_map(|&(p, n)| crate::buddy::pieces(p, n)).collect();
    assert_disjoint_inside(&blocks, &[(16 * MIB, 144 * MIB)]);
    assert_eq!(b.stats().used_by_kind[Kind::Heap as usize], (1..=150u64).sum::<u64>() * PAGE);
    b.check().unwrap();
    for (p, n) in runs {
        b.free_exact(p, n, Kind::Heap);
        assert!(!b.run_allocated(p, n, Kind::Heap));
    }
    b.drain_all();
    assert_eq!(b.zone_info(0).free_blocks, initial, "everything merged back");
    assert_eq!(b.reports().bad_frees.load(Relaxed), 0);
}
