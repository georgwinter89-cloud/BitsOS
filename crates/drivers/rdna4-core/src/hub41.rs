//! Piece 10: GMC 12's memory layout and the card's two memory hubs, MMHUB 4.1.0 and GFXHUB 12.0, as ONE hub with
//! two register sets (`MM`, `GC`). Linux keeps two files that differ in the registers' places and in three
//! details, which a set carries with it: the default behind L2_CNTL3, what the fault control's function does to
//! the two crash bits, and whether an invalidation takes the engine's semaphore and ends in the private
//! invalidation (MMHUB only). The sequences over a set are `hubseq`'s (re-exported here), the stage carries them
//! out and a test holds them against Linux's trace.
//! A: `gmc_v12_0.c` (mc_init :726-768, vram_gtt_location :688-721, gart_enable :1002-1035), `mmhub_v4_1_0.c`
//! and `gfxhub_v12_0.c` (gart_enable and what it calls), `amdgpu_gmc.c` (vram_location :214-238, gart_location
//! :303-345). The MMHUB set under its old names (`FB_OFFSET`, `CONTEXT0_CNTL`, ...) is wave 42's.

use crate::ipfacts::hw;
use crate::nbreg::Field;
use crate::regmap::Reg;

pub use crate::hubseq::*;

/// Which of the card's two hubs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    /// `AMDGPU_GFXHUB(0)`: bit 0 of `vmhubs_mask`, so a flush of all hubs takes it first (gmc_v12_0.c:834).
    Gc,
    /// `AMDGPU_MMHUB0(0)`: bit 8.
    Mm,
}

/// One hub's registers by name, and what differs between the two hubs besides the places.
#[derive(Clone, Copy, Debug)]
pub struct Hub {
    pub kind: Kind,
    /// Linux's word for the hub in a fault's line (gmc_v12_0.c:139).
    pub name: &'static str,
    pub fb_offset: Reg,
    pub default_addr_lsb: Reg,
    pub default_addr_msb: Reg,
    pub l2_cntl: Reg,
    pub l2_cntl2: Reg,
    pub l2_cntl3: Reg,
    pub fault_cntl: Reg,
    pub fault_cntl2: Reg,
    pub fault_status: Reg,
    pub fault_default_lo: Reg,
    pub fault_default_hi: Reg,
    /// LOW_ADDR_LO32; HI32, HIGH_ADDR_LO32, HI32 and the two PHYSICAL_OFFSET registers follow it one dword apart.
    pub identity_aperture: Reg,
    pub l2_cntl4: Reg,
    /// Bit 25 is the private invalidation after a flush, which Linux issues on MMHUB only (gmc_v12_0.c:271-285).
    pub cid2: Option<Reg>,
    pub l2_cntl5: Reg,
    pub agp_top: Reg,
    pub agp_bot: Reg,
    pub agp_base: Reg,
    pub aperture_low: Reg,
    pub aperture_high: Reg,
    pub l1_tlb: Reg,
    pub context0_cntl: Reg,
    /// Contexts 1 to 15 lie one dword apart (`ctx_distance`).
    pub context1_cntl: Reg,
    /// The engines lie one dword apart (`eng_distance`).
    pub eng0_sem: Reg,
    pub eng0_req: Reg,
    pub eng0_ack: Reg,
    /// LO32; HI32 follows; the engines lie two dwords apart (`eng_addr_distance`).
    pub eng0_range_lo: Reg,
    pub ctx0_base_lo: Reg,
    pub ctx0_base_hi: Reg,
    pub ctx0_start_lo: Reg,
    /// LO32; HI32 follows; the contexts lie two dwords apart (`ctx_addr_distance`).
    pub ctx1_start_lo: Reg,
    pub ctx0_end_lo: Reg,
    pub ctx1_end_lo: Reg,
    /// `regMMVM_L2_CNTL3_DEFAULT` 0x80100007 (mmhub_v4_1_0.c:33), `regGCVM_L2_CNTL3_DEFAULT` 0x80120007
    /// (gfxhub_v12_0.c:32); with Linux's two fields set both give 0x80130009.
    pub l2_cntl3_default: u32,
    /// The hub's `set_fault_enable_default(adev, true)` on its fault control.
    pub fault_enable: fn(u32) -> u32,
    /// What that function changes before the fault control: GFXHUB's sets CPG_UTCL1_ERROR_HALT_DISABLE in
    /// CP_DEBUG first, "NO halt CP when page fault" (gfxhub_v12_0.c:423-426).
    pub before_fault_enable: Option<(Reg, fn(u32) -> u32)>,
    /// `gmc_v12_0_use_invalidate_semaphore` (gmc_v12_0.c:189-194): MMHUB only.
    pub semaphore: bool,
}

const fn mm(name: &'static str, offset: u32) -> Reg {
    Reg { name, hw_id: hw::MMHUB, base_index: 0, offset }
}

const fn gc(name: &'static str, offset: u32) -> Reg {
    Reg { name, hw_id: hw::GC, base_index: 0, offset }
}

/// mmhub_4_1_0_offset.h, all base index 0.
pub const MM: Hub = Hub {
    kind: Kind::Mm,
    name: "mmhub",
    fb_offset: mm("MMMC_VM_FB_OFFSET", 0x4C7),
    default_addr_lsb: mm("MMMC_VM_SYSTEM_APERTURE_DEFAULT_ADDR_LSB", 0x4C8),
    default_addr_msb: mm("MMMC_VM_SYSTEM_APERTURE_DEFAULT_ADDR_MSB", 0x4C9),
    l2_cntl: mm("MMVM_L2_CNTL", 0x4E4),
    l2_cntl2: mm("MMVM_L2_CNTL2", 0x4E5),
    l2_cntl3: mm("MMVM_L2_CNTL3", 0x4E6),
    fault_cntl: mm("MMVM_L2_PROTECTION_FAULT_CNTL", 0x4EC),
    fault_cntl2: mm("MMVM_L2_PROTECTION_FAULT_CNTL2", 0x4ED),
    fault_status: mm("MMVM_L2_PROTECTION_FAULT_STATUS_LO32", 0x4F0),
    fault_default_lo: mm("MMVM_L2_PROTECTION_FAULT_DEFAULT_ADDR_LO32", 0x4F4),
    fault_default_hi: mm("MMVM_L2_PROTECTION_FAULT_DEFAULT_ADDR_HI32", 0x4F5),
    identity_aperture: mm("MMVM_L2_CONTEXT1_IDENTITY_APERTURE_LOW_ADDR_LO32", 0x4F7),
    l2_cntl4: mm("MMVM_L2_CNTL4", 0x4FD),
    cid2: Some(mm("MMVM_L2_BANK_SELECT_RESERVED_CID2", 0x500)),
    l2_cntl5: mm("MMVM_L2_CNTL5", 0x503),
    agp_top: mm("MMMC_VM_AGP_TOP", 0x556),
    agp_bot: mm("MMMC_VM_AGP_BOT", 0x557),
    agp_base: mm("MMMC_VM_AGP_BASE", 0x558),
    aperture_low: mm("MMMC_VM_SYSTEM_APERTURE_LOW_ADDR", 0x559),
    aperture_high: mm("MMMC_VM_SYSTEM_APERTURE_HIGH_ADDR", 0x55A),
    l1_tlb: mm("MMMC_VM_MX_L1_TLB_CNTL", 0x55B),
    context0_cntl: mm("MMVM_CONTEXT0_CNTL", 0x564),
    context1_cntl: mm("MMVM_CONTEXT1_CNTL", 0x565),
    eng0_sem: mm("MMVM_INVALIDATE_ENG0_SEM", 0x575),
    eng0_req: mm("MMVM_INVALIDATE_ENG0_REQ", 0x587),
    eng0_ack: mm("MMVM_INVALIDATE_ENG0_ACK", 0x599),
    eng0_range_lo: mm("MMVM_INVALIDATE_ENG0_ADDR_RANGE_LO32", 0x5AB),
    ctx0_base_lo: mm("MMVM_CONTEXT0_PAGE_TABLE_BASE_ADDR_LO32", 0x5CF),
    ctx0_base_hi: mm("MMVM_CONTEXT0_PAGE_TABLE_BASE_ADDR_HI32", 0x5D0),
    ctx0_start_lo: mm("MMVM_CONTEXT0_PAGE_TABLE_START_ADDR_LO32", 0x5EF),
    ctx1_start_lo: mm("MMVM_CONTEXT1_PAGE_TABLE_START_ADDR_LO32", 0x5F1),
    ctx0_end_lo: mm("MMVM_CONTEXT0_PAGE_TABLE_END_ADDR_LO32", 0x60F),
    ctx1_end_lo: mm("MMVM_CONTEXT1_PAGE_TABLE_END_ADDR_LO32", 0x611),
    l2_cntl3_default: L2_CNTL3_DEFAULT,
    fault_enable: fault_cntl,
    before_fault_enable: None,
    semaphore: true,
};

/// gc_12_0_0_offset.h, all base index 0. The places are no constant shift of MMHUB's (FB_OFFSET 0x15A7 against
/// 0x4C7, CONTEXT0_CNTL 0x1624 against 0x564), so every register is named.
pub const GC: Hub = Hub {
    kind: Kind::Gc,
    name: "gfxhub",
    fb_offset: gc("GCMC_VM_FB_OFFSET", 0x15A7),
    default_addr_lsb: gc("GCMC_VM_SYSTEM_APERTURE_DEFAULT_ADDR_LSB", 0x15A8),
    default_addr_msb: gc("GCMC_VM_SYSTEM_APERTURE_DEFAULT_ADDR_MSB", 0x15A9),
    l2_cntl: gc("GCVM_L2_CNTL", 0x15C4),
    l2_cntl2: gc("GCVM_L2_CNTL2", 0x15C5),
    l2_cntl3: gc("GCVM_L2_CNTL3", 0x15C6),
    fault_cntl: gc("GCVM_L2_PROTECTION_FAULT_CNTL", 0x15CC),
    fault_cntl2: gc("GCVM_L2_PROTECTION_FAULT_CNTL2", 0x15CD),
    fault_status: gc("GCVM_L2_PROTECTION_FAULT_STATUS_LO32", 0x15D0),
    fault_default_lo: gc("GCVM_L2_PROTECTION_FAULT_DEFAULT_ADDR_LO32", 0x15D4),
    fault_default_hi: gc("GCVM_L2_PROTECTION_FAULT_DEFAULT_ADDR_HI32", 0x15D5),
    identity_aperture: gc("GCVM_L2_CONTEXT1_IDENTITY_APERTURE_LOW_ADDR_LO32", 0x15D7),
    l2_cntl4: gc("GCVM_L2_CNTL4", 0x15DD),
    cid2: None,
    l2_cntl5: gc("GCVM_L2_CNTL5", 0x15E3),
    agp_top: gc("GCMC_VM_AGP_TOP", 0x1616),
    agp_bot: gc("GCMC_VM_AGP_BOT", 0x1617),
    agp_base: gc("GCMC_VM_AGP_BASE", 0x1618),
    aperture_low: gc("GCMC_VM_SYSTEM_APERTURE_LOW_ADDR", 0x1619),
    aperture_high: gc("GCMC_VM_SYSTEM_APERTURE_HIGH_ADDR", 0x161A),
    l1_tlb: gc("GCMC_VM_MX_L1_TLB_CNTL", 0x161B),
    context0_cntl: gc("GCVM_CONTEXT0_CNTL", 0x1624),
    context1_cntl: gc("GCVM_CONTEXT1_CNTL", 0x1625),
    eng0_sem: gc("GCVM_INVALIDATE_ENG0_SEM", 0x1635),
    eng0_req: gc("GCVM_INVALIDATE_ENG0_REQ", 0x1647),
    eng0_ack: gc("GCVM_INVALIDATE_ENG0_ACK", 0x1659),
    eng0_range_lo: gc("GCVM_INVALIDATE_ENG0_ADDR_RANGE_LO32", 0x166B),
    ctx0_base_lo: gc("GCVM_CONTEXT0_PAGE_TABLE_BASE_ADDR_LO32", 0x168F),
    ctx0_base_hi: gc("GCVM_CONTEXT0_PAGE_TABLE_BASE_ADDR_HI32", 0x1690),
    ctx0_start_lo: gc("GCVM_CONTEXT0_PAGE_TABLE_START_ADDR_LO32", 0x16AF),
    ctx1_start_lo: gc("GCVM_CONTEXT1_PAGE_TABLE_START_ADDR_LO32", 0x16B1),
    ctx0_end_lo: gc("GCVM_CONTEXT0_PAGE_TABLE_END_ADDR_LO32", 0x16CF),
    ctx1_end_lo: gc("GCVM_CONTEXT1_PAGE_TABLE_END_ADDR_LO32", 0x16D1),
    l2_cntl3_default: 0x8012_0007,
    fault_enable: gc_fault_cntl,
    before_fault_enable: Some((crate::gc12::CP_DEBUG, crate::gc12::cp_debug)),
    semaphore: false,
};

/// A hub's register set.
pub const fn of(kind: Kind) -> &'static Hub {
    match kind {
        Kind::Gc => &GC,
        Kind::Mm => &MM,
    }
}

// MMHUB's registers under the names wave 42 gave them.
pub const FB_OFFSET: Reg = MM.fb_offset;
pub const SYSTEM_APERTURE_DEFAULT_ADDR_LSB: Reg = MM.default_addr_lsb;
pub const SYSTEM_APERTURE_DEFAULT_ADDR_MSB: Reg = MM.default_addr_msb;
pub const L2_CNTL: Reg = MM.l2_cntl;
pub const L2_CNTL2: Reg = MM.l2_cntl2;
pub const L2_CNTL3: Reg = MM.l2_cntl3;
pub const FAULT_CNTL: Reg = MM.fault_cntl;
pub const FAULT_CNTL2: Reg = MM.fault_cntl2;
pub const FAULT_DEFAULT_ADDR_LO32: Reg = MM.fault_default_lo;
pub const FAULT_DEFAULT_ADDR_HI32: Reg = MM.fault_default_hi;
pub const IDENTITY_APERTURE: Reg = MM.identity_aperture;
pub const L2_CNTL4: Reg = MM.l2_cntl4;
pub const BANK_SELECT_RESERVED_CID2: Reg = mm("MMVM_L2_BANK_SELECT_RESERVED_CID2", 0x500);
pub const L2_CNTL5: Reg = MM.l2_cntl5;
pub const AGP_TOP: Reg = MM.agp_top;
pub const AGP_BOT: Reg = MM.agp_bot;
pub const AGP_BASE: Reg = MM.agp_base;
pub const SYSTEM_APERTURE_LOW_ADDR: Reg = MM.aperture_low;
pub const SYSTEM_APERTURE_HIGH_ADDR: Reg = MM.aperture_high;
pub const MX_L1_TLB_CNTL: Reg = MM.l1_tlb;
pub const CONTEXT0_CNTL: Reg = MM.context0_cntl;
pub const CONTEXT1_CNTL: Reg = MM.context1_cntl;
pub const INVALIDATE_ENG0_SEM: Reg = MM.eng0_sem;
pub const INVALIDATE_ENG0_REQ: Reg = MM.eng0_req;
pub const INVALIDATE_ENG0_ACK: Reg = MM.eng0_ack;
pub const INVALIDATE_ENG0_ADDR_RANGE_LO32: Reg = MM.eng0_range_lo;
pub const CONTEXT0_PAGE_TABLE_BASE_ADDR_LO32: Reg = MM.ctx0_base_lo;
pub const CONTEXT0_PAGE_TABLE_BASE_ADDR_HI32: Reg = MM.ctx0_base_hi;
pub const CONTEXT0_PAGE_TABLE_START_ADDR_LO32: Reg = MM.ctx0_start_lo;
pub const CONTEXT1_PAGE_TABLE_START_ADDR_LO32: Reg = MM.ctx1_start_lo;
pub const CONTEXT0_PAGE_TABLE_END_ADDR_LO32: Reg = MM.ctx0_end_lo;
pub const CONTEXT1_PAGE_TABLE_END_ADDR_LO32: Reg = MM.ctx1_end_lo;

/// Linux uses engine 17 for the GART (gmc_v12_0.c:221) and programs eighteen ranges (mmhub_v4_1_0.c:360).
pub const GART_ENGINE: u32 = 17;
pub const ENGINES: u32 = 18;
/// `mmhub_v4_1_0_setup_vmid_config` walks contexts 1 to 15 (:311).
pub const USER_CONTEXTS: u32 = 15;

/// mmhub_4_1_0_sh_mask.h; gc_12_0_0_sh_mask.h has every one of them at the same bits for GFXHUB's registers
/// (`rdna4-regs`' test holds both).
pub mod f {
    use super::Field;
    pub const ENABLE_L1_TLB: Field = Field::new(0, 0x0000_0001);
    pub const SYSTEM_ACCESS_MODE: Field = Field::new(3, 0x0000_0018);
    pub const SYSTEM_APERTURE_UNMAPPED_ACCESS: Field = Field::new(5, 0x0000_0020);
    pub const ENABLE_ADVANCED_DRIVER_MODEL: Field = Field::new(6, 0x0000_0040);
    pub const ECO_BITS: Field = Field::new(7, 0x0000_0780);
    pub const L1_MTYPE: Field = Field::new(11, 0x0000_1800);
    pub const ENABLE_L2_CACHE: Field = Field::new(0, 0x0000_0001);
    pub const ENABLE_L2_FRAGMENT_PROCESSING: Field = Field::new(1, 0x0000_0002);
    pub const L2_PDE0_CACHE_TAG_GENERATION_MODE: Field = Field::new(8, 0x0000_0100);
    pub const ENABLE_DEFAULT_PAGE_OUT_TO_SYSTEM_MEMORY: Field = Field::new(11, 0x0000_0800);
    pub const PDE_FAULT_CLASSIFICATION: Field = Field::new(18, 0x0004_0000);
    pub const CONTEXT1_IDENTITY_ACCESS_MODE: Field = Field::new(19, 0x0018_0000);
    pub const IDENTITY_MODE_FRAGMENT_SIZE: Field = Field::new(21, 0x03E0_0000);
    pub const INVALIDATE_ALL_L1_TLBS: Field = Field::new(0, 0x0000_0001);
    pub const INVALIDATE_L2_CACHE: Field = Field::new(1, 0x0000_0002);
    pub const BANK_SELECT: Field = Field::new(0, 0x0000_003F);
    pub const L2_CACHE_BIGK_FRAGMENT_SIZE: Field = Field::new(15, 0x000F_8000);
    pub const VMC_TAP_PDE_REQUEST_PHYSICAL: Field = Field::new(6, 0x0000_0040);
    pub const VMC_TAP_PTE_REQUEST_PHYSICAL: Field = Field::new(7, 0x0000_0080);
    pub const L2_CACHE_SMALLK_FRAGMENT_SIZE: Field = Field::new(0, 0x0000_001F);
    // MMVM_CONTEXT0_CNTL and MMVM_CONTEXT1_CNTL share these.
    pub const ENABLE_CONTEXT: Field = Field::new(0, 0x0000_0001);
    pub const PAGE_TABLE_DEPTH: Field = Field::new(1, 0x0000_0006);
    pub const PAGE_TABLE_BLOCK_SIZE: Field = Field::new(4, 0x0000_00F0);
    pub const RETRY_PERMISSION_OR_INVALID_PAGE_FAULT: Field = Field::new(8, 0x0000_0100);
    /// RANGE, DUMMY_PAGE, PDE0, VALID, READ, WRITE and EXECUTE `_PROTECTION_FAULT_ENABLE_DEFAULT` of a context.
    pub const CONTEXT_FAULT_DEFAULTS: u32 = 0x0000_0800 | 0x0000_2000 | 0x0000_8000 | 0x0002_0000 | 0x0008_0000 | 0x0020_0000 | 0x0080_0000;
    /// The eleven `_PROTECTION_FAULT_ENABLE_DEFAULT` bits of MMVM_L2_PROTECTION_FAULT_CNTL (bits 2 to 12).
    pub const FAULT_DEFAULTS: u32 = 0x0000_1FFC;
    pub const ACTIVE_PAGE_MIGRATION_PTE_READ_RETRY: Field = Field::new(18, 0x0004_0000);
    pub const PER_VMID_INVALIDATE_REQ: Field = Field::new(0, 0x0000_FFFF);
    pub const FLUSH_TYPE: Field = Field::new(16, 0x0007_0000);
    pub const INVALIDATE_L2_PTES: Field = Field::new(19, 0x0008_0000);
    pub const INVALIDATE_L2_PDE0: Field = Field::new(20, 0x0010_0000);
    pub const INVALIDATE_L2_PDE1: Field = Field::new(21, 0x0020_0000);
    pub const INVALIDATE_L2_PDE2: Field = Field::new(22, 0x0040_0000);
    pub const INVALIDATE_L1_PTES: Field = Field::new(23, 0x0080_0000);
    pub const CLEAR_PROTECTION_FAULT_STATUS_ADDR: Field = Field::new(24, 0x0100_0000);
    /// The hub's L2_PROTECTION_FAULT_CNTL: bit 0 clears the fault's status and address (gmc_v12_0.c:130), bits
    /// 30 and 31 make the card crash on a fault.
    pub const CLEAR_FAULT_STATUS: Field = Field::new(0, 0x0000_0001);
    pub const CRASH_ON_NO_RETRY_FAULT: Field = Field::new(30, 0x4000_0000);
    pub const CRASH_ON_RETRY_FAULT: Field = Field::new(31, 0x8000_0000);
}

/// mmhub_v4_1_0.c:33-35; GFXHUB's CNTL3 default differs (`GC.l2_cntl3_default`), its CNTL4 and CNTL5 do not
/// (gfxhub_v12_0.c:32-34).
pub const L2_CNTL3_DEFAULT: u32 = 0x8010_0007;
pub const L2_CNTL4_DEFAULT: u32 = 0x0000_00C1;
pub const L2_CNTL5_DEFAULT: u32 = 0x0000_3FE0;
/// `MTYPE_UC` (include/soc24_enum.h): uncached.
pub const MTYPE_UC: u32 = 3;
/// VM of 256 TB with 9-bit blocks and four levels (gmc_v12_0.c:839): `num_level` 3, `block_size` 9, `max_pfn`
/// 1 << 36.
pub const NUM_LEVEL: u32 = 3;
pub const BLOCK_SIZE: u32 = 9;
pub const MAX_PFN: u64 = 1 << 36;
/// Bit 25 of MMVM_L2_BANK_SELECT_RESERVED_CID2: the private invalidation after a flush (gmc_v12_0.c:278-284).
pub const RESERVED_CACHE_PRIVATE_INVALIDATION: u32 = 1 << 25;
/// `AMDGPU_GMC_HOLE_START` for a VM of four levels (A:amdgpu_gmc.h:36-37, `max_level` 3 from gmc_v12_0.c:841): Linux
/// keeps every MC address below it.
pub const MC_HOLE_START: u64 = 0x0000_8000_0000_0000;
const FOUR_GB: u64 = 1 << 32;

/// Where the card's memory lies in its own address space, as GMC 12 computes it from what the card says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout12 {
    /// MC address of VRAM offset 0 and VRAM's size (`mc_vram_size` = the memory size register in MiB).
    pub vram_start: u64,
    pub vram_bytes: u64,
    /// `vm_manager.vram_base_offset`: what a VRAM offset is counted from in a page-table base (0 on this card).
    pub base_offset: u64,
    pub gart_start: u64,
    pub gart_bytes: u64,
    /// The AGP aperture's default, which is "none" (amdgpu_gmc.c:398-404).
    pub agp_start: u64,
    pub agp_end: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayoutStop {
    /// The memory size register reads 0 or all ones.
    Memsize(u32),
    /// MMMC_VM_FB_LOCATION_BASE or MMMC_VM_FB_OFFSET reads all ones.
    Hub { base: u32, offset: u32 },
    /// VRAM would reach into the hole of the card's address space.
    VramPastHole { end: u64 },
    /// 512 MiB of GART fit neither below nor above VRAM (Linux would shrink the GART; BitsOS stops).
    NoGart,
}

impl Layout12 {
    pub const fn vram_end(&self) -> u64 {
        self.vram_start + self.vram_bytes - 1
    }
    pub const fn gart_end(&self) -> u64 {
        self.gart_start + self.gart_bytes - 1
    }
    /// The MC address of a VRAM offset.
    pub const fn mc(&self, offset: u64) -> u64 {
        self.vram_start + offset
    }
}

/// `gmc_v12_0_mc_init` with `gmc_v12_0_vram_gtt_location`, from the three registers Linux reads (trace lines 18-20).
pub fn layout(memsize_mib: u32, fb_location_base: u32, fb_offset: u32) -> Result<Layout12, LayoutStop> {
    let vram_bytes = crate::layout::vram_bytes(memsize_mib).map_err(|_| LayoutStop::Memsize(memsize_mib))?;
    if fb_location_base == u32::MAX || fb_offset == u32::MAX {
        return Err(LayoutStop::Hub { base: fb_location_base, offset: fb_offset });
    }
    let vram_start = u64::from(fb_location_base & crate::regmap::FB_LOCATION_MASK) << crate::regmap::FB_LOCATION_SHIFT;
    let end = vram_start + vram_bytes - 1;
    if end >= MC_HOLE_START {
        return Err(LayoutStop::VramPastHole { end });
    }
    // AMDGPU_GART_PLACEMENT_LOW: at 0 when the GART fits below VRAM, else at the next 4 GiB above it.
    let gart_bytes = crate::layout::GART_BYTES;
    let after = MC_HOLE_START - (end + 1).next_multiple_of(FOUR_GB);
    if gart_bytes > vram_start.max(after) {
        return Err(LayoutStop::NoGart);
    }
    let gart_start = if vram_start >= gart_bytes { 0 } else { end.next_multiple_of(FOUR_GB) } & !(FOUR_GB - 1);
    Ok(Layout12 { vram_start, vram_bytes, base_offset: u64::from(fb_offset) << 24, gart_start, gart_bytes, agp_start: 0xFFFF_FFFF_FFFF, agp_end: 0 })
}
