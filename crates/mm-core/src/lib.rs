//! Physical frames and the kernel heap (owner: MM, trusted base; contract `C: FrameAlloc, GlobalAlloc`).
//!
//! MM-1: a buddy allocator (`buddy`) over the BootInfo memory map with zones DMA32/NORMAL, per-CPU lists for
//! single frames, owner kinds, statistics and refused bad frees; its tables are sized from the map and carved
//! from the pool (R1). MM-3: the kernel heap (`heap`) is slab caches (`slab`) for the size classes 8..2048
//! with per-CPU magazines (`mags`) and exact page runs above; named `SlabCache`s for kernel objects; the
//! seed chunk serves before the frame allocator exists (`pages`). Every allocation names its owner kind
//! (YK-1 rule 3).
#![no_std]

extern crate alloc;

mod bitmap;
pub mod dma;
pub mod buddy;
pub mod fixtures;
mod heap;
mod heap_stats;
mod mags;
pub mod map;
mod meta;
mod page_frame;
mod pages;
mod pcp;
pub mod reclaim;
pub mod report;
pub mod slab;
#[cfg(test)]
mod tests;

pub use buddy::{boot_buddy, Buddy, BuddyFrames, BuildError, ReleaseError, Storage, ZoneInfo, ORDERS};
pub use heap::{GlobalHeap, KernelHeap, HEAP};
pub use heap_stats::{HeapStats, PROBE_REFUSALS};
pub use page_frame::PageFrame;
pub use pages::SeedStats;
pub use slab::{CacheStats, CheckError, SlabBox, SlabCache};
pub use map::{MapRange, RangeUse};
pub use report::{flush_reports, BadFree};

use ksync::OnceRef;

pub const PAGE: u64 = 4096;
/// Largest block: 2^18 frames = 1 GiB, the largest x86 page. A bound of the buddy math (orders 0..=18), not a
/// limit on how much memory or how many blocks exist (R1).
pub const MAX_ORDER: u32 = 18;
/// The G0 name of the frame allocator, kept so `kernel/` builds unchanged; it is the buddy allocator now.
pub type BumpFrames = BuddyFrames;

/// Where a frame may come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Zone {
    /// Below 4 GiB (devices with 32-bit DMA, e.g. the RTL8852BE; the AP trampoline's tables).
    Dma32,
    /// Anywhere.
    Normal,
}

/// Who owns an allocation (accounting, `hw_info memory`, the POOL_KEEP rule of R2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum Kind {
    Heap = 0,
    PageTable = 1,
    User = 2,
    Dma = 3,
    Stack = 4,
    PageCache = 5,
    Driver = 6,
    Other = 7,
}
pub const KINDS: usize = 8;

#[derive(Clone, Copy, Debug, Default)]
pub struct FrameStats {
    pub total_bytes: u64,
    pub free_bytes: u64,
    pub used_by_kind: [u64; KINDS],
    /// Bytes freed but not reusable. Always 0 since MM-1 (kept for the contract).
    pub leaked_bytes: u64,
}

pub trait FrameAlloc: Sync {
    /// 2^order contiguous 4 KiB frames, aligned to their size. Returns the physical address.
    fn alloc(&self, order: u32, zone: Zone, kind: Kind) -> Option<u64>;
    fn free(&self, phys: u64, order: u32, kind: Kind);
    fn stats(&self) -> FrameStats;
}

static FRAMES: OnceRef<dyn FrameAlloc> = OnceRef::new();

/// Installs the frame allocator (kernel init).
pub fn set_frame_allocator(a: &'static dyn FrameAlloc) {
    FRAMES.set(a);
}

pub fn frames() -> Option<&'static dyn FrameAlloc> {
    FRAMES.get()
}

/// Creates the per-CPU caches (heap magazines, the boot buddy's frame lists) of every possible CPU. The BSP
/// calls it after the possible-CPU count is known and before the APs start: the allocation paths never create a
/// per-CPU slot themselves (that would re-enter the allocator), so a CPU without one only takes the slower
/// shared path.
pub fn prepare_cpus() {
    HEAP.prepare_cpus();
    if let Some(b) = boot_buddy() {
        b.prepare_cpus();
    }
}

/// One frame of `kind`, zeroed.
pub fn alloc_zeroed(kind: Kind) -> Option<u64> {
    alloc_zeroed_block(0, Zone::Normal, kind)
}

/// 2^order frames of `kind` from `zone`, zeroed (DMA buffers). The zeroing stays inside mm-core, on a block
/// it just handed out, so no caller can point it at memory it does not own.
pub fn alloc_zeroed_block(order: u32, zone: Zone, kind: Kind) -> Option<u64> {
    let p = frames()?.alloc(order, zone, kind)?;
    for i in 0..1u64 << order {
        zero_frame(p + i * PAGE);
    }
    Some(p)
}

/// A page-table frame (zeroed by hal when used as a table); the hook hal::mmio and hal::paging use.
pub fn alloc_table_frame() -> Option<u64> {
    frames()?.alloc(0, Zone::Normal, Kind::PageTable)
}

/// Gives back a page-table frame from `alloc_table_frame` (hal::paging frees tables that became empty).
pub fn free_table_frame(phys: u64) {
    if let Some(f) = frames() {
        f.free(phys, 0, Kind::PageTable);
    }
}

/// Crate-private: only for a frame the caller just got from `frames()` and has not passed on.
fn zero_frame(phys: u64) {
    #[cfg(target_os = "none")]
    // SAFETY: the only callers (`alloc_zeroed_block`) pass a frame the allocator just handed them and nobody
    // else knows yet, RAM reachable through the direct map.
    unsafe {
        core::ptr::write_bytes(hal::phys_to_virt(phys) as *mut u8, 0, PAGE as usize)
    };
    #[cfg(not(target_os = "none"))]
    let _ = phys;
}

/// The frame kinds `alloc_copy` copies from: page-cache frames (accounted as `Kind::Dma` until FS-5 change
/// request 3 gives them `Kind::PageCache`) and user frames (a MAP_PRIVATE file page that was already copied
/// once, then made read-only and writable again by mprotect, is copied again on the next write fault). Never a
/// kernel frame (heap, page table, stack, driver, other): those are refused, so safe code cannot copy kernel
/// memory into a frame it then maps for a program (INT-A1b checker finding).
const COPY_SOURCES: [Kind; 3] = [Kind::Dma, Kind::PageCache, Kind::User];

/// FS-5 (COW of a file page): a new frame of `kind` holding a copy of the frame at `src`. Refused (None) unless
/// `src` is an allocated order-0 frame of a kind in `COPY_SOURCES` right now, so the copy reads only RAM in use
/// as page-cache or user memory, never a kernel frame, MMIO or a hole; the write goes only to the frame this
/// call just allocated. The caller must hold `src` for the whole call (a page-cache pin, or the address-space
/// lock over a mapping that owns the user frame), so it cannot be freed and handed to someone else between the
/// check and the copy.
pub fn alloc_copy(src: u64, kind: Kind) -> Option<u64> {
    let b = boot_buddy()?;
    if !COPY_SOURCES.iter().any(|&k| b.run_allocated(src, 1, k)) {
        return None;
    }
    let dst = frames()?.alloc(0, Zone::Normal, kind)?;
    #[cfg(target_os = "none")]
    // SAFETY: `dst` was just handed to us (nobody else knows it); `src` is an allocated page-cache or user RAM
    // frame (checked above, kept allocated by the caller's pin per the contract in the doc comment), both
    // reachable through the direct map; the two frames differ, so the ranges do not overlap. A page-cache or
    // user frame is plain data with no Rust object living in it, so a concurrent writer can at worst make the
    // copy show a mix of old and new bytes (as a user write racing a COW copy does on Linux), never break a
    // kernel invariant.
    unsafe {
        core::ptr::copy_nonoverlapping(hal::phys_to_virt(src) as *const u8, hal::phys_to_virt(dst) as *mut u8, PAGE as usize)
    };
    Some(dst)
}

/// MM-5b (COW): is `phys` an allocated order-0 frame of `Kind::User` right now? A lock-free look at the
/// allocator's own state, safe for any address. Tells a program's own frames (anonymous memory, private
/// copies: shared by fork through MM's share counts) from page-cache and device frames a mapping only borrows.
pub fn is_user_frame(phys: u64) -> bool {
    boot_buddy().is_some_and(|b| b.run_allocated(phys, 1, Kind::User))
}
