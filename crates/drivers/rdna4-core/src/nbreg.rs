//! Wave 42 (pieces 6, 10, 11 of plan/spec/gpu-rdna4.md): the registers of Linux's first three blocks on this card,
//! NBIF 6.3.1 with DF 4.15, MMHUB 4.1.0 and IH 7.0, with the fields their sequences set. Offsets and masks are
//! Linux 7.3.1's headers (`include/asic_reg/nbif/nbif_6_3_1_*`, `mmhub/mmhub_4_1_0_*`, `oss/osssys_7_0_0_*`,
//! `df/df_4_15_*`); `rdna4-regs`' test holds every one equal to the generated crate (DF has no generated block: its
//! two numbers are cited here).

use crate::ipfacts::hw;
use crate::regmap::Reg;

/// A bit field of a register.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Field {
    pub shift: u8,
    pub mask: u32,
}

impl Field {
    pub const fn new(shift: u8, mask: u32) -> Field {
        Field { shift, mask }
    }
    pub const fn get(self, reg: u32) -> u32 {
        (reg & self.mask) >> self.shift
    }
    /// Linux's `REG_SET_FIELD`: `reg` with the field set to `value` (bits of `value` past the field are dropped).
    pub const fn put(self, reg: u32, value: u32) -> u32 {
        (reg & !self.mask) | ((value << self.shift) & self.mask)
    }
}

const fn reg(name: &'static str, hw_id: u16, base_index: u8, offset: u32) -> Reg {
    Reg { name, hw_id, base_index, offset }
}

/// Register `i` of a family whose members lie `stride` dwords apart (the name stays the first member's).
pub const fn member(first: Reg, i: u32, stride: u32) -> Reg {
    Reg { offset: first.offset + i * stride, ..first }
}

// NBIF 6.3.1 (A:nbif_v6_3_1.c).
/// Past BAR5 (base 5): through the index pair. `nbif_v6_3_1_init_registers` :480-487 clears bit 7.
pub const STRAP2: Reg = reg("RCC_DEV0_EPF2_STRAP2", hw::NBIF, 5, 0xD102);
pub const STRAP_NO_SOFT_RESET_DEV0_F2: Field = Field::new(7, 0x0000_0080);
/// `nbif_v6_3_1_init_registers` (nbif_v6_3_1.c:480-487): the strap as read with this one field cleared.
pub const fn strap2(read: u32) -> u32 {
    STRAP_NO_SOFT_RESET_DEV0_F2.put(read, 0)
}
/// `nbif_v6_3_1_remap_hdp_registers` :97-110.
pub const REMAP_HDP_MEM_FLUSH: Reg = crate::regmap::REMAP_HDP_MEM_FLUSH;
pub const REMAP_HDP_REG_FLUSH: Reg = reg("BIF_BX0_REMAP_HDP_REG_FLUSH_CNTL", hw::NBIF, 2, 0x12E);
/// `nbif_v6_3_1_enable_doorbell_aperture` :311-316.
pub const DOORBELL_APER_EN: Reg = reg("RCC_DEV0_EPF0_RCC_DOORBELL_APER_EN", hw::NBIF, 2, 0xC0);
pub const BIF_DOORBELL_APER_EN: Field = Field::new(0, 0x0000_0001);
/// `nbif_v6_3_1_ih_control` :386-413.
pub const INTERRUPT_CNTL: Reg = reg("BIF_BX0_INTERRUPT_CNTL", hw::NBIF, 2, 0xF1);
pub const INTERRUPT_CNTL2: Reg = reg("BIF_BX0_INTERRUPT_CNTL2", hw::NBIF, 2, 0xF2);
pub const IH_DUMMY_RD_OVERRIDE: Field = Field::new(0, 0x0000_0001);
pub const IH_REQ_NONSNOOP_EN: Field = Field::new(3, 0x0000_0008);
/// `nbif_v6_3_1_ih_doorbell_range` :341-384 (the register of NBIF below 7.11.4).
pub const DOORBELL_ENTRY_1: Reg = reg("GDC_S2A0_S2A_DOORBELL_ENTRY_1_CTRL", hw::NBIF, 2, 0x1CC);
pub const PORT1_ENABLE: Field = Field::new(0, 0x0000_0001);
pub const PORT1_AWID: Field = Field::new(1, 0x0000_003E);
pub const PORT1_RANGE_OFFSET: Field = Field::new(7, 0x0001_FF80);
pub const PORT1_RANGE_SIZE: Field = Field::new(17, 0x01FE_0000);
pub const PORT1_AWADDR_31_28_VALUE: Field = Field::new(28, 0xF000_0000);
/// Read by `amdgpu_atomfirmware_get_vram_info` (A:amdgpu_atomfirmware.c:534) during GMC's sw_init; BitsOS reads it
/// only because Linux does at this place.
pub const BIOS_SCRATCH_4: Reg = reg("BIF_BX0_BIOS_SCRATCH_4", hw::NBIF, 1, 0x3C);

/// Where Linux's remap puts the two HDP flush registers, as BAR5 bytes: `MMIO_REG_HOLE_OFFSET` = 0x80000 - 4096
/// (A:nbif_v6_3_1.c:613-619) plus `KFD_MMIO_REMAP_HDP_MEM_FLUSH_CNTL` 0 and `..._REG_FLUSH_CNTL` 4
/// (include/uapi/linux/kfd_ioctl.h:745-746). Fixed by Linux, not chosen: the trace shows exactly these.
/// The memory flush's place is the one byte the secure processor's stages flush at too (`regmap`): one number.
pub const REMAPPED_MEM_FLUSH_BYTE: u32 = crate::regmap::HDP_FLUSH_REMAPPED_BYTE;
pub const REMAPPED_REG_FLUSH_BYTE: u32 = REMAPPED_MEM_FLUSH_BYTE + 4;

// DF 4.15 (A:df_v4_15.c:29-40; include/asic_reg/df/df_4_15_offset.h:25-26, df_4_15_sh_mask.h:25-26). Base 4 of
// the card's DF entry is far past BAR5: through the index pair.
pub const DF_NCS_CONFIG1: Reg = reg("NCSConfigurationRegister1", hw::DF, 4, 0x0901);
pub const DIS_INT_ATOMICS_LCL_PROCESSING: Field = Field::new(3, 0x0003_FFF8);
/// `dis_lcl_proc` of `df_v4_15_hw_init`.
pub const DIS_LCL_PROC: u32 = 1 << 1 | 1 << 2 | 1 << 13;

/// What `df_v4_15_hw_init` writes back for what it read.
pub const fn df_ncs_config1(read: u32) -> u32 {
    read | (DIS_LCL_PROC << DIS_INT_ATOMICS_LCL_PROCESSING.shift)
}

/// The only addresses the RSMU index register may be aimed at (the driver's write guard holds every stage to them,
/// placed by the card's own table: `amdgpu::rdna4::nbregs::NbRegs::index_targets`): the SMU's firmware
/// flags (`regmap::SMN_MP1_FIRMWARE_FLAGS`) and the two registers Linux's common hw_init reaches past BAR5. A
/// target is the BYTE address (dword * 4, A:amdgpu_reg_access.c:590-608).
pub fn index_targets(table: &[crate::ipfacts::Entry]) -> [Option<u32>; 3] {
    let byte = |r| crate::regmap::dword(table, r).and_then(|d| d.checked_mul(4));
    [Some(crate::regmap::SMN_MP1_FIRMWARE_FLAGS), byte(STRAP2), byte(DF_NCS_CONFIG1)]
}
