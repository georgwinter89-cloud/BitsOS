//! kswapd's high-order side on the running kernel: with free memory in single frames only (the test takes the free
//! frames and gives every other one back, so no two free frames are buddies, and holds what blocks are left) and clean cache in memory, an
//! order the heap noted makes kswapd hand it to kcompactd, which compacts a block of that order within a few looks
//! (`Balancer::balance_order`, `crate::compact`; `KSWAPD_CLEARED` counts kcompactd's blocks for handed orders).
//! The free count stays high, so the count balancing leaves the cache alone.

use alloc::vec::Vec;
use ktest::{kassert, ktest, TestResult};
use mm_core::{FrameAlloc, Kind, Zone, PAGE};

const ORDER: u32 = 4;
/// The test's clean cache (64 MiB).
const CACHE_PAGES: u64 = 16384;

/// Takes every free frame down to the high watermark (the rest is for everyone else meanwhile; `take_blocks` then
/// takes its blocks), then gives back the odd ones: free memory stays, free blocks above a frame do not.
fn fragment(f: &dyn FrameAlloc, held: &mut Vec<u64>) {
    let keep = crate::reclaim::watermarks().high + 1024;
    while held.len() < held.capacity() {
        if held.len() % 512 == 0 && f.stats().usable_free_bytes() / PAGE < keep {
            break;
        }
        let Some(p) = f.alloc_light(0, Zone::Normal, Kind::Other) else { break };
        held.push(p);
    }
    held.retain(|&p| {
        let odd = (p / PAGE) % 2 == 1;
        if odd {
            f.free(p, 0, Kind::Other);
        }
        !odd
    });
}

/// Takes every free block of `ORDER` or more (one someone freed meanwhile: only a block kswapd cleared counts).
fn take_blocks(f: &dyn FrameAlloc, blocks: &mut Vec<(u64, u32)>) {
    for order in (ORDER..=mm_core::MAX_ORDER).rev() {
        while blocks.len() < blocks.capacity() {
            let Some(p) = f.alloc_light(order, Zone::Normal, Kind::Other) else { break };
            blocks.push((p, order));
        }
    }
}

fn order_balance() -> TestResult {
    let (Some(f), Some(b)) = (mm_core::frames(), mm_core::boot_buddy()) else { return ktest::fail("no frame allocator") };
    let (cache, _pat) = crate::reclaim_ktest::pattern_file(CACHE_PAGES);
    crate::reclaim_ktest::check_file(&cache, CACHE_PAGES)?;
    let before = census();
    let (mut held, mut blocks): (Vec<u64>, Vec<(u64, u32)>) = (Vec::new(), Vec::new());
    // Room for every frame of the machine: frames others free meanwhile are taken too.
    let want = (f.stats().total_bytes / PAGE) as usize;
    if held.try_reserve_exact(want).is_err() || blocks.try_reserve_exact(4096).is_err() {
        return ktest::fail("no room for the lists of held frames");
    }
    fragment(f, &mut held);
    let free = f.stats().usable_free_bytes() / PAGE;
    let cleared0 = mm_core::reclaim::KSWAPD_CLEARED.get();
    let t0 = ksync::now_ns().unwrap_or(0);
    let (mut seen, mut retaken) = (false, 0u64);
    while ksync::now_ns().unwrap_or(0).saturating_sub(t0) < 2_000_000_000 && blocks.len() < blocks.capacity() {
        if b.has_free_block(ORDER) {
            if mm_core::reclaim::KSWAPD_CLEARED.get() > cleared0 {
                seen = true;
                break;
            }
            take_blocks(f, &mut blocks);
            retaken += 1;
            continue;
        }
        mm_core::HEAP.order_want().note(ORDER);
        sched::sleep_ms(10);
    }
    let cleared = mm_core::reclaim::KSWAPD_CLEARED.get() - cleared0;
    let after = census();
    let took_ms = ksync::now_ns().unwrap_or(0).saturating_sub(t0) / 1_000_000;
    let (frames, full) = (held.len(), held.len() == held.capacity() || blocks.len() == blocks.capacity());
    for p in held {
        f.free(p, 0, Kind::Other);
    }
    for (p, o) in blocks {
        f.free(p, o, Kind::Other);
    }
    klog::event!(Info, "mm", "ktest.order_balance", order = ORDER, held_frames = frames, free_frames = free, retaken = retaken, cleared = cleared,
        took_ms = took_ms, cached_pages = cache.pages(), before = before.as_str(), after = after.as_str());
    kassert!(!full, "a list of held frames ran full");
    kassert!(seen, "no order-{ORDER} block came back in 2 s ({cleared} cleared by kswapd); census with the cache: {before}; at the end: {after}");
    Ok(())
}

fn census() -> alloc::string::String {
    mm_core::block_census(ORDER).map_or(alloc::string::String::from("no boot allocator"), |c| alloc::format!("{c}"))
}
ktest!("mm.reclaim.order_balance", QemuOnly, order_balance);
