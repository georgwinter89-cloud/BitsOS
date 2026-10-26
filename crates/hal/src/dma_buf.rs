//! CPU access to DMA buffers (owner: MM). Volatile, because a device may write the memory at any time.
//!
//! `DmaRegion` is the handle drivers reach their buffers through (contract `C: DmaPool`, desk decision 6a):
//! only trusted code can make one (`unsafe fn new`; in practice only mm-core's DMA allocation does), so safe
//! code can never name physical memory it does not own. Every access is bounds-checked against the region;
//! typed accesses are one naturally aligned access each (a device never sees a torn ring index); bulk copies
//! use `rep movsb` (ERMS/FSRM on both machines, no vector registers in the kernel, R10). The CPU view
//! (`cpu_addr`) is the region's own mapping, never derived from the device address, so UC/WC buffers and a
//! later IOMMU keep the accessors right.
//!
//! Known gap until CP switches (change request in plan/notes/mm-MM-4.md): the free functions `read`/`write`
//! taking a raw `(phys, len)` are the G0 helpers `acpi` still uses for firmware tables and its own test
//! frames. They are safe functions over memory the caller merely names, so the guarantee above does not hold
//! for them; they are deprecated (new callers get a warning) and bounded to the direct map (never the vmap,
//! MMIO or image windows). `write` goes when CP switches; the table reader becomes a bounded read-only helper.

use crate::paging::CacheType;

/// A DMA buffer as the CPU sees it. Not `Clone`: one owner, who also owns the frames.
#[derive(Debug)]
pub struct DmaRegion {
    virt: u64,
    phys: u64,
    len: usize,
    cache: CacheType,
}

impl DmaRegion {
    /// # Safety
    /// [phys, phys + len) is memory the caller owns exclusively for the region's whole life (no Rust object,
    /// no other owner), mapped present and writable at [virt, virt + len) with memory type `cache`, and no
    /// other mapping of it has another type. `virt + len` does not overflow.
    pub unsafe fn new(virt: u64, phys: u64, len: usize, cache: CacheType) -> Self {
        DmaRegion { virt, phys, len, cache }
    }
    /// The CPU's view of byte 0 (diagnostics; the accessors below are the way to touch the memory).
    pub fn cpu_addr(&self) -> u64 {
        self.virt
    }
    /// The physical address of byte 0 (with an IOMMU the device address differs).
    pub fn phys(&self) -> u64 {
        self.phys
    }
    pub fn len(&self) -> usize {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    pub fn cache(&self) -> CacheType {
        self.cache
    }

    /// The CPU address of byte `off` if `width` bytes from there lie inside the region and the access is
    /// naturally aligned (`width` 1 = any); all arithmetic checked.
    fn at(&self, off: usize, width: usize) -> Option<u64> {
        if off.checked_add(width)? > self.len {
            return None;
        }
        let p = self.virt.checked_add(off as u64)?;
        p.is_multiple_of(width as u64).then_some(p)
    }

    /// The CPU address of byte `off` if `n` bytes from there lie inside the region (`n` may be 0).
    fn span(&self, off: usize, n: usize) -> Option<u64> {
        if off.checked_add(n)? > self.len {
            return None;
        }
        self.virt.checked_add(off as u64)
    }

    /// Makes earlier writes visible to the device before a doorbell: drains the write-combining buffers for a
    /// WC region (`sfence`); WB and UC writes are ordered on x86 already, so only the compiler is fenced.
    pub fn write_barrier(&self) {
        #[cfg(target_os = "none")]
        if self.cache == CacheType::WriteCombining {
            // SAFETY: a fence has no memory effect beyond ordering.
            unsafe { core::arch::asm!("sfence", options(nostack, preserves_flags)) };
        }
        core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
    }

    /// Copies `src` into the region at `off`; false (nothing written) if it does not fit.
    pub fn copy_in(&self, off: usize, src: &[u8]) -> bool {
        let Some(p) = self.span(off, src.len()) else {
            return false;
        };
        // SAFETY: [p, p + src.len()) lies inside the region (checked), which `new`'s contract makes memory
        // nobody else owns, mapped writable; `src` is a caller slice, so the two never overlap.
        unsafe { copy(p as *mut u8, src.as_ptr(), src.len()) };
        true
    }

    /// Copies from the region at `off` into `dst`; false (nothing read) if it does not fit.
    pub fn copy_out(&self, off: usize, dst: &mut [u8]) -> bool {
        let Some(p) = self.span(off, dst.len()) else {
            return false;
        };
        // SAFETY: as copy_in, with the roles swapped.
        unsafe { copy(dst.as_mut_ptr(), p as *const u8, dst.len()) };
        true
    }

    /// Sets `n` bytes from `off` to `byte`; false (nothing written) if they do not fit.
    pub fn fill(&self, off: usize, n: usize, byte: u8) -> bool {
        let Some(p) = self.span(off, n) else {
            return false;
        };
        #[cfg(target_arch = "x86_64")]
        // SAFETY: [p, p + n) lies inside the region (checked), memory nobody else owns, mapped writable;
        // `rep stosb` writes exactly rcx bytes upwards (direction flag clear by the ABI).
        unsafe {
            core::arch::asm!("rep stosb", inout("rcx") n => _, inout("rdi") p => _, in("al") byte, options(nostack, preserves_flags))
        };
        true
    }
}

/// `rep movsb`: with ERMS/FSRM the fastest copy that needs no vector registers (R10).
///
/// # Safety
/// `dst` and `src` are valid for `n` bytes and do not overlap.
unsafe fn copy(dst: *mut u8, src: *const u8, n: usize) {
    // SAFETY: the caller's contract; `rep movsb` moves exactly rcx bytes upwards (direction flag clear by the
    // ABI) and changes no flags.
    unsafe {
        core::arch::asm!("rep movsb", inout("rcx") n => _, inout("rsi") src => _, inout("rdi") dst => _, options(nostack, preserves_flags));
    }
}

macro_rules! rw {
    ($r:ident, $w:ident, $t:ty) => {
        impl DmaRegion {
            /// One naturally aligned volatile read inside the region; None if outside or misaligned.
            pub fn $r(&self, off: usize) -> Option<$t> {
                let p = self.at(off, core::mem::size_of::<$t>())? as *const $t;
                // SAFETY: inside the region (checked), naturally aligned, so one bus access; `new`'s contract
                // makes it memory nobody else owns, mapped with the region's type.
                Some(unsafe { p.read_volatile() })
            }
            /// One naturally aligned volatile write inside the region; false (nothing written) if outside or
            /// misaligned.
            pub fn $w(&self, off: usize, v: $t) -> bool {
                let Some(p) = self.at(off, core::mem::size_of::<$t>()) else { return false };
                // SAFETY: as the read above; the mapping is writable.
                unsafe { (p as *mut $t).write_volatile(v) };
                true
            }
        }
    };
}

rw!(read_u8, write_u8, u8);
rw!(read_u16, write_u16, u16);
rw!(read_u32, write_u32, u32);
rw!(read_u64, write_u64, u64);

/// True if [phys, phys + len) lies inside the direct map's span (it may still name a hole: the G0 contract).
fn in_direct_map(phys: u64, len: usize) -> bool {
    phys.checked_add(len as u64).is_some_and(|e| e <= crate::VMAP_BASE - crate::DIRECT_MAP_BASE)
}

/// G0 helper (acpi only, see the module doc): reads RAM at a raw physical address through the direct map.
#[deprecated(note = "raw physical access; use DmaRegion (change request in plan/notes/mm-MM-4.md)")]
pub fn read(phys: u64, len: usize, off: usize, out: &mut [u8]) -> bool {
    if off.checked_add(out.len()).is_none_or(|e| e > len) || !in_direct_map(phys, len) {
        return false;
    }
    let src = (crate::phys_to_virt(phys) as usize + off) as *const u8;
    for (i, b) in out.iter_mut().enumerate() {
        // SAFETY: trust by convention (G0): the caller names direct-mapped RAM it may read (checked bounds).
        *b = unsafe { src.add(i).read_volatile() };
    }
    true
}

/// G0 helper (acpi's test frames only, see the module doc): writes RAM at a raw physical address.
#[deprecated(note = "raw physical access; use DmaRegion (change request in plan/notes/mm-MM-4.md)")]
pub fn write(phys: u64, len: usize, off: usize, data: &[u8]) -> bool {
    if off.checked_add(data.len()).is_none_or(|e| e > len) || !in_direct_map(phys, len) {
        return false;
    }
    let dst = (crate::phys_to_virt(phys) as usize + off) as *mut u8;
    for (i, b) in data.iter().enumerate() {
        // SAFETY: trust by convention (G0): the caller names frames it owns (checked bounds).
        unsafe { dst.add(i).write_volatile(*b) };
    }
    true
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec;

    /// A region over a host buffer (the test owns it exclusively and outlives the region).
    fn region(buf: &mut [u64], len: usize) -> DmaRegion {
        assert!(len <= buf.len() * 8);
        // SAFETY: test-owned memory, nothing else touches it while the region lives.
        unsafe { DmaRegion::new(buf.as_mut_ptr() as u64, 0x1000, len, CacheType::WriteBack) }
    }

    #[test]
    fn typed_accesses_are_aligned_and_bounded() {
        let mut buf = vec![0u64; 4];
        let r = region(&mut buf, 30);
        assert!(r.write_u16(28, 0xBEEF));
        assert_eq!(r.read_u16(28), Some(0xBEEF));
        assert!(!r.write_u16(29, 1), "misaligned");
        assert!(!r.write_u16(30, 1), "past the end");
        assert!(r.write_u64(16, 0x0123_4567_89AB_CDEF));
        assert_eq!(r.read_u32(16), Some(0x89AB_CDEF), "little endian");
        assert_eq!(r.read_u64(24), None, "u64 at 24 would end at 32 > 30");
        assert!(!r.write_u32(usize::MAX - 1, 1), "overflowing offset");
        assert_eq!(r.read_u8(29), Some(0xBE), "high byte of the u16 at 28");
        assert!(r.write_u8(29, 7) && r.read_u8(29) == Some(7));
    }

    #[test]
    #[allow(deprecated)]
    fn raw_helpers_stay_inside_the_direct_map() {
        let span = crate::VMAP_BASE - crate::DIRECT_MAP_BASE;
        let mut b = [0u8; 1];
        assert!(!read(span, 1, 0, &mut b), "first byte past the direct map");
        assert!(!read(span - 1, 2, 0, &mut b), "range reaching past it");
        assert!(!write(u64::MAX, 1, 0, &[1]), "overflow");
        assert!(!write(0, 1, 1, &[1]), "offset past len");
    }

    #[test]
    fn copies_and_fill_are_bounded() {
        let mut buf = vec![0u64; 4];
        let r = region(&mut buf, 32);
        assert!(r.copy_in(30, &[1, 2]));
        assert!(!r.copy_in(31, &[1, 2]), "one byte too far");
        assert!(!r.copy_in(usize::MAX, &[1]), "overflow");
        let mut out = [0u8; 2];
        assert!(r.copy_out(30, &mut out) && out == [1, 2]);
        assert!(!r.copy_out(31, &mut out));
        assert!(r.fill(0, 32, 0xAA));
        assert!(!r.fill(1, 32, 0));
        assert_eq!(r.read_u64(0), Some(0xAAAA_AAAA_AAAA_AAAA));
        assert!(r.copy_in(32, &[]), "an empty copy at the end is fine");
        r.write_barrier();
    }
}
