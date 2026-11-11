//! Unsafe primitives of the GX driver lane (ring barriers, MMIO polls with timeouts, ...). Owner: GX.
//! Every unsafe block needs a `// SAFETY:` comment; the hal checker reviews each item touching this file.
//!
//! `CarveWindow`: bounds-checked access to a window of the GPU's carve-out through MM's vmap allocator
//! (`paging::map_device_window`), not `mmio::map_device`: the hardware backend maps slabs on a path programs reach
//! (GEM_CREATE, GEM_VA), so it needs the allocator's lock around the page-table walk, windows `check_aperture` can
//! see, and an unmap. `map_device_window` refuses RAM and verifies the kernel tables; the `DeviceWindow` is owned by
//! the `CarveWindow`, so the mapping lives as long as any access can name it, and dropping it unmaps on every CPU.
//! Device memory is no Rust object: nothing here aliases one.

use crate::paging::{self, CacheType, DeviceWindow, VmapError};

/// A CPU window over part of the carve-out; offsets are from its first byte.
pub struct CarveWindow {
    win: DeviceWindow,
}

impl CarveWindow {
    /// Maps `len` bytes of device memory at `phys` with `cache`. May wait for the vmap mutex (not in interrupt
    /// context, not in a B4 no-wait section).
    pub fn map(phys: u64, len: u64, cache: CacheType) -> Result<CarveWindow, VmapError> {
        let win = paging::map_device_window(phys, len, cache)?;
        if win.cache() != cache || win.len() != len || win.phys() != phys {
            return Err(VmapError::Verify);
        }
        Ok(CarveWindow { win })
    }

    pub fn len(&self) -> u64 {
        self.win.len()
    }
    pub fn is_empty(&self) -> bool {
        self.win.is_empty()
    }
    pub fn phys(&self) -> u64 {
        self.win.phys()
    }

    /// The address of byte `off` when `[off, off + n)` lies inside the window and that address is `align`-aligned.
    fn place(&self, off: u64, n: u64, align: u64) -> Option<usize> {
        if !crate::BARE_METAL || off.checked_add(n)? > self.win.len() {
            return None;
        }
        // The address itself: a window may start at an unaligned physical offset.
        let at = (self.win.virt() as usize).wrapping_add(off as usize);
        at.is_multiple_of(align as usize).then_some(at)
    }

    /// One 64-bit load; None outside the window or unaligned.
    pub fn read64(&self, off: u64) -> Option<u64> {
        let at = self.place(off, 8, 8)?;
        // SAFETY: `place` checked that the 8 aligned bytes lie inside the mapped, verified window this object keeps
        // mapped. The load is volatile: device memory, one access.
        Some(unsafe { core::ptr::read_volatile(at as *const u64) })
    }

    /// One 64-bit store (what a page-table entry needs: never torn); false outside the window or unaligned.
    pub fn write64(&self, off: u64, value: u64) -> bool {
        let Some(at) = self.place(off, 8, 8) else { return false };
        // SAFETY: as in `read64`; the window is writable (`map_device_window` verified it) and holds no Rust object.
        unsafe { core::ptr::write_volatile(at as *mut u64, value) };
        true
    }

    /// `out.len()` bytes from `off`; false (nothing read) outside the window.
    pub fn read(&self, off: u64, out: &mut [u8]) -> bool {
        let Some(at) = self.place(off, out.len() as u64, 1) else { return false };
        if out.is_empty() {
            return true;
        }
        // SAFETY: the source `[at, at + n)` lies inside the mapped, verified window (checked by `place`), the
        // destination is `n` writable bytes of a live exclusive slice. DF is clear (Rust ABI).
        unsafe {
            core::arch::asm!(
                "rep movsb",
                inout("rcx") out.len() => _,
                inout("rsi") at => _,
                inout("rdi") out.as_mut_ptr() => _,
                options(nostack, preserves_flags)
            );
        }
        true
    }

    /// `data` to `off`; false (nothing written) outside the window.
    pub fn write(&self, off: u64, data: &[u8]) -> bool {
        let Some(at) = self.place(off, data.len() as u64, 1) else { return false };
        if data.is_empty() {
            return true;
        }
        // SAFETY: the destination `[at, at + n)` lies inside the mapped, verified, writable window (checked by
        // `place`), the source is `n` readable bytes of a live slice. DF is clear (Rust ABI).
        unsafe {
            core::arch::asm!(
                "rep movsb",
                inout("rcx") data.len() => _,
                inout("rsi") data.as_ptr() => _,
                inout("rdi") at => _,
                options(nostack, preserves_flags)
            );
        }
        true
    }

    /// `len` bytes (a multiple of 8) at the 8-aligned `off` to one 64-bit value, in 64-bit stores; false (nothing
    /// written) outside the window. An interrupt can land between any two stores (`rep stosq` resumes).
    pub fn fill(&self, off: u64, len: u64, value: u64) -> bool {
        if !len.is_multiple_of(8) {
            return false;
        }
        let Some(at) = self.place(off, len, 8) else { return false };
        // SAFETY: the destination `[at, at + len)` lies inside the mapped, verified, writable window (checked by
        // `place`), 8-aligned, `len / 8` quadwords long. DF is clear (Rust ABI).
        unsafe {
            core::arch::asm!(
                "rep stosq",
                inout("rcx") len / 8 => _,
                inout("rdi") at => _,
                in("rax") value,
                options(nostack, preserves_flags)
            );
        }
        true
    }
}

impl core::fmt::Debug for CarveWindow {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "CarveWindow({:?})", self.win)
    }
}

/// Every earlier store of this CPU reached memory: drains the write-combining buffers (Intel SDM Vol. 3 11.3.1), so
/// another CPU or the GPU sees them.
pub fn store_barrier() {
    if crate::BARE_METAL {
        // SAFETY: `mfence` only orders memory accesses; it has no operands.
        unsafe { core::arch::asm!("mfence", options(nostack, preserves_flags)) };
    }
}
