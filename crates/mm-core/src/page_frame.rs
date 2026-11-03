//! Page-cache frames (FS-5 change request 3): one 4 KiB frame of `Kind::PageCache` from the per-CPU single-frame
//! path, zeroed, reached by safe code only through bounds-checked copies, given back when the handle drops.
//! Before this the page cache took `dma::alloc` per page (exact-run path, accounted as `Kind::Dma`).
//!
//! The frame may also be mapped into programs (file mappings, MM): their stores race with the kernel's copies
//! exactly as a device's DMA races with a driver's, which the volatile `DmaRegion` accessors allow for. The
//! frame is plain data, never a Rust object.

use crate::{frames, Kind, PAGE};
use hal::dma_buf::DmaRegion;
use hal::paging::CacheType;

/// One owned, zeroed page-cache frame.
#[derive(Debug)]
pub struct PageFrame {
    region: DmaRegion,
}

impl PageFrame {
    /// A zeroed frame; `None` when memory is out.
    pub fn new_zeroed() -> Option<PageFrame> {
        let phys = crate::alloc_zeroed(Kind::PageCache)?;
        // SAFETY: `phys` is a frame the allocator just handed out (zeroed by `alloc_zeroed`, nobody else knows
        // it), RAM reachable writable and write-back through the direct map, which is its only kernel mapping;
        // the handle owns it until `drop` frees it. A later user mapping of it is write-back too (MM maps
        // page-cache frames with the default memory type).
        let region = unsafe { DmaRegion::new(hal::phys_to_virt(phys), phys, PAGE as usize, CacheType::WriteBack) };
        Some(PageFrame { region })
    }

    pub fn phys(&self) -> u64 {
        self.region.phys()
    }

    /// PF-6 (fs-PF-4 CR 2): copies `n` bytes from `off` straight from the frame to user memory at `dst` of the
    /// address space `root` (`hal::usercopy::copy_to_user_partial_raw`: the fast path, the fault resolver runs
    /// on a fault). Returns the bytes that reached it: all, or exactly those before the first user page that
    /// cannot be written; 0 when the range leaves the page and when `root` is not the loaded root of a thread
    /// that owns it here (never a page walk: any caller, with any `root`, writes only through the loaded user
    /// mapping, so safe code cannot reach a frame another space may free meanwhile). MM's
    /// `copy_frames_to_user` copies for a thread that does not own `root` another way. May sleep in the
    /// resolver: not inside a grace section (`copy_to_user_nofault` there).
    pub fn copy_to_user(&self, root: u64, off: usize, n: usize, dst: u64) -> usize {
        if off.checked_add(n).is_none_or(|e| e > self.region.len()) {
            return 0;
        }
        // SAFETY: cpu_addr+off..+n lies inside the frame (checked), which this handle owns and keeps mapped in
        // the direct map for the whole call (`&self`). The copy goes only through the loaded root's user
        // mapping (the hal call has no walk), so no lock of `root` is needed. A program writing a mapping of
        // the frame meanwhile, even the destination being that same frame (a shared mapping of this file page),
        // only races or overlaps a plain `rep movsb` byte copy (module comment), never a Rust value.
        unsafe { hal::usercopy::copy_to_user_partial_raw(root, dst, (self.region.cpu_addr() as usize + off) as *const u8, n) }
    }

    /// PF-6 (fs-PF-5 CR 2): as `copy_to_user` without handling a fault (`hal::usercopy::copy_to_user_nofault_raw`):
    /// takes no lock and never waits, so a lockless reader may call it inside a grace section; a user page not
    /// present and writable ends it (the bytes before it count), and so does a root that is not loaded here (0).
    pub fn copy_to_user_nofault(&self, root: u64, off: usize, n: usize, dst: u64) -> usize {
        if off.checked_add(n).is_none_or(|e| e > self.region.len()) {
            return 0;
        }
        // SAFETY: as in `copy_to_user` (the same fast path without the resolver: no lock, no sleep).
        unsafe { hal::usercopy::copy_to_user_nofault_raw(root, dst, (self.region.cpu_addr() as usize + off) as *const u8, n) }
    }

    /// Copies `out.len()` bytes from `off`; false (nothing copied) when the range leaves the page.
    pub fn read(&self, off: usize, out: &mut [u8]) -> bool {
        self.region.copy_out(off, out)
    }

    /// Copies `data` to `off`; false (nothing copied) when the range leaves the page.
    pub fn write(&self, off: usize, data: &[u8]) -> bool {
        self.region.copy_in(off, data)
    }

    /// Sets `n` bytes from `off` to `byte`; false when the range leaves the page.
    pub fn fill(&self, off: usize, n: usize, byte: u8) -> bool {
        self.region.fill(off, n, byte)
    }
}

impl Drop for PageFrame {
    fn drop(&mut self) {
        if let Some(fa) = frames() {
            fa.free(self.region.phys(), 0, Kind::PageCache);
        }
    }
}
