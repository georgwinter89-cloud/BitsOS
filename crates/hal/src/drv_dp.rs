//! Unsafe primitives of the DP driver lane. Owner: DP.
//! Every unsafe block needs a `// SAFETY:` comment; the hal checker reviews each item touching this file.
//!
//! What lives here: `FbWindow`, the GOP framebuffer mapped write-combining through MM-2's vmap
//! (`paging::map_device_window`), and the two things gopfb does with it: bounded copies into it (`rep movsb`;
//! ERMS/FSRM on both machines, CONTRACTS §6a: no SSE in the kernel) and an `sfence` that drains the
//! write-combining buffers. There is deliberately **no read**: reading the framebuffer is 25-100x slower than
//! writing it (spec/gpu.md §2.3); the picture's truth is the RAM frame.
//!
//! Soundness: `map_device_window` refuses RAM and verifies the kernel page tables (every page present,
//! writable, NX, not user, exactly the asked physical range with the asked type) before it hands out a
//! window; the `DeviceWindow` is owned by the `FbWindow`, so the mapping lives as long as any copy can name
//! it. Device memory is no Rust object, so a store there aliases nothing. Every copy is bounds-checked against
//! the window length with checked arithmetic, and the source is a Rust slice (valid for its length; a `u32`
//! slice is read as its bytes, which is fine: u32 has no padding).
//!
//! Host builds (unit tests): no window can be mapped (no device mapper), and the copy refuses anyway.
//!
//! DP-2: `copy_from_region_u32` reads fbdev's RAM pages (MM-4 DMA buffers that programs map) straight into a
//! RAM frame's `u32` pixels with one `rep movsb` (the safe `DmaRegion::copy_out` only fills byte slices, which
//! would cost a second copy per frame).

use crate::dma_buf::DmaRegion;
use crate::paging::{self, CacheType, DeviceWindow, VmapError};

/// Copies `dst.len()` 32-bit pixels from byte `off` of the DMA region `r` into `dst`; false (nothing copied)
/// if they do not lie inside the region. Programs may store into the region at the same time (their mapping
/// of fbdev's pages): the copy then holds old or new bytes of those pixels, never anything outside.
pub fn copy_from_region_u32(r: &DmaRegion, off: usize, dst: &mut [u32]) -> bool {
    let Some(n) = dst.len().checked_mul(4) else { return false };
    if off.checked_add(n).is_none_or(|e| e > r.len()) {
        return false;
    }
    let Some(src) = r.cpu_addr().checked_add(off as u64) else { return false };
    if n == 0 {
        return true;
    }
    let dst = dst.as_mut_ptr().cast::<u8>();
    // SAFETY: `DmaRegion::new`'s contract (only mm-core's DMA allocation makes one) says [cpu_addr, cpu_addr +
    // len) is mapped, readable memory the region owns for its whole life, and `r` is borrowed for this call;
    // [src, src + n) lies inside it (checked above, no overflow). That memory is no Rust object (devices and
    // programs write it), so reading it aliases nothing. `dst` is a live, exclusively borrowed slice of `n`
    // bytes (u32 has no padding and every byte pattern is a valid u32), so the two never overlap. DF is clear
    // (Rust ABI); `rep movsb` moves exactly rcx bytes and changes no flags.
    unsafe {
        core::arch::asm!(
            "rep movsb",
            inout("rcx") n => _,
            inout("rsi") src => _,
            inout("rdi") dst => _,
            options(nostack, preserves_flags)
        );
    }
    true
}

/// A write-only, write-combining window over a framebuffer.
pub struct FbWindow {
    win: DeviceWindow,
}

impl FbWindow {
    /// Maps `len` bytes of the framebuffer at `phys` write-combining through the vmap allocator. May wait for
    /// the vmap mutex (not in interrupt context, not in a B4 no-wait section).
    pub fn map_wc(phys: u64, len: u64) -> Result<FbWindow, VmapError> {
        let win = paging::map_device_window(phys, len, CacheType::WriteCombining)?;
        if win.cache() != CacheType::WriteCombining || win.len() != len || win.phys() != phys {
            return Err(VmapError::Verify);
        }
        Ok(FbWindow { win })
    }

    pub fn len(&self) -> usize {
        self.win.len() as usize
    }
    pub fn is_empty(&self) -> bool {
        self.win.is_empty()
    }
    pub fn phys(&self) -> u64 {
        self.win.phys()
    }
    /// Virtual address of byte 0 (for tests that check the page tables; never dereferenced outside this file).
    pub fn virt(&self) -> u64 {
        self.win.virt()
    }

    /// Copies `src` to byte `off` of the window; false (nothing written) if it does not fit.
    pub fn write_bytes(&self, off: usize, src: &[u8]) -> bool {
        self.copy_in(off, src.as_ptr(), src.len())
    }

    /// Copies 32-bit pixels (little-endian, as they lie in memory) to byte `off` of the window.
    pub fn write_pixels(&self, off: usize, src: &[u32]) -> bool {
        let Some(n) = src.len().checked_mul(4) else { return false };
        self.copy_in(off, src.as_ptr().cast::<u8>(), n)
    }

    /// `n` bytes from `src` (the pointer and byte length of a live slice) to byte `off`.
    fn copy_in(&self, off: usize, src: *const u8, n: usize) -> bool {
        if !crate::BARE_METAL || off.checked_add(n).is_none_or(|e| e > self.len()) {
            return false;
        }
        if n == 0 {
            return true;
        }
        let dst = (self.win.virt() as usize).wrapping_add(off);
        // SAFETY: the destination `[dst, dst + n)` lies inside the mapped, verified device window (checked
        // above, no overflow), which this FbWindow keeps mapped; the source is `n` readable bytes of a live
        // slice (both callers pass a slice's pointer and byte length). DF is clear (Rust ABI). Device memory is
        // no Rust object, so nothing is aliased.
        unsafe {
            core::arch::asm!(
                "rep movsb",
                inout("rcx") n => _,
                inout("rsi") src => _,
                inout("rdi") dst => _,
                options(nostack, preserves_flags)
            );
        }
        true
    }

    /// Drains the write-combining buffers (after a present, before a fence is signalled).
    pub fn flush(&self) {
        if crate::BARE_METAL {
            // SAFETY: `sfence` only orders stores; it has no operands.
            unsafe { core::arch::asm!("sfence", options(nostack, preserves_flags)) };
        }
    }
}

impl core::fmt::Debug for FbWindow {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "FbWindow({:?})", self.win)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_window_on_the_host() {
        assert!(FbWindow::map_wc(0xC000_0000, 4096).is_err());
    }
}
