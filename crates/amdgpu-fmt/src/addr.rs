//! GPU addresses: the 48-bit VM space with its canonical hole, and the carve-out's two addresses.

use crate::GPU_PAGE;

/// Bits of a GPU virtual address (gmc_v11_0.c:791: 256 TB, 4 levels).
pub const VA_BITS: u32 = 48;
/// Size of the VM space in bytes.
pub const VA_SIZE: u64 = 1 << VA_BITS;
/// First address of the hole between the two canonical halves (amdgpu_gmc.h:35).
pub const HOLE_START: u64 = 0x0000_8000_0000_0000;
/// First address after the hole (amdgpu_gmc.h:36).
pub const HOLE_END: u64 = 0xffff_8000_0000_0000;
/// Mask from a canonical address to the page-table index space (amdgpu_gmc.h:45).
pub const HOLE_MASK: u64 = 0x0000_ffff_ffff_ffff;

/// A GPU VA as the page tables see it: refuses addresses in the hole (amdgpu_gem.c VA ioctl, -EINVAL), keeps
/// the low 48 bits of upper-half addresses.
pub fn va_index(va: u64) -> Option<u64> {
    if (HOLE_START..HOLE_END).contains(&va) {
        return None;
    }
    Some(va & HOLE_MASK)
}

/// VA kept free at the bottom and at the top of a user VM (amdgpu_vm.h:132, 2 MiB in Linux 6.6): the kernel
/// maps its own buffers (CSA and the like) at the top, and RADV/libdrm assume both ends are unusable.
pub const VA_RESERVED_SIZE: u64 = 2 << 20;

/// The user VA ioctl's range check (amdgpu_gem.c:685-710): refuses a VA below [`VA_RESERVED_SIZE`], in the
/// hole, or a range running into the top reserved area of a VM of `vm_size` bytes (`VA_SIZE` on GFX 11);
/// returns the index-space start. [`crate::vm::Vm`] itself only enforces the 48-bit end like
/// amdgpu_vm_bo_map (amdgpu_vm.c:1538), because the kernel maps into the reserved ranges; DR calls this
/// before every user map, unmap or clear. Overflow is refused where Linux's sum would wrap.
pub fn user_range(va: u64, len: u64, vm_size: u64) -> Option<u64> {
    if va < VA_RESERVED_SIZE {
        return None;
    }
    let idx = va_index(va)?;
    let end = idx.checked_add(len)?;
    (end <= vm_size.checked_sub(VA_RESERVED_SIZE)?).then_some(idx)
}

/// The canonical (sign-extended) form of a page-table address (amdgpu_gmc.h:372 amdgpu_gmc_sign_extend).
pub fn sign_extend(addr: u64) -> u64 {
    if addr >= HOLE_START { addr | HOLE_END } else { addr }
}

/// The carve-out ("VRAM" of an APU, or a dGPU's VRAM later) in both address spaces: `mc_base` is what
/// registers and ring packets use (0x80_0000_0000 on the 780M), `phys_base` is what page-table entries use
/// (FB_OFFSET << 24 on an APU, gmc_v11_0.c:499 adds it as vram_base_offset). Any size: it comes from MEMSIZE.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Carveout {
    mc_base: u64,
    phys_base: u64,
    size: u64,
}

impl Carveout {
    /// `None` for size 0, unaligned bases or size, or a range that leaves 48 bits.
    pub fn new(mc_base: u64, phys_base: u64, size: u64) -> Option<Carveout> {
        let aligned = |v: u64| v.is_multiple_of(GPU_PAGE);
        if size == 0 || !aligned(mc_base) || !aligned(phys_base) || !aligned(size) {
            return None;
        }
        let mc_end = mc_base.checked_add(size)?;
        let phys_end = phys_base.checked_add(size)?;
        if mc_end > VA_SIZE || phys_end > VA_SIZE {
            return None;
        }
        Some(Carveout { mc_base, phys_base, size })
    }

    pub fn size(&self) -> u64 {
        self.size
    }
    pub fn mc_base(&self) -> u64 {
        self.mc_base
    }
    pub fn phys_base(&self) -> u64 {
        self.phys_base
    }

    /// Host-physical address of a byte offset into the carve-out.
    pub fn phys_of(&self, offset: u64) -> Option<u64> {
        (offset < self.size).then(|| self.phys_base + offset)
    }

    /// Host-physical address of an MC address inside the carve-out (gmc_v11_0.c:499).
    pub fn mc_to_phys(&self, mc: u64) -> Option<u64> {
        self.phys_of(mc.checked_sub(self.mc_base)?)
    }

    /// MC address of a host-physical address inside the carve-out.
    pub fn phys_to_mc(&self, phys: u64) -> Option<u64> {
        let off = phys.checked_sub(self.phys_base)?;
        (off < self.size).then(|| self.mc_base + off)
    }

    /// True when `[phys, phys + len)` lies inside the carve-out.
    pub fn contains_phys(&self, phys: u64, len: u64) -> bool {
        match (phys.checked_sub(self.phys_base), phys.checked_add(len)) {
            (Some(off), Some(_)) => off <= self.size && len <= self.size - off,
            _ => false,
        }
    }
}
