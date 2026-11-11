//! The kernel heap's scattered large blocks (`mm_core::Scatter`; Linux's vmalloc behind kvmalloc, mm/vmalloc.c):
//! a vmap reservation with its guard page below and one `Kind::Heap` frame per page, mapped 4 KiB WB, writable,
//! no-execute, global. The heap calls in only where no spin lock is held, so the small heap blocks the vmap
//! bookkeeping takes under its spin locks never come back here; nothing here calls klog or the scheduler on the
//! heap's behalf (events are the `mm.report` thread's, from the heap's counters).
//!
//! The heap keeps frees it may not do where they happen (a spin lock held, interrupts off) on its own list;
//! `kick` wakes `mm.report` (`report`), which drains it.

use core::sync::atomic::{AtomicBool, Ordering};
use hal::paging::{self, PageFlags, PageSize};
use ksync::WaitQueue;
use mm_core::{Kind, Scatter, Zone, PAGE};

struct Blocks;

static BLOCKS: Blocks = Blocks;
static WQ: WaitQueue = WaitQueue::new();
static KICKED: AtomicBool = AtomicBool::new(false);

klog::counter!(SCATTER_KEPT, "mm.heap.scatter_kept");

impl Scatter for Blocks {
    fn span(&self) -> (usize, usize) {
        (bootinfo::layout::VMAP_BASE as usize, bootinfo::layout::VMAP_END as usize)
    }
    fn alloc(&self, pages: u64) -> Option<usize> {
        alloc(pages).map(|v| v as usize)
    }
    fn free(&self, v: usize, pages: u64) -> bool {
        pages.checked_mul(PAGE).is_some_and(|len| release(v as u64, len))
    }
    fn looks_live(&self, v: usize, pages: u64) -> bool {
        looks_live(v as u64, pages)
    }
    fn kick(&self) {
        if !KICKED.swap(true, Ordering::AcqRel) {
            WQ.wake_one();
        }
    }
}

fn install() -> Result<(), &'static str> {
    if !mm_core::HEAP.set_scatter(&BLOCKS) {
        return Err("the heap's scattered blocks were installed already");
    }
    Ok(())
}
kreg::initcall!(kreg::Level::Core, "mm.scatter", install);

fn alloc(pages: u64) -> Option<u64> {
    let len = pages.checked_mul(PAGE)?;
    let (root, f) = (paging::kernel_root()?, mm_core::frames()?);
    // vmalloc refuses more than RAM up front; this refuses what the free frames above the min watermark cannot
    // give, so a hopeless request costs neither the reserve (others' spin-lock and interrupt allocations) nor a
    // rollback with its shootdown. The heap's reclaim rounds then free frames and ask again.
    let st = f.stats();
    let room = (st.usable_free_bytes() / PAGE).saturating_sub(mm_core::reclaim::watermarks(st.total_bytes / PAGE).min);
    if pages > room {
        return None;
    }
    let body = crate::vmap::reserve_quiet(len).ok()?;
    let flags = PageFlags(PageFlags::kernel_data().0 | PageFlags::GLOBAL);
    for i in 0..pages {
        let Some(p) = f.alloc(0, Zone::Normal, Kind::Heap) else {
            release(body, len);
            return None;
        };
        if paging::map_kernel(root, body + i * PAGE, p, PageSize::Size4K, flags).is_err() {
            f.free(p, 0, Kind::Heap);
            release(body, len);
            return None;
        }
    }
    Some(body)
}

/// Unmaps a block (or what of it got mapped), frees its frames once every CPU dropped them (`unmap_kernel_pages`;
/// unconfirmed ones stay out of use, counted in `mm.heap.scatter_kept`) and releases the range. False: not a live
/// reservation of `len`.
fn release(body: u64, len: u64) -> bool {
    let (Some(root), Some(f)) = (paging::kernel_root(), mm_core::frames()) else { return false };
    crate::vmap::release_quiet(body, len, &mut |b, l| {
        let mut freed = 0;
        let mapped = paging::unmap_kernel_pages(root, b, l, &mut |p| {
            f.free(p, 0, Kind::Heap);
            freed += 1;
        });
        SCATTER_KEPT.add(mapped - freed);
    })
    .is_some()
}

/// The guard below `v` is unmapped and its first and last pages are writable heap frames (lock-free page-table
/// reads: the caller may hold any lock or run with interrupts off).
fn looks_live(v: u64, pages: u64) -> bool {
    let (Some(root), Some(b)) = (paging::kernel_root(), mm_core::boot_buddy()) else { return false };
    let Some(last) = pages.checked_sub(1).and_then(|n| n.checked_mul(PAGE)).and_then(|o| v.checked_add(o)) else { return false };
    let heap = |va: u64| {
        paging::translate(root, va).is_some_and(|(p, fl)| fl.contains(PageFlags::WRITABLE) && b.run_allocated(p, 1, Kind::Heap))
    };
    v >= PAGE && paging::translate(root, v - PAGE).is_none() && heap(v) && heap(last)
}

/// Waits up to `ns` for a deferred heap free (`kick`); for `mm.report`.
pub(crate) fn wait_kick(ns: u64) {
    WQ.wait_until_timeout(|| KICKED.load(Ordering::Acquire), ns);
    KICKED.store(false, Ordering::Release);
}
