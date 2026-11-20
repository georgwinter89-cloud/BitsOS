//! Wave 44, item RG (piece 13): the kernel rings' part of Linux's `gfx_v12_0_hw_init` once MES maps them, the
//! trace's section gc-ring (init.txt lines 9568-9603), as registers, fields and arithmetic:
//!
//! - `gfx_v12_0_cp_gfx_start` (gfx_v12_0.c:2699-2710, reached from `cp_async_gfx_ring_resume` :3144):
//!   CP_MAX_CONTEXT = `max_hw_contexts` - 1 (8, :961) and CP_DEVICE_ID 1; the gfx enable is skipped
//!   (`amdgpu_async_gfx_ring` 1, amdgpu_drv.c:223);
//! - `amdgpu_ring_test_helper` on the gfx ring, then on the four compute rings (:3571-3583), each
//!   `gfx_v12_0_ring_test_ring` (:452-492): SCRATCH_REG0 0xCAFEDEAD by MMIO, SET_UCONFIG_REG of 0xDEADBEEF from
//!   the ring, the register polled at most `usec_timeout` times 1 us apart;
//! - the three fault interrupts' enables (`amdgpu_irq_get` of `priv_reg_irq`, `priv_inst_irq`, `bad_op_irq`,
//!   :3852-3862): PRIV_REG on ME 0's one pipe and MEC 1's two pipes (:4935-4979), PRIV_INSTR on ME 0's pipe
//!   only (:5026-5050), OPCODE_ERROR on all three (:4981-5024). This card has no user queue functions, so
//!   `set_userq_eop_interrupts` writes nothing.
//!
//! The end-of-pipe enables (`gfx_v12_0_set_gfx_eop_interrupt_state` :4752-4792 and `_compute_` :4794-4843: TIME_STAMP
//! and GENERIC0) are Linux's fence driver's at `amdgpu_fence_driver_hw_init` after the blocks (section late),
//! here for the rungs. Offsets and masks are gc_12_0_0_offset.h's and gc_12_0_0_sh_mask.h's (`rdna4-regs`' test
//! holds each); the interrupt sources are irqsrcs_gfx_12_0_0.h's.

use crate::hubseq::Op;
use crate::ipfacts::hw;
use crate::nbreg::Field;
use crate::regmap::Reg;

const fn gc(name: &'static str, base_index: u8, offset: u32) -> Reg {
    Reg { name, hw_id: hw::GC, base_index, offset }
}

pub use crate::gc12cp::{CP_INT_CNTL_RING0, SCRATCH_REG0};
pub const CP_MAX_CONTEXT: Reg = gc("CP_MAX_CONTEXT", 0, 0x1E4E);
pub const CP_DEVICE_ID: Reg = gc("CP_DEVICE_ID", 0, 0x1DEB);
pub const CP_ME1_PIPE0_INT_CNTL: Reg = gc("CP_ME1_PIPE0_INT_CNTL", 0, 0x1E25);
pub const CP_ME1_PIPE1_INT_CNTL: Reg = gc("CP_ME1_PIPE1_INT_CNTL", 0, 0x1E26);
/// What shows a busy or hung engine without a write (`gfx_v12_0_is_idle` :3938 reads GUI_ACTIVE, the
/// `wait_for_idle` and devcoredump lists :80-215 name both).
pub const GRBM_STATUS: Reg = gc("GRBM_STATUS", 0, 0x0DA4);
pub use crate::gc12::CP_STAT;

/// `adev->gfx.config.max_hw_contexts` (gfx_v12_0.c:961).
pub const MAX_HW_CONTEXTS: u32 = 8;
pub const DEVICE_ID: u32 = 1;

pub mod f {
    use super::Field;
    pub const PRIV_INSTR_INT_ENABLE: Field = Field::new(22, 0x0040_0000);
    pub const PRIV_REG_INT_ENABLE: Field = Field::new(23, 0x0080_0000);
    pub const OPCODE_ERROR_INT_ENABLE: Field = Field::new(24, 0x0100_0000);
    pub const TIME_STAMP_INT_ENABLE: Field = Field::new(26, 0x0400_0000);
    pub const GENERIC0_INT_ENABLE: Field = Field::new(31, 0x8000_0000);
    // GRBM_STATUS.
    pub const CP_BUSY: Field = Field::new(29, 0x2000_0000);
    pub const GUI_ACTIVE: Field = Field::new(31, 0x8000_0000);
    // CP_STAT.
    pub const CP_STAT_BUSY: Field = Field::new(31, 0x8000_0000);
}

/// The end-of-pipe interrupt's two enables, which Linux sets and clears together.
pub const EOP_ENABLES: u32 = f::TIME_STAMP_INT_ENABLE.mask | f::GENERIC0_INT_ENABLE.mask;

pub fn priv_reg_on(v: u32) -> u32 {
    f::PRIV_REG_INT_ENABLE.put(v, 1)
}
pub fn priv_instr_on(v: u32) -> u32 {
    f::PRIV_INSTR_INT_ENABLE.put(v, 1)
}
pub fn opcode_error_on(v: u32) -> u32 {
    f::OPCODE_ERROR_INT_ENABLE.put(v, 1)
}
pub fn eop_on(v: u32) -> u32 {
    v | EOP_ENABLES
}

/// The fault interrupts' enables in Linux's order, each a read and a write of its change.
pub const FAULT_ENABLES: [Op; 7] = [
    Op::Modify(CP_INT_CNTL_RING0, priv_reg_on),
    Op::Modify(CP_ME1_PIPE0_INT_CNTL, priv_reg_on),
    Op::Modify(CP_ME1_PIPE1_INT_CNTL, priv_reg_on),
    Op::Modify(CP_INT_CNTL_RING0, priv_instr_on),
    Op::Modify(CP_INT_CNTL_RING0, opcode_error_on),
    Op::Modify(CP_ME1_PIPE0_INT_CNTL, opcode_error_on),
    Op::Modify(CP_ME1_PIPE1_INT_CNTL, opcode_error_on),
];

pub fn priv_reg_off(v: u32) -> u32 {
    f::PRIV_REG_INT_ENABLE.put(v, 0)
}
pub fn priv_instr_off(v: u32) -> u32 {
    f::PRIV_INSTR_INT_ENABLE.put(v, 0)
}
pub fn opcode_error_off(v: u32) -> u32 {
    f::OPCODE_ERROR_INT_ENABLE.put(v, 0)
}
pub fn eop_off(v: u32) -> u32 {
    v & !EOP_ENABLES
}

/// What Linux's suspend (the shutdown before a kexec, amdgpu_drv.c:2577-2599) clears of the enables, each a read
/// and a write of its change: first `amdgpu_fence_driver_hw_fini` (amdgpu_device.c:4577, amdgpu_fence.c:533-560)
/// puts each ring's end-of-pipe source, which clears a pipe's pair at its last ring; then `gfx_v12_0_hw_fini`
/// (gfx_v12_0.c:3888-3890) puts the bad opcode, privileged instruction and privileged register sources in that
/// order, each over the pipes its `set_*_state` walks.
pub const EOP_DISABLE_OPS: [Op; 3] = [Op::Modify(CP_INT_CNTL_RING0, eop_off), Op::Modify(CP_ME1_PIPE0_INT_CNTL, eop_off), Op::Modify(CP_ME1_PIPE1_INT_CNTL, eop_off)];
pub const FAULT_DISABLES: [Op; 7] = [
    Op::Modify(CP_INT_CNTL_RING0, opcode_error_off),
    Op::Modify(CP_ME1_PIPE0_INT_CNTL, opcode_error_off),
    Op::Modify(CP_ME1_PIPE1_INT_CNTL, opcode_error_off),
    Op::Modify(CP_INT_CNTL_RING0, priv_instr_off),
    Op::Modify(CP_INT_CNTL_RING0, priv_reg_off),
    Op::Modify(CP_ME1_PIPE0_INT_CNTL, priv_reg_off),
    Op::Modify(CP_ME1_PIPE1_INT_CNTL, priv_reg_off),
];

/// `amdgpu_fence_driver_hw_init` (amdgpu_fence.c:608-635): one `amdgpu_irq_get` per ring in ring order, the gfx
/// ring's pipe first, then MEC 1's two pipes (the four compute rings share them; a second get of a type writes
/// nothing).
pub const EOP_ENABLE_OPS: [Op; 3] = [Op::Modify(CP_INT_CNTL_RING0, eop_on), Op::Modify(CP_ME1_PIPE0_INT_CNTL, eop_on), Op::Modify(CP_ME1_PIPE1_INT_CNTL, eop_on)];

/// The interrupt control register of a kernel queue's pipe: ME 0's one gfx pipe, MEC 1's two.
pub const fn int_cntl_of(me: u32, pipe: u32) -> Option<Reg> {
    match (me, pipe) {
        (0, 0) => Some(CP_INT_CNTL_RING0),
        (1, 0) => Some(CP_ME1_PIPE0_INT_CNTL),
        (1, 1) => Some(CP_ME1_PIPE1_INT_CNTL),
        _ => None,
    }
}

/// `SOC21_IH_CLIENTID_GRBM_CP` (soc15_ih_clientid.h:100) and the sources `gfx_v12_0_sw_init` registers
/// (:1472-1500, irqsrcs_gfx_12_0_0.h:53-56).
pub const CLIENT_GRBM_CP: u8 = 0x14;
pub const SRC_EOP: u8 = 181;
pub const SRC_BAD_OPCODE: u8 = 183;
pub const SRC_PRIV_REG: u8 = 184;
pub const SRC_PRIV_INSTR: u8 = 185;

/// An entry's ring id as `gfx_v12_0_eop_irq` decodes it (:4883-4885): me in bits 3:2, pipe in 1:0, queue in 6:4.
pub const fn ring_id(me: u32, pipe: u32, queue: u32) -> u8 {
    ((me & 3) << 2 | (pipe & 3) | (queue & 7) << 4) as u8
}

/// `amdgpu_gfx_get_hdp_flush_mask` (amdgpu_gfx.c:1364-1396) with NBIF 6.3.1's bits (nbif_v6_3_1.c:466-468): a
/// gfx ring's flush waits on CP0 << pipe with the PFP, a compute ring's on CP2 << pipe with the ME.
pub const fn hdp_flush_mask(gfx: bool, pipe: u32) -> (u32, u8) {
    if gfx { (1 << pipe, 1) } else { (4 << pipe, 0) }
}

/// The rings' frames: `align_mask` 0xff of both kinds (:5359, :5404), so every commit pads to 256 dwords.
pub const FRAME_DW: u64 = 256;
