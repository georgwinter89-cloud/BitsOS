//! Memory controller side of GC 11 (Linux `gmc_v11_0`, linux-6.6 amdgpu/gmc_v11_0.c) with its two hubs: GFXHUB
//! 3.0 (Linux `gfxhub_v3_0`, inside GC) and MMHUB 3.0.1 (Linux `mmhub_v3_0_1`). GART, VM contexts, the carve-out
//! (on APUs its CPU address is MMHUB's FB offset, gmc_v11_0.c:720-723). GX-7 (GART) and GX-8 (VM per fd).
//!
//! GX-7h: both hubs' registers by name (`R`, as (segment, dword) of GC 11.0.0 / MMHUB 3.0.1:
//! gc_11_0_0_offset.h, mmhub_3_0_1_offset.h), the values Linux's gart_enable gives them (gfxhub_v3_0.c:119-352,
//! mmhub_v3_0_1.c:137-384, checked against the 27.10. capture of the 780M) and the L2 protection fault decode
//! (gmc_v11_0.c:96-140, the hubs' print_l2_protection_fault_status). The run is `gpucore::gmc`.

use super::{Firmware, Module, Role, State, v};

const GC11: &[crate::ip::IpVersion] = &[v(11, 0, 0), v(11, 0, 2), v(11, 0, 3), v(11, 0, 4)];

pub static GMC: Module = Module {
    name: "gmc11",
    role: Role::Gmc,
    versions: &[v(11, 0, 1)],
    linux: "gmc_v11_0",
    linux_also: GC11,
    firmware: Firmware::None("no firmware"),
    state: State::Present("gpucore::gmc (GX-7h: GART, VMID 0 on both hubs, TLB flush, fault decode)"),
};

/// Linux picks `gfxhub_v3_0_3` for GC 11.0.3 and `gfxhub_v3_0` for the others (gmc_v11_0.c set_gfxhub_funcs).
pub static GFXHUB: Module = Module {
    name: "gfxhub3_0",
    role: Role::Gfxhub,
    versions: &[v(11, 0, 1)],
    linux: "gfxhub_v3_0",
    linux_also: &[v(11, 0, 0), v(11, 0, 2), v(11, 0, 4)],
    firmware: Firmware::None("no firmware"),
    state: State::Present("gpucore::gmc (GX-7h: programmed as Linux, VMID 0 = GART)"),
};

pub static MMHUB: Module = Module {
    name: "mmhub3_0_1",
    role: Role::Mmhub,
    versions: &[v(3, 0, 1)],
    linux: "mmhub_v3_0_1",
    linux_also: &[],
    firmware: Firmware::None("no firmware"),
    state: State::Present("gpucore::gmc (GX-7h: read, written only where it differs and the display does not use it)"),
};

/// MMMC_VM_FB_OFFSET (segment, dword) in MMHUB 3.0.1's bases (mmhub_3_0_1_offset.h): bits 23:0 << 24 is the
/// carve-out's CPU address on an APU.
pub const MMHUB_FB_OFFSET: (usize, u32) = (1, 0x8d7);

/// GX-7b, read-only facts for the state classifier (mmhub_3_0_1_offset.h, segment 1). The FB location registers
/// hold bits 47:24 of MC addresses; AGP and the system aperture likewise (AGP 47:24, aperture 47:18).
pub const MMHUB_FB_LOCATION_BASE: (usize, u32) = (1, 0x8ec);
pub const MMHUB_FB_LOCATION_TOP: (usize, u32) = (1, 0x8ed);
pub const MMHUB_AGP_TOP: (usize, u32) = (1, 0x8ee);
pub const MMHUB_AGP_BOT: (usize, u32) = (1, 0x8ef);
pub const MMHUB_AGP_BASE: (usize, u32) = (1, 0x8f0);
pub const MMHUB_SYSTEM_APERTURE_LOW: (usize, u32) = (1, 0x8f1);
pub const MMHUB_SYSTEM_APERTURE_HIGH: (usize, u32) = (1, 0x8f2);
/// Bit 0 ENABLE_L1_TLB (the Mini-PC: 0x501 cold, 0x1859 after Linux's gmc init).
pub const MMHUB_MX_L1_TLB_CNTL: (usize, u32) = (1, 0x8f3);
/// Bit 0 ENABLE_L2_CACHE (0x80602 cold, 0x80e01 after Linux).
pub const MMVM_L2_CNTL: (usize, u32) = (1, 0x700);
pub const MMVM_L2_PROTECTION_FAULT_STATUS: (usize, u32) = (1, 0x70c);
/// Bit 0 ENABLE_CONTEXT: VMID 0 (GART) on (0x01fffe00 cold, 0x01fffe01 after Linux).
pub const MMVM_CONTEXT0_CNTL: (usize, u32) = (1, 0x740);

/// GFXHUB 3.0's copies inside GC (gc_11_0_0_offset.h, segment 0): readable only while GC is powered.
pub const GCMC_VM_FB_OFFSET: (usize, u32) = (0, 0x15a7);
pub const GCMC_VM_FB_LOCATION_BASE: (usize, u32) = (0, 0x1678);
pub const GCMC_VM_FB_LOCATION_TOP: (usize, u32) = (0, 0x1679);
pub const GCVM_CONTEXT0_CNTL: (usize, u32) = (0, 0x1688);

// ---- GX-7h: both hubs ----

/// The two VM hubs of GC 11: GFXHUB (inside GC: CP, shaders, SDMA) and MMHUB (VCN, JPEG, DCN, PSP, IH, HDP).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Hub {
    Gfx,
    Mm,
}

impl Hub {
    /// The discovery hardware id and segment that hold the hub's registers.
    pub fn ip(self) -> (u16, usize) {
        match self {
            Hub::Gfx => (crate::gpucore::hwid::GC, 0),
            Hub::Mm => (crate::gpucore::hwid::MMHUB, 1),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Hub::Gfx => "GFXHUB",
            Hub::Mm => "MMHUB",
        }
    }

    /// Register name prefix: GC/MM (GCVM_, GCMC_VM_, MMVM_, MMMC_VM_).
    fn p(self) -> &'static str {
        match self {
            Hub::Gfx => "GC",
            Hub::Mm => "MM",
        }
    }
}

/// VM contexts (VMIDs) per hub and invalidation engines (gfxhub_v3_0.c:291 "i <= 14" contexts 1..15, :340 18).
pub const CONTEXTS: u8 = 16;
pub const ENGINES: u8 = 18;
/// The engine Linux keeps for the CPU's own flushes (GART, unmap): gmc_v11_0.c:202.
pub const CPU_ENGINE: u8 = 17;

/// One hub register. Indexed ones take the context or engine number.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum R {
    FbOffset,
    FbBase,
    FbTop,
    AgpTop,
    AgpBot,
    AgpBase,
    ApertLow,
    ApertHigh,
    DefaultLsb,
    DefaultMsb,
    L1Tlb,
    L2Cntl,
    L2Cntl2,
    L2Cntl3,
    L2Cntl4,
    L2Cntl5,
    FaultCntl,
    FaultCntl2,
    FaultStatus,
    FaultAddrLo,
    FaultAddrHi,
    FaultDefaultLo,
    FaultDefaultHi,
    IdentLowLo,
    IdentLowHi,
    IdentHighLo,
    IdentHighHi,
    IdentOffLo,
    IdentOffHi,
    Ctx(u8),
    PtBaseLo(u8),
    PtBaseHi(u8),
    PtStartLo(u8),
    PtStartHi(u8),
    PtEndLo(u8),
    PtEndHi(u8),
    Sem(u8),
    Req(u8),
    Ack(u8),
    RangeLo(u8),
    RangeHi(u8),
    /// GFXHUB only (GC's CP_DEBUG, set by gfxhub_v3_0_set_fault_enable_default).
    CpDebug,
}

impl R {
    /// The register's dword in the hub's segment (gc_11_0_0_offset.h segment 0, mmhub_3_0_1_offset.h segment 1).
    pub fn dword(self, hub: Hub) -> Option<u32> {
        let g = hub == Hub::Gfx;
        let pick = |gc: u32, mm: u32| Some(if g { gc } else { mm });
        let ix = |n: u8, lim: u8| (n < lim).then_some(u32::from(n));
        match self {
            R::FbOffset => pick(0x15a7, 0x8d7),
            R::DefaultLsb => pick(0x15a8, 0x8d8),
            R::DefaultMsb => pick(0x15a9, 0x8d9),
            R::FbBase => pick(0x1678, 0x8ec),
            R::FbTop => pick(0x1679, 0x8ed),
            R::AgpTop => pick(0x167a, 0x8ee),
            R::AgpBot => pick(0x167b, 0x8ef),
            R::AgpBase => pick(0x167c, 0x8f0),
            R::ApertLow => pick(0x167d, 0x8f1),
            R::ApertHigh => pick(0x167e, 0x8f2),
            R::L1Tlb => pick(0x167f, 0x8f3),
            R::L2Cntl => pick(0x15bc, 0x700),
            R::L2Cntl2 => pick(0x15bd, 0x701),
            R::L2Cntl3 => pick(0x15be, 0x702),
            R::L2Cntl4 => pick(0x15d4, 0x718),
            R::L2Cntl5 => pick(0x15da, 0x71e),
            R::FaultCntl => pick(0x15c4, 0x708),
            R::FaultCntl2 => pick(0x15c5, 0x709),
            R::FaultStatus => pick(0x15c8, 0x70c),
            R::FaultAddrLo => pick(0x15c9, 0x70d),
            R::FaultAddrHi => pick(0x15ca, 0x70e),
            R::FaultDefaultLo => pick(0x15cb, 0x70f),
            R::FaultDefaultHi => pick(0x15cc, 0x710),
            R::IdentLowLo => pick(0x15ce, 0x712),
            R::IdentLowHi => pick(0x15cf, 0x713),
            R::IdentHighLo => pick(0x15d0, 0x714),
            R::IdentHighHi => pick(0x15d1, 0x715),
            R::IdentOffLo => pick(0x15d2, 0x716),
            R::IdentOffHi => pick(0x15d3, 0x717),
            R::Ctx(n) => Some(if g { 0x1688 } else { 0x740 } + ix(n, CONTEXTS)?),
            R::PtBaseLo(n) => Some(if g { 0x16f3 } else { 0x7ab } + 2 * ix(n, CONTEXTS)?),
            R::PtBaseHi(n) => Some(if g { 0x16f4 } else { 0x7ac } + 2 * ix(n, CONTEXTS)?),
            R::PtStartLo(n) => Some(if g { 0x1713 } else { 0x7cb } + 2 * ix(n, CONTEXTS)?),
            R::PtStartHi(n) => Some(if g { 0x1714 } else { 0x7cc } + 2 * ix(n, CONTEXTS)?),
            R::PtEndLo(n) => Some(if g { 0x1733 } else { 0x7eb } + 2 * ix(n, CONTEXTS)?),
            R::PtEndHi(n) => Some(if g { 0x1734 } else { 0x7ec } + 2 * ix(n, CONTEXTS)?),
            R::Sem(e) => Some(if g { 0x1699 } else { 0x751 } + ix(e, ENGINES)?),
            R::Req(e) => Some(if g { 0x16ab } else { 0x763 } + ix(e, ENGINES)?),
            R::Ack(e) => Some(if g { 0x16bd } else { 0x775 } + ix(e, ENGINES)?),
            R::RangeLo(e) => Some(if g { 0x16cf } else { 0x787 } + 2 * ix(e, ENGINES)?),
            R::RangeHi(e) => Some(if g { 0x16d0 } else { 0x788 } + 2 * ix(e, ENGINES)?),
            R::CpDebug => g.then_some(0x1e1f),
        }
    }

    /// Linux's register name (the asic_reg headers').
    pub fn name(self, hub: Hub) -> alloc::string::String {
        use alloc::format;
        let p = hub.p();
        let vm = |s: &str| format!("{p}VM_{s}");
        let mc = |s: &str| format!("{p}MC_VM_{s}");
        match self {
            R::FbOffset => mc("FB_OFFSET"),
            R::FbBase => mc("FB_LOCATION_BASE"),
            R::FbTop => mc("FB_LOCATION_TOP"),
            R::AgpTop => mc("AGP_TOP"),
            R::AgpBot => mc("AGP_BOT"),
            R::AgpBase => mc("AGP_BASE"),
            R::ApertLow => mc("SYSTEM_APERTURE_LOW_ADDR"),
            R::ApertHigh => mc("SYSTEM_APERTURE_HIGH_ADDR"),
            R::DefaultLsb => mc("SYSTEM_APERTURE_DEFAULT_ADDR_LSB"),
            R::DefaultMsb => mc("SYSTEM_APERTURE_DEFAULT_ADDR_MSB"),
            R::L1Tlb => mc("MX_L1_TLB_CNTL"),
            R::L2Cntl => vm("L2_CNTL"),
            R::L2Cntl2 => vm("L2_CNTL2"),
            R::L2Cntl3 => vm("L2_CNTL3"),
            R::L2Cntl4 => vm("L2_CNTL4"),
            R::L2Cntl5 => vm("L2_CNTL5"),
            R::FaultCntl => vm("L2_PROTECTION_FAULT_CNTL"),
            R::FaultCntl2 => vm("L2_PROTECTION_FAULT_CNTL2"),
            R::FaultStatus => vm("L2_PROTECTION_FAULT_STATUS"),
            R::FaultAddrLo => vm("L2_PROTECTION_FAULT_ADDR_LO32"),
            R::FaultAddrHi => vm("L2_PROTECTION_FAULT_ADDR_HI32"),
            R::FaultDefaultLo => vm("L2_PROTECTION_FAULT_DEFAULT_ADDR_LO32"),
            R::FaultDefaultHi => vm("L2_PROTECTION_FAULT_DEFAULT_ADDR_HI32"),
            R::IdentLowLo => vm("L2_CONTEXT1_IDENTITY_APERTURE_LOW_ADDR_LO32"),
            R::IdentLowHi => vm("L2_CONTEXT1_IDENTITY_APERTURE_LOW_ADDR_HI32"),
            R::IdentHighLo => vm("L2_CONTEXT1_IDENTITY_APERTURE_HIGH_ADDR_LO32"),
            R::IdentHighHi => vm("L2_CONTEXT1_IDENTITY_APERTURE_HIGH_ADDR_HI32"),
            R::IdentOffLo => vm("L2_CONTEXT_IDENTITY_PHYSICAL_OFFSET_LO32"),
            R::IdentOffHi => vm("L2_CONTEXT_IDENTITY_PHYSICAL_OFFSET_HI32"),
            R::Ctx(n) => vm(&format!("CONTEXT{n}_CNTL")),
            R::PtBaseLo(n) => vm(&format!("CONTEXT{n}_PAGE_TABLE_BASE_ADDR_LO32")),
            R::PtBaseHi(n) => vm(&format!("CONTEXT{n}_PAGE_TABLE_BASE_ADDR_HI32")),
            R::PtStartLo(n) => vm(&format!("CONTEXT{n}_PAGE_TABLE_START_ADDR_LO32")),
            R::PtStartHi(n) => vm(&format!("CONTEXT{n}_PAGE_TABLE_START_ADDR_HI32")),
            R::PtEndLo(n) => vm(&format!("CONTEXT{n}_PAGE_TABLE_END_ADDR_LO32")),
            R::PtEndHi(n) => vm(&format!("CONTEXT{n}_PAGE_TABLE_END_ADDR_HI32")),
            R::Sem(e) => vm(&format!("INVALIDATE_ENG{e}_SEM")),
            R::Req(e) => vm(&format!("INVALIDATE_ENG{e}_REQ")),
            R::Ack(e) => vm(&format!("INVALIDATE_ENG{e}_ACK")),
            R::RangeLo(e) => vm(&format!("INVALIDATE_ENG{e}_ADDR_RANGE_LO32")),
            R::RangeHi(e) => vm(&format!("INVALIDATE_ENG{e}_ADDR_RANGE_HI32")),
            R::CpDebug => alloc::string::String::from("CP_DEBUG"),
        }
    }
}

/// Field masks (the same layout on both hubs: gc_11_0_0_sh_mask.h, mmhub_3_0_1_sh_mask.h).
pub mod f {
    pub const L1_ENABLE: u32 = 1 << 0;
    pub const L1_SYSTEM_ACCESS_MODE: u32 = 3 << 3;
    pub const L1_UNMAPPED_ACCESS: u32 = 1 << 5;
    pub const L1_ADVANCED_DRIVER_MODEL: u32 = 1 << 6;
    pub const L1_ECO_BITS: u32 = 0xf << 7;
    pub const L1_MTYPE: u32 = 7 << 11;
    pub const L2_ENABLE: u32 = 1 << 0;
    pub const L2_FRAGMENT_PROCESSING: u32 = 1 << 1;
    pub const L2_PDE0_TAG_GENERATION: u32 = 1 << 8;
    pub const L2_DEFAULT_PAGE_OUT_TO_SYSTEM: u32 = 1 << 11;
    pub const L2_PDE_FAULT_CLASSIFICATION: u32 = 1 << 18;
    pub const L2_CONTEXT1_IDENTITY_ACCESS_MODE: u32 = 3 << 19;
    pub const L2_IDENTITY_FRAGMENT_SIZE: u32 = 0x1f << 21;
    pub const L2C2_INVALIDATE_ALL_L1_TLBS: u32 = 1 << 0;
    pub const L2C2_INVALIDATE_L2_CACHE: u32 = 1 << 1;
    pub const CTX_ENABLE: u32 = 1 << 0;
    pub const CTX_DEPTH: u32 = 3 << 1;
    pub const CTX_BLOCK_SIZE: u32 = 0xf << 3;
    pub const CTX_RETRY_PERMISSION_OR_INVALID: u32 = 1 << 7;
    /// RANGE, DUMMY_PAGE, PDE0, VALID, READ, WRITE, EXECUTE ..._ENABLE_DEFAULT (bits 10, 12, 14, 16, 18, 20, 22).
    pub const CTX_FAULT_DEFAULTS: u32 = 0x0055_5400;
    pub const FAULT2_PTE_READ_RETRY: u32 = 1 << 18;
    /// L2_PROTECTION_FAULT_CNTL bits 2..12: every *_PROTECTION_FAULT_ENABLE_DEFAULT.
    pub const FAULT_ENABLE_DEFAULTS: u32 = 0x1ffc;
    pub const FAULT_CLEAR_STATUS: u32 = 1 << 0;
    pub const CP_DEBUG_UTCL1_ERROR_HALT_DISABLE: u32 = 1 << 15;
    pub const MTYPE_UC: u32 = 3;
}

/// The values Linux writes that are not read-modify-write (gfxhub_v3_0.c:233-253 with its register defaults:
/// BANK_SELECT 9, L2_CACHE_BIGK_FRAGMENT_SIZE 6; the 27.10. capture of the 780M has the same on both hubs).
pub const L2_CNTL3_VALUE: u32 = 0x8013_0009;
pub const L2_CNTL4_VALUE: u32 = 0x0000_0001;
pub const L2_CNTL5_VALUE: u32 = 0x0000_3fe0;
/// The identity aperture switched off (gfxhub_v3_0.c:268-289): low above high.
pub const IDENT_LOW_LO: u32 = 0xffff_ffff;
pub const IDENT_LOW_HI: u32 = 0x0000_000f;
/// Every invalidation engine over the whole space (gfxhub_v3_0.c:340-351).
pub const RANGE_LO_ALL: u32 = 0xffff_ffff;
pub const RANGE_HI_ALL: u32 = 0x1f;
/// VM levels below the root for user contexts (amdgpu_vm.c: 4 levels on GFX 11, num_level 3) and the block
/// size (9 bits, PAGE_TABLE_BLOCK_SIZE = block_size - 9 = 0).
pub const USER_DEPTH: u32 = 3;
/// The last page of the 48-bit space (max_pfn - 1).
pub const MAX_PFN_LAST: u64 = (1 << (48 - 12)) - 1;

/// MX_L1_TLB_CNTL as init_tlb_regs leaves it (0x501 -> 0x1859 on the 780M).
pub fn l1_tlb(v: u32) -> u32 {
    let clear = f::L1_SYSTEM_ACCESS_MODE | f::L1_UNMAPPED_ACCESS | f::L1_ECO_BITS | f::L1_MTYPE;
    (v & !clear) | f::L1_ENABLE | f::L1_SYSTEM_ACCESS_MODE | f::L1_ADVANCED_DRIVER_MODEL | (f::MTYPE_UC << 11)
}

/// L2_CNTL as init_cache_regs leaves it (0x80602 -> 0x80e01).
pub fn l2_cntl(v: u32) -> u32 {
    let clear = f::L2_FRAGMENT_PROCESSING | f::L2_PDE0_TAG_GENERATION | f::L2_PDE_FAULT_CLASSIFICATION | f::L2_CONTEXT1_IDENTITY_ACCESS_MODE | f::L2_IDENTITY_FRAGMENT_SIZE;
    (v & !clear) | f::L2_ENABLE | f::L2_DEFAULT_PAGE_OUT_TO_SYSTEM | (1 << 19)
}

/// L2_CNTL2: both invalidate bits (0 -> 3).
pub fn l2_cntl2(v: u32) -> u32 {
    v | f::L2C2_INVALIDATE_ALL_L1_TLBS | f::L2C2_INVALIDATE_L2_CACHE
}

/// CONTEXT0_CNTL as enable_system_domain leaves it: on, depth 0 (the flat GART), no retry (0x01fffe00 -> +1).
pub fn ctx0_cntl(v: u32) -> u32 {
    (v & !(f::CTX_DEPTH | f::CTX_RETRY_PERMISSION_OR_INVALID)) | f::CTX_ENABLE
}

/// CONTEXT1..15_CNTL as setup_vmid_config leaves them: on, 4 levels, block size 0, every fault's default on, no
/// retry (amdgpu_noretry, the capture: 0x01fffe00 -> 0x01fffe07).
pub fn ctxn_cntl(v: u32) -> u32 {
    (v & !(f::CTX_DEPTH | f::CTX_BLOCK_SIZE | f::CTX_RETRY_PERMISSION_OR_INVALID)) | f::CTX_ENABLE | (USER_DEPTH << 1) | f::CTX_FAULT_DEFAULTS
}

/// L2_PROTECTION_FAULT_CNTL2: ACTIVE_PAGE_MIGRATION_PTE_READ_RETRY (GC 0x20000 -> 0x60000, MM 0xa0000 -> 0xe0000).
pub fn fault_cntl2(v: u32) -> u32 {
    v | f::FAULT2_PTE_READ_RETRY
}

/// L2_PROTECTION_FAULT_CNTL by set_fault_enable_default(true): faults go to the default page.
pub fn fault_cntl(v: u32) -> u32 {
    v | f::FAULT_ENABLE_DEFAULTS
}

/// CP_DEBUG: no CP halt on a UTCL1 error (0x400000 -> 0x408000).
pub fn cp_debug(v: u32) -> u32 {
    v | f::CP_DEBUG_UTCL1_ERROR_HALT_DISABLE
}

/// INVALIDATE_ENGn_REQ in legacy mode for one VMID (get_invalidate_req): L1 and L2 PTEs and PDE0-2 (vmid 0,
/// flush type 0: 0x00f80001).
pub fn invalidate_req(vmid: u8, flush_type: u32) -> u32 {
    let per_vmid = 1u32.checked_shl(u32::from(vmid)).unwrap_or(0) & 0xffff;
    per_vmid | (flush_type & 7) << 16 | 0x1f << 19
}

/// L2_PROTECTION_FAULT_STATUS decoded (both hubs: MORE_FAULTS 0, WALKER_ERROR 3:1, PERMISSION_FAULTS 7:4,
/// MAPPING_ERROR 8, CID 17:9, RW 18, ATOMIC 19, VMID 23:20, VF 24, VFID 28:25, PRT 29).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fault {
    pub raw: u32,
    pub more: bool,
    pub walker: u32,
    pub permission: u32,
    pub mapping: bool,
    pub cid: u32,
    pub write: bool,
    pub atomic: bool,
    pub vmid: u32,
    pub vf: bool,
    pub vfid: u32,
    pub prt: bool,
}

impl Fault {
    pub fn decode(raw: u32) -> Fault {
        let b = |s: u32| raw >> s & 1 != 0;
        Fault {
            raw,
            more: b(0),
            walker: raw >> 1 & 7,
            permission: raw >> 4 & 0xf,
            mapping: b(8),
            cid: raw >> 9 & 0x1ff,
            write: b(18),
            atomic: b(19),
            vmid: raw >> 20 & 0xf,
            vf: b(24),
            vfid: raw >> 25 & 0xf,
            prt: b(29),
        }
    }

    /// The faulting address from FAULT_ADDR_LO32 (page bits 43:12) and HI32 (bits 47:44): gmc_v11_0.c:105-106
    /// builds it the same way from the IH entry.
    pub fn address(lo: u32, hi: u32) -> u64 {
        u64::from(lo) << 12 | u64::from(hi & 0xf) << 44
    }

    /// The client's name (gfxhub_v3_0.c:33-52; mmhub_v3_0_1.c:37-73 by CID and RW).
    pub fn client(&self, hub: Hub) -> Option<&'static str> {
        const GFX: [&str; 18] =
            ["CB/DB", "Reserved", "GE1", "GE2", "CPF", "CPC", "CPG", "RLC", "TCP", "SQC (inst)", "SQC (data)", "SQG", "Reserved", "SDMA0", "SDMA1", "GCR", "SDMA2", "SDMA3"];
        const MM_READ: &[(u32, &str)] = &[(0, "VMC"), (4, "DCEDMC"), (5, "DCEVGA"), (6, "MP0"), (7, "MP1"), (8, "MPIO"), (16, "HDP"), (17, "LSDMA"), (18, "JPEG"), (19, "VCNU0"), (21, "VSCH"), (22, "VCNU1"), (23, "VCN1"), (52, "VCN0")];
        const MM_WRITE: &[(u32, &str)] = &[
            (2, "DBGUNBIO"), (3, "DCEDWB"), (4, "DCEDMC"), (5, "DCEVGA"), (6, "MP0"), (7, "MP1"), (8, "MPIO"), (10, "DBGU0"), (11, "DBGU1"), (12, "DBGU2"), (13, "DBGU3"),
            (14, "XDP"), (15, "OSSSYS"), (16, "HDP"), (17, "LSDMA"), (18, "JPEG"), (19, "VCNU0"), (20, "VCN0"), (21, "VSCH"), (22, "VCNU1"), (23, "VCN1"),
        ];
        match hub {
            Hub::Gfx => GFX.get(self.cid as usize).copied(),
            Hub::Mm => (if self.write { MM_WRITE } else { MM_READ }).iter().find(|(c, _)| *c == self.cid).map(|&(_, n)| n),
        }
    }

    /// The decode as one line (the fields Linux prints, plus VMID and the client's name).
    pub fn text(&self, hub: Hub) -> alloc::string::String {
        if self.raw == 0 {
            return alloc::string::String::from("no fault recorded");
        }
        alloc::format!(
            "{:#010x}: client {} (CID {:#x}), {}{}, VMID {}, MORE_FAULTS {}, WALKER_ERROR {}, PERMISSION_FAULTS {:#x}, MAPPING_ERROR {}{}{}",
            self.raw,
            self.client(hub).unwrap_or("unknown"),
            self.cid,
            if self.write { "write" } else { "read" },
            if self.atomic { " (atomic)" } else { "" },
            self.vmid,
            u32::from(self.more),
            self.walker,
            self.permission,
            u32::from(self.mapping),
            if self.prt { ", PRT" } else { "" },
            if self.vf { alloc::format!(", VF {}", self.vfid) } else { alloc::string::String::new() }
        )
    }
}
