//! Device memory (owner: PC). `IoMem` is a mapped MMIO window with bounds-checked volatile access; it is the
//! only way safe crates touch device registers or the framebuffer (V2-PLAN §3).
//!
//! Safety argument: `map_device` refuses any range that overlaps RAM (the ranges the kernel registered from
//! the BootInfo memory map), wraps around or lies past the CPU's physical address bits, so a safe caller can
//! disturb a device but never the kernel's memory. `framebuffer` only hands out the part of the vmap area
//! below the bump start the kernel gave `init` (what bitsboot mapped there: the GOP framebuffer). `init`
//! runs once; later calls change nothing.
//! G0: windows are carved from the vmap area by a bump pointer and never unmapped; MM-2 adds a real vmap
//! allocator with unmapping, and PC-1 builds BAR windows on top.

use crate::paging::{self, CacheType, PageFlags};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MmioError {
    OverlapsRam,
    NoAddressSpace,
    MapFailed(paging::MapError),
    OutOfBounds,
    NotReady,
}

#[derive(Debug)]
pub struct IoMem {
    virt: u64,
    phys: u64,
    size: usize,
}

static NEXT_VIRT: AtomicU64 = AtomicU64::new(0);
static RAM: crate::percpu_free::Ranges = crate::percpu_free::Ranges::new();
static FRAME_SOURCE: AtomicU64 = AtomicU64::new(0);
/// End of what bitsboot mapped at the start of the vmap area (the framebuffer); `init`'s `vmap_start`.
static BOOT_MAPPED_END: AtomicU64 = AtomicU64::new(0);
/// First physical address past the CPU's physical address bits.
static PHYS_LIMIT: AtomicU64 = AtomicU64::new(1 << 52);

/// Kernel init: where the vmap bump starts, which physical ranges are RAM, and where page-table frames come
/// from (mm-core's allocator; it must hand out only free frames the page-table code then owns). One-shot: a
/// second call is refused (false) before it touches anything, so no later caller can swap the frame source,
/// the RAM list or the bump pointer.
pub fn init(vmap_start: u64, ram: Vec<(u64, u64)>, frame_source: fn() -> Option<u64>) -> bool {
    let f = frame_source as usize as u64;
    if f == 0 || FRAME_SOURCE.compare_exchange(0, f, Ordering::AcqRel, Ordering::Acquire).is_err() {
        return false;
    }
    let v = crate::cpu::live();
    let bits = if v.max_ext() >= 0x8000_0008 { v.get(0x8000_0008, 0).eax & 0xFF } else { 0 };
    PHYS_LIMIT.store(phys_limit(bits), Ordering::Release);
    RAM.set(ram);
    BOOT_MAPPED_END.store(vmap_start, Ordering::Release);
    NEXT_VIRT.store(vmap_start.saturating_add(0x1F_FFFF) & !0x1F_FFFF, Ordering::Release);
    true
}

/// 2^bits for a plausible physical address width (x86 allows 52 at most); 2^52 when CPUID gives none.
pub fn phys_limit(bits: u32) -> u64 {
    if (1..=52).contains(&bits) { 1u64 << bits } else { 1u64 << 52 }
}

fn frame() -> Option<u64> {
    let f = FRAME_SOURCE.load(Ordering::Acquire);
    if f == 0 {
        return None;
    }
    // SAFETY: only `init` stores here (once, by compare_exchange from 0), always a valid fn() -> Option<u64>.
    let f: fn() -> Option<u64> = unsafe { core::mem::transmute::<usize, fn() -> Option<u64>>(f as usize) };
    f()
}

/// The page-aligned physical range `[start, end)` covering `size` bytes at `phys`; refused when it is empty,
/// wraps around the address space or ends past `limit` (the CPU's physical address bits).
pub fn page_range(phys: u64, size: usize, limit: u64) -> Result<(u64, u64), MmioError> {
    if size == 0 {
        return Err(MmioError::OutOfBounds);
    }
    let end = phys.checked_add(size as u64).and_then(|e| e.checked_add(0xFFF)).ok_or(MmioError::OutOfBounds)? & !0xFFF;
    if end > limit {
        return Err(MmioError::OutOfBounds);
    }
    Ok((phys & !0xFFF, end))
}

/// Reserves `len` bytes (2 MiB-aligned) of the vmap area below `limit`. The bump pointer only moves when the
/// whole range fits, so a refused request costs no address space.
pub fn reserve(next: &AtomicU64, len: u64, limit: u64) -> Result<u64, MmioError> {
    let step = len.checked_add(0x1F_FFFF).ok_or(MmioError::NoAddressSpace)? & !0x1F_FFFF;
    let mut cur = next.load(Ordering::Acquire);
    loop {
        if cur == 0 {
            return Err(MmioError::NotReady);
        }
        let end = cur.checked_add(step).filter(|&e| e <= limit).ok_or(MmioError::NoAddressSpace)?;
        match next.compare_exchange_weak(cur, end, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return Ok(cur),
            Err(now) => cur = now,
        }
    }
}

/// Maps `size` bytes of device memory at `phys` with the given memory type.
pub fn map_device(phys: u64, size: usize, cache: CacheType) -> Result<IoMem, MmioError> {
    let (start, end) = page_range(phys, size, PHYS_LIMIT.load(Ordering::Acquire))?;
    if RAM.overlaps(start, end) {
        return Err(MmioError::OverlapsRam);
    }
    if FRAME_SOURCE.load(Ordering::Acquire) == 0 {
        return Err(MmioError::NotReady);
    }
    let len = end - start;
    let base = reserve(&NEXT_VIRT, len, bootinfo::layout::CPU_AREA_BASE)?;
    let root = crate::switch::stack_root();
    let flags = PageFlags::kernel_data().with_cache(cache);
    let mut off = 0;
    while off < len {
        paging::map_4k(root, base + off, start + off, flags, &mut frame).map_err(MmioError::MapFailed)?;
        off += 4096;
    }
    Ok(IoMem { virt: base + (phys - start), phys, size })
}

/// The GOP framebuffer as bitsboot mapped it (VMAP_BASE, PAT entry 1 = write-combining after `pat::init`).
/// The record's virtual range must lie inside what bitsboot mapped below `init`'s `vmap_start`; anything
/// else (a forged record, or a call before `init`) gets `None`.
pub fn framebuffer(fb: &bootinfo::Framebuffer) -> Option<IoMem> {
    let size = usize::try_from(fb.size).ok().filter(|&s| s != 0)?;
    boot_window_ok(fb.virt, fb.size, BOOT_MAPPED_END.load(Ordering::Acquire)).then_some(IoMem { virt: fb.virt, phys: fb.phys, size })
}

/// `[virt, virt + size)` lies inside `[VMAP_BASE, mapped_end)`.
pub fn boot_window_ok(virt: u64, size: u64, mapped_end: u64) -> bool {
    virt >= bootinfo::layout::VMAP_BASE && size != 0 && virt.checked_add(size).is_some_and(|e| e <= mapped_end)
}

macro_rules! access {
    ($r:ident, $w:ident, $t:ty) => {
        pub fn $r(&self, off: usize) -> Result<$t, MmioError> {
            self.check(off, core::mem::size_of::<$t>())?;
            // SAFETY: inside the mapped window (checked), naturally aligned by `check`, device memory.
            Ok(unsafe { ((self.virt as usize + off) as *const $t).read_volatile() })
        }
        pub fn $w(&self, off: usize, v: $t) -> Result<(), MmioError> {
            self.check(off, core::mem::size_of::<$t>())?;
            // SAFETY: as above.
            unsafe { ((self.virt as usize + off) as *mut $t).write_volatile(v) };
            Ok(())
        }
    };
}

impl IoMem {
    fn check(&self, off: usize, width: usize) -> Result<(), MmioError> {
        // Alignment of the address itself: a window may start at an unaligned physical offset.
        if !(self.virt as usize).wrapping_add(off).is_multiple_of(width) || off.checked_add(width).is_none_or(|e| e > self.size) {
            Err(MmioError::OutOfBounds)
        } else {
            Ok(())
        }
    }
    access!(read8, write8, u8);
    access!(read16, write16, u16);
    access!(read32, write32, u32);
    access!(read64, write64, u64);

    pub fn size(&self) -> usize {
        self.size
    }
    pub fn phys(&self) -> u64 {
        self.phys
    }

    /// Copies bytes into the window (framebuffers; never read back from a framebuffer, it is slow).
    pub fn write_bytes(&self, off: usize, src: &[u8]) -> Result<(), MmioError> {
        if off.checked_add(src.len()).is_none_or(|e| e > self.size) {
            return Err(MmioError::OutOfBounds);
        }
        // SAFETY: the destination range is inside the mapped window (checked).
        unsafe { core::ptr::copy_nonoverlapping(src.as_ptr(), (self.virt as usize + off) as *mut u8, src.len()) };
        Ok(())
    }

    /// Fills `len` 32-bit pixels starting at byte `off` with `value`.
    pub fn fill32(&self, off: usize, len: usize, value: u32) -> Result<(), MmioError> {
        let bytes = len.checked_mul(4).ok_or(MmioError::OutOfBounds)?;
        if !(self.virt as usize).wrapping_add(off).is_multiple_of(4) || off.checked_add(bytes).is_none_or(|e| e > self.size) {
            return Err(MmioError::OutOfBounds);
        }
        let p = (self.virt as usize + off) as *mut u32;
        for i in 0..len {
            // SAFETY: inside the window (checked above), aligned.
            unsafe { p.add(i).write_volatile(value) };
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_range_refuses_wrap_and_high_addresses() {
        let limit = phys_limit(39);
        assert_eq!(page_range(0xFE00_0010, 0x20, limit), Ok((0xFE00_0000, 0xFE00_1000)));
        // The checker's case: the end wraps to a small number, which used to map physical page 0.
        assert_eq!(page_range(0xFFFF_FFFF_FFFF_F000, 0x2000, phys_limit(52)), Err(MmioError::OutOfBounds));
        assert_eq!(page_range(u64::MAX - 0x800, 0x100, phys_limit(52)), Err(MmioError::OutOfBounds));
        // Past the CPU's physical address bits (reserved PTE bits otherwise).
        assert_eq!(page_range(1 << 39, 0x1000, limit), Err(MmioError::OutOfBounds));
        assert_eq!(page_range((1 << 39) - 0x1000, 0x1000, limit), Ok(((1 << 39) - 0x1000, 1 << 39)));
        assert_eq!(page_range((1 << 39) - 0x1000, 0x1001, limit), Err(MmioError::OutOfBounds));
        assert_eq!(page_range(0x1000, 0, limit), Err(MmioError::OutOfBounds));
        assert_eq!(phys_limit(0), 1 << 52);
        assert_eq!(phys_limit(64), 1 << 52);
    }

    #[test]
    fn reserve_moves_only_when_the_range_fits() {
        let next = AtomicU64::new(0);
        assert_eq!(reserve(&next, 0x1000, 1 << 40), Err(MmioError::NotReady));
        next.store(0x20_0000, Ordering::Relaxed);
        assert_eq!(reserve(&next, 0x1000, 0x80_0000), Ok(0x20_0000));
        assert_eq!(next.load(Ordering::Relaxed), 0x40_0000);
        // Too large: refused, and the bump pointer has not moved.
        assert_eq!(reserve(&next, 0x40_0001, 0x80_0000), Err(MmioError::NoAddressSpace));
        assert_eq!(reserve(&next, u64::MAX - 0x10, u64::MAX), Err(MmioError::NoAddressSpace));
        assert_eq!(next.load(Ordering::Relaxed), 0x40_0000);
        assert_eq!(reserve(&next, 0x40_0000, 0x80_0000), Ok(0x40_0000));
    }

    #[test]
    fn framebuffer_window_only_inside_the_boot_mapping() {
        let base = bootinfo::layout::VMAP_BASE;
        let end = base + 0x80_0000;
        assert!(boot_window_ok(base, 0x80_0000, end));
        assert!(!boot_window_ok(base, 0x80_0001, end));
        assert!(!boot_window_ok(base - 0x1000, 0x1000, end), "below the vmap area (direct map)");
        assert!(!boot_window_ok(0xFFFF_FFFF_8000_0000, 0x1000, end), "kernel image");
        assert!(!boot_window_ok(u64::MAX - 0xF, 0x100, u64::MAX), "wraps");
        assert!(!boot_window_ok(base, 0x1000, 0), "before init");
        assert!(!boot_window_ok(base, 0, end));
    }

    #[test]
    fn access_checks_the_address_alignment() {
        let m = IoMem { virt: 0x1002, phys: 0xFE00_0002, size: 0x100 };
        assert_eq!(m.check(0, 4), Err(MmioError::OutOfBounds));
        assert_eq!(m.check(2, 4), Ok(()));
        assert_eq!(m.check(0xFE, 4), Err(MmioError::OutOfBounds));
    }
}
