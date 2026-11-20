//! Page-cache frames (FS-5 change request 3): one 4 KiB frame of `Kind::PageCache` from the per-CPU single-frame
//! path, zeroed, reached by safe code only through bounds-checked copies, given back when the handle drops.
//! Before this the page cache took `dma::alloc` per page (exact-run path, accounted as `Kind::Dma`).
//!
//! The frame may also be mapped into programs (file mappings, MM): their stores race with the kernel's copies
//! exactly as a device's DMA races with a driver's, which the volatile `DmaRegion` accessors allow for. The
//! frame is plain data, never a Rust object.

use crate::compact::Target;
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
        // SAFETY: `phys` is a frame the allocator just handed out (zeroed by `alloc_zeroed`, nobody else knows it);
        // RAM reachable writable and write-back through the direct map, which is its only kernel mapping; the
        // handle owns it until `drop` frees it. A later user mapping of it is write-back too (MM maps page-cache
        // frames with the default memory type).
        Some(PageFrame { region: unsafe { Self::region(phys) } })
    }

    /// W35 MM-2 (INT-W35-MM2, change request 1): the migration target `t` becomes a page-cache frame, zeroed (the
    /// moved page's new home: vfs `Frame::from_page_frame` wraps it). Only an order-0 `Kind::PageCache` block of
    /// the boot allocator (the one `drop` frees to); Err(t) gives any other target back untouched. The handle is
    /// the proof that nobody else knows the frame: a plain address could not prove that.
    pub fn from_target(t: Target<'_>) -> Result<PageFrame, Target<'_>> {
        if t.order() != 0 || t.kind() != Kind::PageCache || !crate::boot_buddy().is_some_and(|b| core::ptr::eq(b, t.buddy())) {
            return Err(t);
        }
        let phys = t.into_phys();
        crate::zero_frame(phys);
        // SAFETY: `phys` came out of a `Target` (made only by the allocator for a block it just handed out, never
        // passed on while the handle lived), consumed here, so nobody else knows the frame; an order-0 page-cache
        // block of the boot allocator (checked), which `drop` frees it to; zeroed above; RAM reachable writable and
        // write-back through the direct map (as in `new_zeroed`).
        Ok(PageFrame { region: unsafe { Self::region(phys) } })
    }

    /// # Safety
    /// `phys` is an order-0 RAM frame the caller alone owns and hands to the new handle.
    unsafe fn region(phys: u64) -> DmaRegion {
        // SAFETY: the caller's contract (an owned RAM frame, direct-mapped write-back).
        unsafe { DmaRegion::new(hal::phys_to_virt(phys), phys, PAGE as usize, CacheType::WriteBack) }
    }

    pub fn phys(&self) -> u64 {
        self.region.phys()
    }

    /// PF-6 (PF-4's CR 2): copies `n` bytes from `off` straight from the frame to user memory at `dst` of the
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

    /// PF-6 (PF-5's CR 2): as `copy_to_user` without handling a fault (`hal::usercopy::copy_to_user_nofault_raw`):
    /// takes no lock and never waits, so a lockless reader may call it inside a grace section; a user page not
    /// present and writable ends it (the bytes before it count), and so does a root that is not loaded here (0).
    pub fn copy_to_user_nofault(&self, root: u64, off: usize, n: usize, dst: u64) -> usize {
        if off.checked_add(n).is_none_or(|e| e > self.region.len()) {
            return 0;
        }
        // SAFETY: as in `copy_to_user` (the same fast path without the resolver: no lock, no sleep).
        unsafe { hal::usercopy::copy_to_user_nofault_raw(root, dst, (self.region.cpu_addr() as usize + off) as *const u8, n) }
    }

    /// W35 MM-2: the bytes of `src` into this frame (a migration's copy of a page-cache page). Both handles own
    /// their frames for the call; a program storing into a mapping of either only races a plain byte copy.
    pub fn copy_from(&self, src: &PageFrame) {
        if src.phys() == self.phys() {
            return;
        }
        #[cfg(target_os = "none")]
        // SAFETY: both frames are owned by live handles (`&self`, `&src`) and stay mapped in the direct map for the
        // call; they are different frames (checked) of PAGE bytes each. Frame contents are plain data (module
        // comment), so a racing store only mixes bytes.
        unsafe { core::ptr::copy_nonoverlapping(src.region.cpu_addr() as *const u8, self.region.cpu_addr() as *mut u8, PAGE as usize) }
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
        // W35 MM-2: the page cache's owner word goes with the page, so the frame's next owner (an anonymous page
        // above all) never starts a migration's search from a stale word.
        crate::reclaim::set_frame_owner(self.region.phys(), 0);
        if let Some(fa) = frames() {
            fa.free(self.region.phys(), 0, Kind::PageCache);
        }
    }
}
