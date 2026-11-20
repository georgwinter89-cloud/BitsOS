//! Wave 42 (piece 11): IH 7.0's two rings as pure arithmetic: the registers, the control values of
//! `ih_v7_0_irq_init` (A:ih_v7_0.c:310-424), an entry's layout (`amdgpu_ih_decode_iv_helper`,
//! A:amdgpu_ih.c:263-295) and the write pointer's overflow rule (`ih_v7_0_get_wptr`, A:ih_v7_0.c:453-498).

use crate::ipfacts::hw;
use crate::nbreg::{self, Field};
use crate::regmap::Reg;

const fn oss(name: &'static str, offset: u32) -> Reg {
    Reg { name, hw_id: hw::OSSSYS, base_index: 0, offset }
}

// osssys_7_0_0_offset.h, all base index 0.
pub const RB_CNTL: Reg = oss("IH_RB_CNTL", 0x80);
pub const RB_RPTR: Reg = oss("IH_RB_RPTR", 0x81);
pub const RB_WPTR: Reg = oss("IH_RB_WPTR", 0x82);
pub const RB_BASE: Reg = oss("IH_RB_BASE", 0x83);
pub const RB_BASE_HI: Reg = oss("IH_RB_BASE_HI", 0x84);
pub const RB_WPTR_ADDR_HI: Reg = oss("IH_RB_WPTR_ADDR_HI", 0x85);
pub const RB_WPTR_ADDR_LO: Reg = oss("IH_RB_WPTR_ADDR_LO", 0x86);
pub const DOORBELL_RPTR: Reg = oss("IH_DOORBELL_RPTR", 0x87);
pub const RB_CNTL_RING1: Reg = oss("IH_RB_CNTL_RING1", 0x8C);
pub const RB_RPTR_RING1: Reg = oss("IH_RB_RPTR_RING1", 0x8D);
pub const RB_WPTR_RING1: Reg = oss("IH_RB_WPTR_RING1", 0x8E);
pub const RB_BASE_RING1: Reg = oss("IH_RB_BASE_RING1", 0x8F);
pub const RB_BASE_HI_RING1: Reg = oss("IH_RB_BASE_HI_RING1", 0x90);
pub const DOORBELL_RPTR_RING1: Reg = oss("IH_DOORBELL_RPTR_RING1", 0x93);
pub const STORM_CLIENT_LIST_CNTL: Reg = oss("IH_STORM_CLIENT_LIST_CNTL", 0xAA);
pub const CNTL2: Reg = oss("IH_CNTL2", 0xC1);
pub const INT_FLOOD_CNTL: Reg = oss("IH_INT_FLOOD_CNTL", 0xD5);
pub const MSI_STORM_CTRL: Reg = oss("IH_MSI_STORM_CTRL", 0xF1);
pub const RING1_CLIENT_CFG_INDEX: Reg = oss("IH_RING1_CLIENT_CFG_INDEX", 0x183);
pub const RING1_CLIENT_CFG_DATA: Reg = oss("IH_RING1_CLIENT_CFG_DATA", 0x184);

/// osssys_7_0_0_sh_mask.h.
pub mod f {
    use super::Field;
    pub const RB_ENABLE: Field = Field::new(0, 0x0000_0001);
    pub const RB_SIZE: Field = Field::new(1, 0x0000_003E);
    pub const WPTR_WRITEBACK_ENABLE: Field = Field::new(8, 0x0000_0100);
    pub const RB_FULL_DRAIN_ENABLE: Field = Field::new(9, 0x0000_0200);
    pub const RB_USED_INT_THRESHOLD: Field = Field::new(12, 0x0000_F000);
    pub const WPTR_OVERFLOW_ENABLE: Field = Field::new(16, 0x0001_0000);
    pub const ENABLE_INTR: Field = Field::new(17, 0x0002_0000);
    pub const MC_SNOOP: Field = Field::new(20, 0x0010_0000);
    pub const RPTR_REARM: Field = Field::new(21, 0x0020_0000);
    pub const MC_RO: Field = Field::new(22, 0x0040_0000);
    pub const MC_VMID: Field = Field::new(24, 0x0F00_0000);
    pub const MC_SPACE: Field = Field::new(28, 0x7000_0000);
    pub const WPTR_OVERFLOW_CLEAR: Field = Field::new(31, 0x8000_0000);
    pub const RB_OVERFLOW: Field = Field::new(0, 0x0000_0001);
    pub const DOORBELL_OFFSET: Field = Field::new(0, 0x03FF_FFFF);
    pub const DOORBELL_ENABLE: Field = Field::new(28, 0x1000_0000);
    pub const CLIENT18_IS_STORM_CLIENT: Field = Field::new(18, 0x0004_0000);
    pub const FLOOD_CNTL_ENABLE: Field = Field::new(3, 0x0000_0008);
    pub const MSI_STORM_DELAY: Field = Field::new(0, 0x0000_0FFF);
    pub const CFG_INDEX: Field = Field::new(0, 0x0000_0007);
    pub const CFG_CLIENT_ID: Field = Field::new(0, 0x0000_00FF);
    pub const CFG_SOURCE_ID: Field = Field::new(8, 0x0000_FF00);
    pub const CFG_SOURCE_ID_MATCH_ENABLE: Field = Field::new(16, 0x0001_0000);
    pub const SELF_IV_FORCE_WPTR_UPDATE_TIMEOUT: Field = Field::new(0, 0x0000_001F);
    pub const SELF_IV_FORCE_WPTR_UPDATE_ENABLE: Field = Field::new(8, 0x0000_0100);
}

/// Both rings: 256 KiB (`ih_v7_0_sw_init` :601 and `IH_RING_SIZE`, A:amdgpu_ih.h:30), so 8192 entries.
pub const RING_BYTES: u32 = 256 << 10;
pub const ENTRY_BYTES: u32 = 32;
pub const PTR_MASK: u32 = RING_BYTES - 1;
/// `MC_SPACE` 4: the ring's address is a GPU virtual address of VMID 0, so it lies in the GART
/// (`use_bus_addr` is false with PSP loading, :600).
pub const MC_SPACE_GPU_VA: u32 = 4;
/// `AMDGPU_NAVI10_DOORBELL_IH` (A:amdgpu_doorbell.h:215, set for this chip in soc24.c:290): ring 0's doorbell is
/// `ih << 1`, ring 1's `(ih + 1) << 1` (:608, :617), as dword indices into BAR2.
pub const DOORBELL_IH: u32 = 0x178;
pub const DOORBELL_RING0: u32 = DOORBELL_IH << 1;
pub const DOORBELL_RING1: u32 = (DOORBELL_IH + 1) << 1;
/// `AMDGPU_IH_MAX_NUM_IVS` (A:amdgpu_ih.h): entries between two updates of the read pointer.
pub const MAX_NUM_IVS: u32 = 32;
/// `SOC21_IH_CLIENTID_IH` (include/soc15_ih_clientid.h): the IH's own entries (ring 1's write pointer).
pub const CLIENT_IH: u8 = 0;
/// The client and source `ih_v7_0_irq_init` sends to ring 1 (:386-389): GFX, source 0.
pub const RING1_CLIENT: u32 = 0xA;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ring {
    Ring0,
    Ring1,
}

/// `ih_v7_0_toggle_ring_interrupts` :127-146: ENABLE_INTR exists on ring 0 only.
pub fn toggled(ring: Ring, v: u32, on: bool) -> u32 {
    let v = f::RB_ENABLE.put(v, u32::from(on));
    if ring == Ring::Ring0 { f::ENABLE_INTR.put(v, u32::from(on)) } else { v }
}

/// `ih_v7_0_rb_cntl` :187-208 with what `ih_v7_0_enable_ring` :250-262 adds per ring (`msi`: ring 0 re-arms on a
/// read pointer update).
pub fn rb_cntl(ring: Ring, v: u32, msi: bool) -> u32 {
    let v = f::MC_SPACE.put(v, MC_SPACE_GPU_VA);
    let v = f::WPTR_OVERFLOW_CLEAR.put(v, 1);
    let v = f::WPTR_OVERFLOW_ENABLE.put(v, 1);
    let v = f::RB_SIZE.put(v, (RING_BYTES / 4).trailing_zeros());
    let v = f::WPTR_WRITEBACK_ENABLE.put(v, 1);
    let v = f::MC_SNOOP.put(v, 1);
    let v = f::MC_VMID.put(f::MC_RO.put(v, 0), 0);
    match ring {
        Ring::Ring0 => f::RPTR_REARM.put(v, u32::from(msi)),
        Ring::Ring1 => f::RB_FULL_DRAIN_ENABLE.put(f::WPTR_OVERFLOW_ENABLE.put(v, 0), 1),
    }
}

/// `ih_v7_0_doorbell_rptr` :210-235.
pub const fn doorbell_rptr(index: u32) -> u32 {
    f::DOORBELL_ENABLE.put(f::DOORBELL_OFFSET.put(0, index), 1)
}

/// `nbif_v6_3_1_ih_doorbell_range` :341-384 for ring 0's doorbell, two doorbells wide.
pub fn doorbell_range(v: u32) -> u32 {
    let v = nbreg::PORT1_AWID.put(nbreg::PORT1_ENABLE.put(v, 1), 0);
    let v = nbreg::PORT1_RANGE_SIZE.put(nbreg::PORT1_RANGE_OFFSET.put(v, DOORBELL_RING0), 2);
    nbreg::PORT1_AWADDR_31_28_VALUE.put(v, 0)
}

/// `nbif_v6_3_1_ih_control` :396-407: no dummy read override, a snooped ring.
pub fn interrupt_cntl(v: u32) -> u32 {
    nbreg::IH_REQ_NONSNOOP_EN.put(nbreg::IH_DUMMY_RD_OVERRIDE.put(v, 0), 0)
}

/// :371-392.
pub fn ring1_client_cfg(v: u32) -> u32 {
    f::CFG_SOURCE_ID_MATCH_ENABLE.put(f::CFG_SOURCE_ID.put(f::CFG_CLIENT_ID.put(v, RING1_CLIENT), 0), 1)
}

/// `force_update_wptr_for_self_int(adev, 0, 8, true)` :92-125: (IH_CNTL2, IH_RB_CNTL_RING1).
pub fn self_int(cntl2: u32, ring1: u32) -> (u32, u32) {
    (f::SELF_IV_FORCE_WPTR_UPDATE_ENABLE.put(f::SELF_IV_FORCE_WPTR_UPDATE_TIMEOUT.put(cntl2, 8), 1), f::RB_USED_INT_THRESHOLD.put(ring1, 0))
}

/// `force_update_wptr_for_self_int(adev, 0, 8, false)`, the first step of `ih_v7_0_irq_disable` :433-440:
/// (IH_CNTL2, IH_RB_CNTL_RING1).
pub fn self_int_off(cntl2: u32, ring1: u32) -> (u32, u32) {
    (f::SELF_IV_FORCE_WPTR_UPDATE_ENABLE.put(f::SELF_IV_FORCE_WPTR_UPDATE_TIMEOUT.put(cntl2, 8), 0), f::RB_USED_INT_THRESHOLD.put(ring1, 0))
}

/// One entry of a ring.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Iv {
    pub client_id: u8,
    pub src_id: u8,
    pub ring_id: u8,
    pub vmid: u8,
    pub vmid_src: bool,
    pub timestamp: u64,
    pub timestamp_src: bool,
    pub pasid: u16,
    pub node_id: u8,
    pub src_data: [u32; 4],
}

impl Iv {
    /// `amdgpu_ih_decode_iv_helper`.
    pub const fn decode(dw: &[u32; 8]) -> Iv {
        Iv {
            client_id: dw[0] as u8,
            src_id: (dw[0] >> 8) as u8,
            ring_id: (dw[0] >> 16) as u8,
            vmid: ((dw[0] >> 24) & 0xF) as u8,
            vmid_src: dw[0] >> 31 != 0,
            timestamp: dw[1] as u64 | ((dw[2] & 0xFFFF) as u64) << 32,
            timestamp_src: dw[2] >> 31 != 0,
            pasid: dw[3] as u16,
            node_id: (dw[3] >> 16) as u8,
            src_data: [dw[4], dw[5], dw[6], dw[7]],
        }
    }

    /// The eight dwords the card writes for this entry (a fake card's side; the trace of 18.11. holds decoded
    /// entries only).
    pub const fn encode(&self) -> [u32; 8] {
        [
            self.client_id as u32 | (self.src_id as u32) << 8 | (self.ring_id as u32) << 16 | ((self.vmid & 0xF) as u32) << 24 | (self.vmid_src as u32) << 31,
            self.timestamp as u32,
            ((self.timestamp >> 32) & 0xFFFF) as u32 | (self.timestamp_src as u32) << 31,
            self.pasid as u32 | (self.node_id as u32) << 16,
            self.src_data[0],
            self.src_data[1],
            self.src_data[2],
            self.src_data[3],
        ]
    }
}

/// Where reading goes on after an overflow: "the last not overwritten vector (wptr + 32)" (:471-478). `wptr` is
/// the register's value with RB_OVERFLOW already cleared.
pub const fn after_overflow(wptr: u32) -> u32 {
    (wptr + ENTRY_BYTES) & PTR_MASK
}
