//! Register addresses as the discovery table makes them: an offset on top of an IP's base, chosen by base index. This
//! is the arithmetic only, for the first registers GX-10 needs. The generated register crate is `plan/spec/gpu-rdna4.md` piece 5.

use crate::ipfacts::{Entry, find, hw};

/// BAR5, measured. 0x20000 dwords.
pub const BAR5_BYTES: u32 = 512 << 10;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reg {
    pub name: &'static str,
    pub hw_id: u16,
    pub base_index: u8,
    pub offset: u32,
}

const fn psp(name: &'static str, offset: u32) -> Reg {
    Reg { name, hw_id: hw::MP0, base_index: 0, offset }
}

const fn smu(name: &'static str, offset: u32) -> Reg {
    Reg { name, hw_id: hw::MP1, base_index: 1, offset }
}

// `regMPASP_SMN_*`: base index 0 on this chip, where 13.0.4's PSP sat on index 1 (M:365-422).
pub const PSP_C2PMSG_33: Reg = psp("MPASP_SMN_C2PMSG_33", 0x61);
pub const PSP_C2PMSG_35: Reg = psp("MPASP_SMN_C2PMSG_35", 0x63);
pub const PSP_C2PMSG_36: Reg = psp("MPASP_SMN_C2PMSG_36", 0x64);
pub const PSP_C2PMSG_64: Reg = psp("MPASP_SMN_C2PMSG_64", 0x80);
pub const PSP_C2PMSG_69: Reg = psp("MPASP_SMN_C2PMSG_69", 0x85);
pub const PSP_C2PMSG_70: Reg = psp("MPASP_SMN_C2PMSG_70", 0x86);
pub const PSP_C2PMSG_71: Reg = psp("MPASP_SMN_C2PMSG_71", 0x87);
pub const PSP_C2PMSG_81: Reg = psp("MPASP_SMN_C2PMSG_81", 0x91);
/// The KM ring's write pointer on bare metal (M:393; A:psp_v14_0.c:350-370).
pub const PSP_C2PMSG_67: Reg = psp("MPASP_SMN_C2PMSG_67", 0x83);
// The SR-IOV ring (M:441-446; A:psp_v14_0.c:250-266): a virtual function's, never a bare-metal driver's.
pub const PSP_C2PMSG_101: Reg = psp("MPASP_SMN_C2PMSG_101", 0xA5);
pub const PSP_C2PMSG_102: Reg = psp("MPASP_SMN_C2PMSG_102", 0xA6);
pub const PSP_C2PMSG_103: Reg = psp("MPASP_SMN_C2PMSG_103", 0xA7);

// `regMP1_SMN_*`, the SMU's mailbox: message, argument, answer (M:161-210;
// A:../pm/swsmu/smu14/smu_v14_0_2_ppt.c:2149-2151).
pub const SMU_MESSAGE: Reg = smu("MP1_SMN_C2PMSG_66", 0x82);
pub const SMU_ARGUMENT: Reg = smu("MP1_SMN_C2PMSG_82", 0x92);
pub const SMU_ANSWER: Reg = smu("MP1_SMN_C2PMSG_90", 0x9A);
// The SMU's debug mailbox (M:135-180; A:../pm/swsmu/smu14/smu_v14_0_2_ppt.c:2159-2161): Linux sends Mode1Reset there.
pub const SMU_DEBUG_ARGUMENT: Reg = smu("MP1_SMN_C2PMSG_53", 0x75);
pub const SMU_DEBUG_ANSWER: Reg = smu("MP1_SMN_C2PMSG_54", 0x76);
pub const SMU_DEBUG_MESSAGE: Reg = smu("MP1_SMN_C2PMSG_75", 0x8B);

/// The SMN address Linux reads the SMU's firmware flags at, through the PCIE index pair: `MP1_Public |
/// smnMP1_FIRMWARE_FLAGS` (../pm/swsmu/inc/smu_v14_0.h:33 and :38; A:../pm/swsmu/smu14/smu_v14_0.c:217-222). A fixed
/// number, never the table's place for the register (MP1 base 7 + 0x4009 is another address).
pub const SMN_MP1_FIRMWARE_FLAGS: u32 = 0x03B0_0000 | 0x0301_0024;
/// `MP1_CRU1_MP1_FIRMWARE_FLAGS__INTERRUPTS_ENABLED_MASK` (mp_14_0_2_sh_mask.h:688): the SMU's firmware runs.
pub const MP1_FW_INTERRUPTS_ENABLED: u32 = 0x1;

/// The PCIE index pair of NBIF 6.3.1 (nbif_6_3_1_offset.h:1131-1134); NBIO 6.3.1 is below 7.11.4, so Linux takes
/// the RSMU pair (A:nbif_v6_3_1.c:449-463) for `amdgpu_device_indirect_rreg` (A:amdgpu_reg_access.c:590-610).
pub const RSMU_INDEX: Reg = Reg { name: "BIF_BX_PF0_RSMU_INDEX", hw_id: hw::NBIF, base_index: 1, offset: 0x0 };
pub const RSMU_DATA: Reg = Reg { name: "BIF_BX_PF0_RSMU_DATA", hw_id: hw::NBIF, base_index: 1, offset: 0x1 };

/// The HDP flush register itself. Linux writes its 0 after a PSP frame (amdgpu_psp.c:3961 -> amdgpu_hdp_generic_flush,
/// amdgpu_hdp.c:51-60) not here but at the remapped place below, then reads MEMSIZE.
pub const HDP_MEM_FLUSH: Reg = Reg { name: "BIF_BX_PF0_HDP_MEM_COHERENCY_FLUSH_CNTL", hw_id: hw::NBIF, base_index: 2, offset: 0xF7 };

/// Where Linux's HDP flush goes: `rmmio_remap.reg_offset + KFD_MMIO_REMAP_HDP_MEM_FLUSH_CNTL`, with the offset
/// `MMIO_REG_HOLE_OFFSET` = 0x80000 - PAGE_SIZE (nbif_v6_3_1.c:613-626), a fixed byte of BAR5 and no place of the
/// discovery table. The trace shows it as dword 0x1fc00. It flushes only after soc24's common hw_init has pointed
/// `REMAP_HDP_MEM_FLUSH_CNTL` at it (`nbif_v6_3_1_remap_hdp_registers`, nbif_v6_3_1.c:97-110; the trace's `nbif` section, init.txt line 11).
pub const HDP_FLUSH_REMAPPED_BYTE: u32 = 0x8_0000 - 0x1000;
pub const HDP_FLUSH_REMAPPED: Reg = Reg { name: "HDP_MEM_FLUSH_CNTL (remapped)", hw_id: hw::NBIF, base_index: 0, offset: HDP_FLUSH_REMAPPED_BYTE / 4 };
/// nbif_6_3_1_offset.h:1501-1502: the register that holds the remapped place of the HDP flush.
pub const REMAP_HDP_MEM_FLUSH: Reg = Reg { name: "BIF_BX0_REMAP_HDP_MEM_FLUSH_CNTL", hw_id: hw::NBIF, base_index: 2, offset: 0x12D };

/// Where the card's firmware placed VRAM for the GPU (mmhub_4_1_0_offset.h:860-863; A:mmhub_v4_1_0.c:516-526):
/// `(BASE & FB_BASE_MASK) << 24` is the MC address of VRAM offset 0. Read only; the hubs are G3's.
pub const MMHUB_FB_LOCATION_BASE: Reg = Reg { name: "MMMC_VM_FB_LOCATION_BASE", hw_id: hw::MMHUB, base_index: 0, offset: 0x554 };
pub const MMHUB_FB_LOCATION_TOP: Reg = Reg { name: "MMMC_VM_FB_LOCATION_TOP", hw_id: hw::MMHUB, base_index: 0, offset: 0x555 };
/// `MMMC_VM_FB_LOCATION_BASE__FB_BASE_MASK` (mmhub_4_1_0_sh_mask.h:5078), in units of 16 MiB.
pub const FB_LOCATION_MASK: u32 = 0x00FF_FFFF;
pub const FB_LOCATION_SHIFT: u32 = 24;

/// What Linux polls after the RLC autoload (gc_12_0_0_offset.h:2130-2131, :6861-6862; A:gfx_v12_0.c:2331-2354):
/// CP_STAT 0 and BOOTLOAD_COMPLETE (gc_12_0_0_sh_mask.h:21873).
pub const GC_CP_STAT: Reg = Reg { name: "CP_STAT", hw_id: hw::GC, base_index: 0, offset: 0xF40 };
pub const GC_RLC_BOOTLOAD_STATUS: Reg = Reg { name: "RLC_RLCS_BOOTLOAD_STATUS", hw_id: hw::GC, base_index: 1, offset: 0x4E7C };
pub const BOOTLOAD_COMPLETE: u32 = 0x8000_0000;

/// `include/asic_reg/nbif/nbif_6_3_1_offset.h`:1809-1810; the size of VRAM in MiB.
pub const MEMSIZE: Reg = Reg { name: "RCC_DEV0_EPF0_RCC_CONFIG_MEMSIZE", hw_id: hw::NBIF, base_index: 2, offset: 0xC3 };

/// How the CPU gets at a register.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reach {
    /// Byte offset into BAR5.
    Bar5(u32),
    /// Beyond BAR5, this dword goes through the indirect pair (Linux's `amdgpu_device_indirect_rreg`).
    NeedsIndirect(u32),
}

/// The dword address, or `None` if the table has no such IP or base index.
pub fn dword(table: &[Entry], reg: Reg) -> Option<u32> {
    let base = *find(table, reg.hw_id, 0)?.bases.get(reg.base_index as usize)?;
    base.checked_add(reg.offset)
}

pub const fn reach_of(dword: u32) -> Reach {
    if dword < BAR5_BYTES / 4 { Reach::Bar5(dword * 4) } else { Reach::NeedsIndirect(dword) }
}

pub fn reach(table: &[Entry], reg: Reg) -> Option<Reach> {
    Some(reach_of(dword(table, reg)?))
}
