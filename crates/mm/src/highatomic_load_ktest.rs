//! ktest `mm.frames.highatomic.load`: the HighAtomic reserve under the load of the Mini-PC's crash of 12.11.2026
//! (generation 130: the page cache had filled memory, the cache and the heap behind it grew fast, a program
//! started, and a 5-page heap block asked for under a spin lock found nothing, 25 s after the first takes).
//!
//! Memory is filled with two files' clean pages, frame by frame in turn, and one file's pages are taken back, so
//! what is free lies in single frames between clean cache (as measured on the Mini-PC: pieces of order 0..2) and
//! every block of two frames or more has to be made (compaction) or comes out of the reserve. Then:
//! 1. `RUNS` short-lived 5-page blocks under a spin lock out of the reserve, one after the other (the walk of
//!    /sys/bus/pci that cloned 17296 bytes under `pci.devices` 60 times a second): each is an order-3 block trimmed
//!    to 4 + 1 frames; the reserve must have every frame back afterwards (desk 12.11.: does a take cost an order-3
//!    block for good?).
//!
//! The reserve is the allocator's last step: a block of eight frames that anything frees into ordinary memory
//! meanwhile is taken first, and since a freed short-lived block is such a block, one of them then serves every
//! later request (12.11., in a series after the fs. tests: another thread's allocation made the heap give its
//! empty slabs back 4 ms into step 1, and 2367 of 3000 requests were served by the one block that made; elsewhere
//! kcompactd's proactive pass did the same). That is the allocator as it should be, and not what steps 1 and 2
//! ask about. So a block that did not come out of the reserve is held to the end and not counted (`FOREIGN` of them
//! at most), the steps go on until their number of blocks came out of the reserve, and kcompactd's proactive pass,
//! which would repair in the background what the set-up broke on purpose, is off until step 3.
//!
//! 2. `SETTLED` 5-page blocks out of the reserve under a spin lock that stay (long-lived takers: what a slab page or a driver's buffer
//!    taken that way does). They leave the reserve below its low mark; within `WHOLE_MS` it must be whole again
//!    with the blocks still held: free pageblocks joined (made by compaction, nothing else is free), the
//!    pageblocks the blocks settled in were handed over.
//! 3. `LOAD_MS` of load: a reader fills the cache with a third file (reclaim keeps up), a thread that may wait
//!    builds kernel objects whose slabs are blocks of 2, 4 and 8 frames and holds them (the heap growing with the
//!    cache), another takes and drops GPU-like buffers (`DmaPages`), and the test takes 5-page blocks under a spin
//!    lock: most dropped at once, some held to the end. No block under the spin lock may fail, and no object of
//!    the thread that may wait.
//! 4. Within `WHOLE_MS` after the load no zone's reserve is low, it has not grown past its size, and a 5-page
//!    block under a spin lock succeeds at the end.
//!
//! QemuOnly: it fills the machine's memory (the 2 GiB gate boot; on a bigger machine it fails).

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use ktest::{kassert, ktest, TestResult};
use mm_core::{Kind, Zone, PAGEBLOCK_FRAMES};
use vfs::pagecache::{Backend, CachePage, PageCache};
use vfs::reclaim::{Mode, Reclaimer};

const P: u64 = 4096;
const MIB: u64 = 1 << 20;
/// The block of the crash: 17296 bytes, five pages.
const BLOCK: usize = 17296;
const RUNS: u64 = 3000;
/// Blocks of steps 1 and 2 that came out of ordinary memory, at most (each five frames, held to the end).
const FOREIGN: usize = 512;
/// Long-lived blocks of step 2: five frames each, more than a quarter of the gate boot's reserve (four pageblocks).
const SETTLED: usize = 120;
const LOAD_MS: u64 = 4000;
/// Blocks of step 3 held to the end.
const KEPT: usize = 100;
const WHOLE_MS: u64 = 10_000;

/// A file of zero pages: a read brings one page in, nothing more.
struct Zeros;

impl Backend for Zeros {
    fn read_page(&self, _pgoff: u64, _page: &Arc<CachePage>) -> Result<(), i64> {
        Ok(())
    }
    fn write_run(&self, _first: u64, _pages: &[Arc<CachePage>], _size: u64) -> Result<(), i64> {
        Ok(())
    }
}

fn file(pages: u64) -> PageCache {
    PageCache::with_backend(Arc::new(Zeros), pages * P)
}

fn touch(c: &PageCache, page: u64) -> Result<(), String> {
    let mut b = [0u8; 8];
    c.read_at(page * P, &mut b).map(|_| ()).map_err(|e| format!("read page {page}: {e}"))
}

fn usable_free() -> u64 {
    mm_core::frames().map_or(0, |f| f.stats().usable_free_bytes() / P)
}

/// A 5-page heap block asked for under a spin lock, as the crash's caller did (fallibly here: a failure is a red
/// test, not a panic).
fn atomic_block(lock: &ksync::SpinLock<()>) -> Option<Vec<u8>> {
    let _held = lock.lock();
    let mut v: Vec<u8> = Vec::new();
    v.try_reserve_exact(BLOCK).ok()?;
    Some(v)
}

/// Whether the heap block lies in a pageblock of the reserve.
fn in_reserve(b: &mm_core::Buddy, v: &[u8]) -> bool {
    bootinfo::layout::virt_to_phys(v.as_ptr() as u64).is_some_and(|p| b.pageblock_type(p) == Some(mm_core::MigrateType::HighAtomic))
}

/// 5-page blocks under the spin lock until `want` of them came out of the reserve, each of those given to `out`
/// (dropped there, or kept). The others go to `foreign` and stay: left free they would serve the next request.
fn reserve_blocks(b: &mm_core::Buddy, lock: &ksync::SpinLock<()>, want: u64, foreign: &mut Vec<Vec<u8>>, out: &mut dyn FnMut(Vec<u8>)) -> TestResult {
    let mut got = 0;
    while got < want {
        let v = atomic_block(lock).ok_or_else(|| format!("block {got} of {want} under the spin lock failed: reserve {:?}", b.reserve_stats()))?;
        if in_reserve(b, &v) {
            got += 1;
            out(v);
        } else {
            kassert!(foreign.len() < FOREIGN, "{FOREIGN} blocks of eight frames came free in ordinary memory before {want} came out of the reserve ({got} did): the set-up does not hold");
            foreign.push(v);
        }
    }
    Ok(())
}

/// Frames taken straight from the allocator, given back when this drops (also when a check fails).
struct Frames(Vec<(u64, u32)>);

impl Drop for Frames {
    fn drop(&mut self) {
        if let Some(f) = mm_core::frames() {
            for (p, order) in self.0.drain(..) {
                f.free(p, order, Kind::Other);
            }
        }
    }
}

/// kcompactd's proactive pass off until this drops.
struct NoProactive(u32);

impl NoProactive {
    fn new() -> Self {
        let was = mm_core::compact::proactiveness();
        mm_core::compact::set_proactiveness(0);
        NoProactive(was)
    }
}

impl Drop for NoProactive {
    fn drop(&mut self) {
        mm_core::compact::set_proactiveness(u64::from(self.0));
    }
}

/// The reserve's free frames and its free frames in blocks of 8 or more (what a 5-page block needs).
fn reserve_supply(b: &mm_core::Buddy) -> (u64, u64) {
    let ha = mm_core::MigrateType::HighAtomic as usize;
    (0..2).map(|z| b.zone_info(z)).fold((0, 0), |(free, big), i| {
        (free + i.free_highatomic, big + (3..mm_core::ORDERS).map(|o| i.free_by_type[ha][o] << o).sum::<u64>())
    })
}

/// What the threads of phase 2 count.
#[derive(Default)]
struct Load {
    stop: AtomicBool,
    cache_pages: AtomicU64,
    objects: AtomicU64,
    object_fails: AtomicU64,
    slowest_object_us: AtomicU64,
    gpu_sets: AtomicU64,
}

/// Kernel objects of the three size classes whose slabs are 2, 4 and 8 frames, held (a caller that may wait).
fn build_objects(l: &Load) -> Vec<Vec<u8>> {
    let mut held: Vec<Vec<u8>> = Vec::new();
    let sizes = [2048usize, 2048, 1024, 512];
    let mut i = 0usize;
    while !l.stop.load(Relaxed) && held.len() < 12_000 {
        let mut v: Vec<u8> = Vec::new();
        let t0 = time::now_ns();
        if v.try_reserve_exact(sizes[i % sizes.len()]).is_err() || held.try_reserve(1).is_err() {
            l.object_fails.fetch_add(1, Relaxed);
        } else {
            held.push(v);
            l.objects.fetch_add(1, Relaxed);
        }
        l.slowest_object_us.fetch_max((time::now_ns() - t0) / 1000, Relaxed);
        i += 1;
        if i.is_multiple_of(32) {
            sched::yield_now();
        }
    }
    held
}

fn load() -> TestResult {
    let frames = mm_core::frames().ok_or("no frame allocator")?;
    let b = mm_core::boot_buddy().ok_or("no boot allocator")?;
    let total = frames.stats().total_bytes;
    if total > 5 * (512 * MIB) {
        return ktest::fail("a test for small RAM (the 2 GiB gate boot): it fills the machine's memory");
    }
    let s0 = b.reserve_stats();
    let target: u64 = s0.target.iter().sum();
    kassert!(target > 0 && s0.pageblocks == s0.target, "no whole reserve to start with: {s0:?}");
    kassert!(mm_core::reclaim::context_allows(), "the test's context may not reclaim");
    let lock = ksync::SpinLock::named("mm.ktest.highatomic_load", ());
    let w = crate::reclaim::watermarks();

    // Lists first, while memory is whole.
    let mut kept: Vec<Vec<u8>> = Vec::new();
    kept.try_reserve_exact(SETTLED + KEPT).map_err(|_| "no room for the list")?;
    let mut foreign: Vec<Vec<u8>> = Vec::new();
    foreign.try_reserve_exact(FOREIGN).map_err(|_| "no room for the list")?;
    let mut blocks = Frames(Vec::new());
    blocks.0.try_reserve_exact(4096).map_err(|_| "no room for the list")?;
    let quiet = NoProactive::new();

    // Clean cache over all memory down to the low watermark, the two files' pages frame by frame in turn.
    let each = usable_free() / 2;
    let (f, g) = (file(each), file(each));
    let mut pages = 0;
    while pages < each && usable_free() > w.low + 256 {
        touch(&f, pages)?;
        touch(&g, pages)?;
        pages += 1;
    }
    // One file's pages go: free single frames between the other's (above the high watermark: kswapd rests).
    let hole = (w.high + 4 * MIB / P).min(g.pages());
    let o = Reclaimer::of(&[&g]).shrink(hole, Mode::Background, true);
    mm_core::grace::expedite();
    b.drain_all();
    kassert!(o.freed * 2 >= hole, "the file's pages did not go: {o:?}");
    // Whatever is free in blocks of two frames or more is held for the test's time: only single frames stay.
    for order in (1..=mm_core::MAX_ORDER).rev() {
        while blocks.0.len() < blocks.0.capacity() {
            let Some(p) = frames.alloc_light(order, Zone::Normal, Kind::Other) else { break };
            blocks.0.push((p, order));
        }
    }
    let free_blocks = b.zone_info(0).free_blocks;
    klog::event!(Info, "mm", "ktest.highatomic_fill", file_pages = 2 * pages, held_blocks = blocks.0.len(), holes = o.freed, usable_free = usable_free(),
        o0 = free_blocks[0], o1 = free_blocks[1], o2 = free_blocks[2], o3 = free_blocks[3], o4_up = free_blocks[4..].iter().sum::<u64>());

    // 1. Short-lived blocks out of the reserve, one after the other.
    let (free1, big1) = reserve_supply(b);
    let s1 = b.reserve_stats();
    reserve_blocks(b, &lock, RUNS, &mut foreign, &mut drop)?;
    let s2 = b.reserve_stats();
    let (free2, big2) = reserve_supply(b);
    // Frames others took meanwhile (blocks of other orders, and of this order beyond the test's own) may be
    // missing; nothing else.
    let own3 = s2.takes[3] - s1.takes[3];
    let others: u64 = (0..9).filter(|&o| o != 3).map(|o| (s2.takes[o] - s1.takes[o]) << o).sum::<u64>() + (own3.saturating_sub(RUNS) << 3);
    klog::event!(Info, "mm", "ktest.highatomic_runs", runs = RUNS, from_reserve = own3, foreign = foreign.len(), free_before = free1, free_after = free2,
        in_blocks_before = big1, in_blocks_after = big2, other_takes_pages = others, refilled = s2.refilled - s1.refilled);
    kassert!(own3 >= RUNS, "the reserve counted {own3} takes of eight frames for {RUNS} blocks that lay in it");
    kassert!(free2 + others >= free1 || s2.refilled > s1.refilled, "{RUNS} short-lived blocks cost the reserve frames: {free1} -> {free2} free ({others} taken by others)");
    kassert!(big2 + 8 * others >= big1 || s2.refilled > s1.refilled, "{RUNS} short-lived blocks cost the reserve its blocks of 8 frames: {big1} -> {big2} frames in such blocks");

    // 2. Blocks that stay, then the refill.
    reserve_blocks(b, &lock, SETTLED as u64, &mut foreign, &mut |v| kept.push(v))?;
    let low = b.reserve_low();
    let s3 = b.reserve_stats();
    kassert!(low || s3.refilled > s2.refilled, "{SETTLED} long-lived blocks ({} of them out of the reserve) did not bring it below its low mark: {s3:?}", s3.takes[3] - s2.takes[3]);
    let whole = |s: &mm_core::buddy::ReserveStats| (0..2).all(|z| s.pageblocks[z] == s.target[z] && s.free[z] * 16 >= s.target[z] * PAGEBLOCK_FRAMES * 15);
    let t0 = time::now_ns();
    let mut s4 = b.reserve_stats();
    let after_settling = s3.free.iter().sum::<u64>();
    while !(whole(&s4) && s4.refilled > s2.refilled) && time::now_ns() - t0 < WHOLE_MS * 1_000_000 {
        sched::sleep_ms(20);
        s4 = b.reserve_stats();
    }
    klog::event!(Info, "mm", "ktest.highatomic_whole", after_ms = (time::now_ns() - t0) / 1_000_000, settled = SETTLED, from_reserve = s3.takes[3] - s2.takes[3], foreign = foreign.len(),
        free_low = after_settling, free_now = s4.free.iter().sum::<u64>(), reserve_frames = target * PAGEBLOCK_FRAMES, pageblocks = s4.pageblocks.iter().sum::<u64>(),
        refilled = s4.refilled - s2.refilled, handed_over = s4.handed - s2.handed);
    kassert!(whole(&s4), "the reserve is not whole {WHOLE_MS} ms after {SETTLED} blocks settled in it: {s4:?}");
    kassert!(s4.refilled > s2.refilled && s4.handed > s2.handed, "whole without a refill and a hand-over: {s4:?}");
    kassert!(kept.len() == SETTLED, "the blocks are still held");
    // The load runs on the machine as it is.
    drop(quiet);

    // 3. The load.
    let l = Arc::new(Load::default());
    let h = file(each);
    let reader = {
        let l = l.clone();
        sched::spawn("mm-ktest-ha-cache", sched::Class::Normal, move || {
            let mut i = 0;
            while !l.stop.load(Relaxed) && i < each {
                if touch(&h, i).is_ok() {
                    l.cache_pages.fetch_add(1, Relaxed);
                }
                i += 1;
            }
            drop(h);
        })
        .map_err(|e| format!("spawn: {e:?}"))?
    };
    let objects: Arc<ksync::Mutex<Vec<Vec<u8>>>> = Arc::new(ksync::Mutex::new(Vec::new()));
    let builder = {
        let (l, objects) = (l.clone(), objects.clone());
        sched::spawn("mm-ktest-ha-objects", sched::Class::Normal, move || {
            let held = build_objects(&l);
            *objects.lock() = held;
        })
        .map_err(|e| format!("spawn: {e:?}"))?
    };
    let gpu = {
        let l = l.clone();
        sched::spawn("mm-ktest-ha-gpu", sched::Class::Normal, move || {
            let mut sets = alloc::collections::VecDeque::new();
            while !l.stop.load(Relaxed) {
                if let Ok(s) = crate::dma_pages::DmaPages::alloc(256, crate::dma_pages::DmaPagesOptions::default()) {
                    l.gpu_sets.fetch_add(1, Relaxed);
                    sets.push_back(s);
                }
                if sets.len() > 8 {
                    sets.pop_front();
                }
                sched::sleep_ms(5);
            }
        })
        .map_err(|e| format!("spawn: {e:?}"))?
    };
    let (mut asked, mut failed) = (0u64, 0u64);
    let end = time::now_ns() + LOAD_MS * 1_000_000;
    let mut lowest = u64::MAX;
    while time::now_ns() < end {
        for _ in 0..8 {
            asked += 1;
            match atomic_block(&lock) {
                Some(v) if kept.len() < SETTLED + KEPT && asked.is_multiple_of(16) => kept.push(v),
                Some(_) => {}
                None => failed += 1,
            }
        }
        lowest = lowest.min(reserve_supply(b).0);
        sched::sleep_ms(1);
    }
    l.stop.store(true, Relaxed);
    reader.join();
    builder.join();
    gpu.join();
    let s5 = b.reserve_stats();
    let heap = Kind::Heap as usize;
    klog::event!(Info, "mm", "ktest.highatomic_load", atomic_asked = asked, atomic_failed = failed, kept = kept.len(), reserve_lowest = lowest,
        reserve_free = s5.free.iter().sum::<u64>(), reserve_frames = target * PAGEBLOCK_FRAMES, takes = s5.total_takes() - s4.total_takes(),
        take_pages = s5.total_pages() - s4.total_pages(), heap_pages = s5.pages[heap] - s4.pages[heap], dma_pages = s5.pages[Kind::Dma as usize] - s4.pages[Kind::Dma as usize],
        by_waiters = s5.last_tries - s4.last_tries, o0 = s5.takes[0] - s4.takes[0], o1 = s5.takes[1] - s4.takes[1], o2 = s5.takes[2] - s4.takes[2],
        o3 = s5.takes[3] - s4.takes[3], refilled = s5.refilled - s4.refilled, handed_over = s5.handed - s4.handed, cache_pages = l.cache_pages.load(Relaxed),
        objects = l.objects.load(Relaxed), object_fails = l.object_fails.load(Relaxed), slowest_object_us = l.slowest_object_us.load(Relaxed),
        gpu_sets = l.gpu_sets.load(Relaxed));
    kassert!(failed == 0, "{failed} of {asked} 5-page blocks under a spin lock failed (reserve lowest {lowest} of {} frames, {} takes by callers that may wait)",
        target * PAGEBLOCK_FRAMES, s5.last_tries - s4.last_tries);
    kassert!(l.object_fails.load(Relaxed) == 0, "{} kernel objects of a caller that may wait failed", l.object_fails.load(Relaxed));

    // 4. Not low, not grown, and the block of the crash.
    let t0 = time::now_ns();
    while b.reserve_low() && time::now_ns() - t0 < WHOLE_MS * 1_000_000 {
        sched::sleep_ms(20);
    }
    let s6 = b.reserve_stats();
    klog::event!(Info, "mm", "ktest.highatomic_end", after_ms = (time::now_ns() - t0) / 1_000_000, reserve_free = s6.free.iter().sum::<u64>(),
        pageblocks = s6.pageblocks.iter().sum::<u64>(), refilled = s6.refilled - s0.refilled, handed_over = s6.handed - s0.handed, kept = kept.len());
    kassert!(!b.reserve_low(), "the reserve is still low {WHOLE_MS} ms after the load: {s6:?}");
    kassert!(s6.pageblocks.iter().zip(s6.target).all(|(p, t)| *p <= t), "the reserve grew past its size: {s6:?}");
    kassert!(atomic_block(&lock).is_some(), "the 5-page block under a spin lock failed at the end: {:?}", b.reserve_stats());

    drop((kept, foreign));
    drop(core::mem::take(&mut *objects.lock()));
    drop(blocks);
    drop((f, g));
    Ok(())
}
ktest!("mm.frames.highatomic.load", QemuOnly, load);
