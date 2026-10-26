//! Low-level DMA helpers (owner: MM): physical/virtual translation of the direct map and cache maintenance.
//! The DMA API drivers use is `mm::dma` (contract `C: DmaPool`).

/// Physical address of a direct-map virtual address (kernel heap and frames), if it is one.
pub fn virt_to_phys(virt: u64) -> Option<u64> {
    bootinfo::layout::virt_to_phys(virt)
}

pub fn phys_to_virt(phys: u64) -> u64 {
    bootinfo::layout::phys_to_virt(phys)
}

/// Writes back and invalidates the cache lines of a buffer (for devices without snooping, e.g. HDA no-snoop).
pub fn flush_cache_range(virt: u64, len: u64) {
    #[cfg(target_os = "none")]
    {
        let mut a = virt & !63;
        while a < virt + len {
            // SAFETY: clflush on a mapped kernel address only affects caches.
            unsafe { core::arch::asm!("clflush [{}]", in(reg) a, options(nostack, preserves_flags)) };
            a += 64;
        }
        // SAFETY: a fence has no memory effect beyond ordering.
        unsafe { core::arch::asm!("mfence", options(nostack, preserves_flags)) };
    }
    #[cfg(not(target_os = "none"))]
    let _ = (virt, len);
}
