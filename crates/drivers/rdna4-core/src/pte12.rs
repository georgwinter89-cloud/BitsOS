//! Wave 42: GFX 12's page-table entry as the GART uses it. GFX 12 moves bits against GFX 11 (`amdgpu-fmt`'s `pte`):
//! the memory type sits at 55:54, PRT at 56, and bit 63 says "this is a PTE" (A:amdgpu_vm.h:57-68 for the low
//! flags, :121-140 for gfx12). The GART is VMID 0's flat table: one 8-byte entry per 4 KiB page
//! (A:amdgpu_gart.c:354-374, amdgpu_gmc.c:163-178).

pub const VALID: u64 = 1 << 0;
pub const SYSTEM: u64 = 1 << 1;
pub const SNOOPED: u64 = 1 << 2;
pub const EXECUTABLE: u64 = 1 << 4;
pub const READABLE: u64 = 1 << 5;
pub const WRITEABLE: u64 = 1 << 6;
pub const FRAG_SHIFT: u32 = 7;
pub const FRAG_MASK: u64 = 0x1F << FRAG_SHIFT;
pub const MTYPE_SHIFT: u32 = 54;
pub const MTYPE_MASK: u64 = 3 << MTYPE_SHIFT;
pub const PRT: u64 = 1 << 56;
pub const IS_PTE: u64 = 1 << 63;

/// `adev->gmc.pte_addr_mask` (A:gmc_v12_0.c:911): page address bits 47:12.
pub const ADDR_MASK: u64 = 0x0000_FFFF_FFFF_F000;
pub const PAGE: u64 = 4096;
/// `MTYPE_UC`.
pub const MTYPE_UC: u64 = 3;

/// `adev->gart.gart_pte_flags` (A:gmc_v12_0.c:791-793): uncached, executable, a PTE.
pub const GART_FLAGS: u64 = (MTYPE_UC << MTYPE_SHIFT) | EXECUTABLE | IS_PTE;
/// What a bound system page carries (`amdgpu_ttm_tt_pte_flags`, A:amdgpu_ttm.c:1439-1484, for a cached kernel
/// buffer in the GTT domain): valid, system, snooped, readable, writeable on top of the GART's flags.
pub const BOUND_FLAGS: u64 = VALID | SYSTEM | SNOOPED | READABLE | WRITEABLE | GART_FLAGS;
/// Linux fills a fresh table with `memset_io(ptr, gart_pte_flags, size)` (A:amdgpu_gart.c:278): the flags' low
/// byte in every byte, so each entry starts as 0x1010101010101010, which is not valid.
pub const FILL_BYTE: u8 = GART_FLAGS as u8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PteError {
    /// Not on a 4 KiB boundary.
    Unaligned(u64),
    /// The page does not lie below the card's DMA ceiling (44 bits, `layout::DMA_BITS`).
    PastCeiling(u64),
}

/// `amdgpu_gmc_set_pte_pde`'s value.
pub const fn entry(addr: u64, flags: u64) -> u64 {
    (addr & ADDR_MASK) | flags
}

/// The entry of a bound system page, refused when the card could not reach the page.
pub fn bound(page: u64) -> Result<u64, PteError> {
    if !page.is_multiple_of(PAGE) {
        return Err(PteError::Unaligned(page));
    }
    if !crate::layout::dma_reachable(page, PAGE) {
        return Err(PteError::PastCeiling(page));
    }
    Ok(entry(page, BOUND_FLAGS))
}

/// The entry of an unbound page: the dummy page with no flag ("system bit must be 0 to mean invalid",
/// A:amdgpu_gart.c:314-315).
pub const fn unbound(dummy: u64) -> u64 {
    entry(dummy, 0)
}

/// An entry taken apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Decoded {
    pub addr: u64,
    pub valid: bool,
    pub system: bool,
    pub snooped: bool,
    pub executable: bool,
    pub readable: bool,
    pub writeable: bool,
    pub frag: u8,
    pub mtype: u8,
    pub prt: bool,
    pub is_pte: bool,
}

pub const fn decode(e: u64) -> Decoded {
    Decoded {
        addr: e & ADDR_MASK,
        valid: e & VALID != 0,
        system: e & SYSTEM != 0,
        snooped: e & SNOOPED != 0,
        executable: e & EXECUTABLE != 0,
        readable: e & READABLE != 0,
        writeable: e & WRITEABLE != 0,
        frag: ((e & FRAG_MASK) >> FRAG_SHIFT) as u8,
        mtype: ((e & MTYPE_MASK) >> MTYPE_SHIFT) as u8,
        prt: e & PRT != 0,
        is_pte: e & IS_PTE != 0,
    }
}
