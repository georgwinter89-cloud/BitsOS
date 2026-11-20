//! The kernel heap's scattered large blocks (`mm_core::Scatter`; Linux's vmalloc behind kvmalloc, mm/vmalloc.c):
//! a vmap reservation with its guard page below and one `Kind::Heap` frame per page, mapped 4 KiB WB, writable,
//! no-execute, global. The heap calls in only where no spin lock is held, so the small heap blocks the vmap
//! bookkeeping takes under its spin locks never come back here; nothing here calls klog or the scheduler on the
//! heap's behalf (events are the `mm.report` thread's, from the heap's counters).
//!
//! The heap keeps frees it may not do where they happen (a spin lock held, interrupts off) on its own list;
//! `kick` wakes `mm.report` (`report`), which drains it.
//!
//! A freed block is kept as it is (`Kept`) for the next block of the same size: making one costs a frame
//! allocation per page and giving one back two TLB shootdowns that every CPU answers, and buffers of one size
//! come and go all the time (a socket's 68 KiB receive buffer). Mini-PC 12.11.2026, generation 137: 1800 such
//! blocks a second beside a full page cache, 54000 shootdown interrupts a second on sixteen CPUs, and each block's
//! frames taken one by one out of the page cache's pageblocks. Linux keeps what vfree unmapped until 32 MiB times
//! log2(CPUs) have gathered and flushes once (`lazy_max_pages`, mm/vmalloc.c); here the block stays whole. The
//! kept blocks are bounded (`keep_limit`), go when memory is short (`trim`, from kswapd's look and before a
//! block is refused) and when nobody took them for two periods of `mm.report` (`age`).

use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use hal::paging::{self, PageFlags, PageSize};
use ksync::{SpinLock, WaitQueue};
use mm_core::{Kind, Scatter, Zone, PAGE};

struct Blocks;

static BLOCKS: Blocks = Blocks;
static WQ: WaitQueue = WaitQueue::new();
static KICKED: AtomicBool = AtomicBool::new(false);

klog::counter!(SCATTER_KEPT, "mm.heap.scatter_kept");
// Blocks taken from the kept ones instead of being made, and kept blocks given back (bound, memory short, age).
klog::counter!(SCATTER_REUSED, "mm.heap.scatter_reused");
klog::counter!(SCATTER_TRIMMED, "mm.heap.scatter_trimmed");

/// Kept blocks at most: the list is made once (`install`), a free never allocates.
const KEEP_BLOCKS: usize = 256;
/// The largest block kept (1 MiB): the buffers that come and go are small, and a kept block's frames lie in as
/// many pageblocks as it has pages.
const KEEP_BLOCK_PAGES: u64 = 256;

/// One freed block kept whole: its first address, its pages and `Kept::now` when it was freed.
#[derive(Clone, Copy)]
struct KeptBlock {
    body: u64,
    pages: u64,
    since: u64,
}

struct Kept {
    blocks: Vec<KeptBlock>,
    pages: u64,
    /// Counts `age` calls.
    now: u64,
}

static KEPT: SpinLock<Kept> = SpinLock::named("mm.scatter.kept", Kept { blocks: Vec::new(), pages: 0, now: 0 });
/// `Kept::pages` for the heap's statistics (read from any context).
static KEPT_PAGES: AtomicU64 = AtomicU64::new(0);

/// Pages kept at most: 1/512 of memory, at least 4 MiB, at most 64 MiB (24 GiB: 48 MiB).
fn keep_limit() -> u64 {
    mm_core::frames().map_or(0, |f| f.stats().total_bytes / PAGE / 512).clamp(1024, 16384)
}

/// A kept block of exactly `pages`, the one freed last (its frames are the warmest).
fn take_kept(pages: u64) -> Option<u64> {
    let mut k = KEPT.lock();
    let i = k.blocks.iter().rposition(|b| b.pages == pages)?;
    let b = k.blocks.remove(i);
    k.pages -= pages;
    KEPT_PAGES.store(k.pages, Ordering::Relaxed);
    Some(b.body)
}

/// Keeps the block; false: no room (the caller gives it back) or it is kept already (a double free).
fn keep(body: u64, pages: u64) -> Result<bool, ()> {
    let limit = keep_limit();
    let mut k = KEPT.lock();
    if k.blocks.iter().any(|b| b.body == body) {
        return Err(());
    }
    if pages > KEEP_BLOCK_PAGES || k.blocks.len() == k.blocks.capacity() || k.pages + pages > limit {
        return Ok(false);
    }
    let since = k.now;
    k.blocks.push(KeptBlock { body, pages, since });
    k.pages += pages;
    KEPT_PAGES.store(k.pages, Ordering::Relaxed);
    Ok(true)
}

/// Gives kept blocks back: all of them (memory is short), or those nobody took for two calls of `age`. Thread
/// context where a shootdown may be waited for; the list's lock is not held while a block goes.
fn trim_where(all: bool) -> u64 {
    let mut n = 0;
    loop {
        let b = {
            let mut k = KEPT.lock();
            let now = k.now;
            let Some(i) = k.blocks.iter().position(|b| all || b.since + 2 <= now) else { return n };
            let b = k.blocks.remove(i);
            k.pages -= b.pages;
            KEPT_PAGES.store(k.pages, Ordering::Relaxed);
            b
        };
        release(b.body, b.pages * PAGE);
        SCATTER_TRIMMED.inc();
        n += 1;
    }
}

/// Memory is short: every kept block goes back (kswapd's look below the low watermark).
pub(crate) fn trim() -> u64 {
    if KEPT.lock().pages == 0 {
        return 0;
    }
    trim_where(true)
}

/// `mm.report`'s period: blocks kept through two of them go back.
pub(crate) fn age() -> u64 {
    {
        let mut k = KEPT.lock();
        if k.blocks.is_empty() {
            return 0;
        }
        k.now += 1;
    }
    trim_where(false)
}

impl Scatter for Blocks {
    fn span(&self) -> (usize, usize) {
        (bootinfo::layout::VMAP_BASE as usize, bootinfo::layout::VMAP_END as usize)
    }
    fn alloc(&self, pages: u64) -> Option<usize> {
        if let Some(v) = take_kept(pages) {
            SCATTER_REUSED.inc();
            return Some(v as usize);
        }
        // What is kept is memory too: before a block is refused it goes back and the block is tried once more.
        alloc(pages).or_else(|| if trim() > 0 { alloc(pages) } else { None }).map(|v| v as usize)
    }
    fn free(&self, v: usize, pages: u64) -> bool {
        let Some(len) = pages.checked_mul(PAGE) else { return false };
        if !crate::vmap::reserved(v as u64, len) {
            return false;
        }
        match keep(v as u64, pages) {
            Ok(true) => true,
            Ok(false) => release(v as u64, len),
            Err(()) => false,
        }
    }
    fn looks_live(&self, v: usize, pages: u64) -> bool {
        looks_live(v as u64, pages)
    }
    fn kick(&self) {
        if !KICKED.swap(true, Ordering::AcqRel) {
            WQ.wake_one();
        }
    }
    fn kept_pages(&self) -> u64 {
        KEPT_PAGES.load(Ordering::Relaxed)
    }
}

fn install() -> Result<(), &'static str> {
    if !mm_core::HEAP.set_scatter(&BLOCKS) {
        return Err("the heap's scattered blocks were installed already");
    }
    // No room for the list: nothing is kept, every block goes back as before. Made before the lock: 6 KiB under a
    // spin lock is a block the heap cannot reclaim for (`heap.atomic_large`).
    let mut list = Vec::new();
    if list.try_reserve_exact(KEEP_BLOCKS).is_ok() {
        let mut k = KEPT.lock();
        if k.blocks.capacity() < KEEP_BLOCKS && k.blocks.is_empty() {
            core::mem::swap(&mut k.blocks, &mut list);
        }
    }
    drop(list);
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

/// Unmaps a block (or what of it got mapped), frees its frames once every CPU dropped them and releases the range,
/// all in one shootdown round (`unmap_kernel_frames`; frames of an unconfirmed round stay out of use, counted in
/// `mm.heap.scatter_kept`, and the range is quarantined). False: not a live reservation of `len`.
fn release(body: u64, len: u64) -> bool {
    let Some(f) = mm_core::frames() else { return false };
    crate::vmap::release_with(body, len, &mut |b, l| {
        let mapped = mapped_pages(b, l);
        let mut freed = 0;
        let r = paging::unmap_kernel_frames(b, l, &mut |p| {
            f.free(p, 0, Kind::Heap);
            freed += 1;
        });
        SCATTER_KEPT.add(mapped.saturating_sub(freed));
        r
    })
    .is_some()
}

/// Pages of [b, b + l) mapped now (lock-free page-table reads; the block is the caller's).
fn mapped_pages(b: u64, l: u64) -> u64 {
    let Some(root) = paging::kernel_root() else { return 0 };
    (0..l / PAGE).filter(|i| paging::translate(root, b + i * PAGE).is_some()).count() as u64
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
