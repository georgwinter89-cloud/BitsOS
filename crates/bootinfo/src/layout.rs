//! Virtual address layout shared by bitsboot and the kernel (V2-PLAN §2.3). Changing a value is a desk change.

/// Kernel image (code model "kernel": the top 2 GB).
pub const KERNEL_BASE: u64 = 0xFFFF_FFFF_8000_0000;
/// Direct map of all physical RAM (as Linux): `virt = DIRECT_MAP_BASE + phys`.
pub const DIRECT_MAP_BASE: u64 = 0xFFFF_8880_0000_0000;
/// vmap / MMIO area with explicit PAT types. bitsboot maps the GOP framebuffer at its start.
pub const VMAP_BASE: u64 = 0xFFFF_C900_0000_0000;
/// End (exclusive) of the vmap area.
pub const VMAP_END: u64 = 0xFFFF_E900_0000_0000;
/// The CPU area: the top 64 GiB of vmap, handed out top-down for per-CPU IST stacks with guard pages (hal::gdt).
/// MMIO mappings grow up from VMAP_BASE and must stay below it.
pub const CPU_AREA_BASE: u64 = VMAP_END - (64 << 30);
/// Highest user address + 1 page: user space is `0 .. USER_TOP`.
pub const USER_TOP: u64 = 0x0000_7FFF_FFFF_F000;
pub const PAGE_SIZE: u64 = 4096;

/// Physical -> direct-map virtual.
pub const fn phys_to_virt(phys: u64) -> u64 {
    DIRECT_MAP_BASE + phys
}

/// Direct-map virtual -> physical, if the address is inside the direct map.
pub const fn virt_to_phys(virt: u64) -> Option<u64> {
    if virt >= DIRECT_MAP_BASE && virt < VMAP_BASE { Some(virt - DIRECT_MAP_BASE) } else { None }
}
