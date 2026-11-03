//! Interrupt handler ring on OSSSYS 6.0.x: Linux `ih_v6_0` (linux-6.6 amdgpu/ih_v6_0.c). GX-D6: the ring
//! belongs to the device object; vblank, flips and GFX/SDMA fences arrive on it (GX-7).

use super::{Firmware, Module, Role, State, v};

pub static MODULE: Module = Module {
    name: "ih6_0",
    role: Role::Ih,
    versions: &[v(6, 0, 1)],
    linux: "ih_v6_0",
    linux_also: &[v(6, 0, 0), v(6, 0, 2)],
    firmware: Firmware::None("no firmware"),
    state: State::Planned("GX-7"),
};

/// GX-7b, read-only facts (osssys_6_0_0_offset.h, segment 0 of OSSSYS). IH_RB_CNTL bit 0 RB_ENABLE (the Mini-PC:
/// 0x40610000 cold, 0x40330121 after Linux's ih_v6_0 init).
pub const IH_RB_CNTL: (usize, u32) = (0, 0x80);
pub const IH_RB_BASE: (usize, u32) = (0, 0x81);
pub const IH_RB_BASE_HI: (usize, u32) = (0, 0x82);
pub const IH_RB_RPTR: (usize, u32) = (0, 0x83);
pub const IH_RB_WPTR: (usize, u32) = (0, 0x84);
/// Bit 28 ENABLE: the ring's read pointer comes by doorbell.
pub const IH_DOORBELL_RPTR: (usize, u32) = (0, 0x87);
/// GX-7m: IH_VMID_0_LUT .. IH_VMID_15_LUT (osssys_6_0_0_offset.h:30, segment 0): the VMID-to-PASID table of
/// GFXHUB's VMIDs that names a fault's process; a kernel ring writes entry `vmid` (gmc_v11_0.c:429-445), MES does
/// it for its own queues. (The MMHUB table IH_VMID_0_LUT_MM is not used: no MMHUB VMID here.)
pub const IH_VMID_0_LUT: (usize, u32) = (0, 0x0);
