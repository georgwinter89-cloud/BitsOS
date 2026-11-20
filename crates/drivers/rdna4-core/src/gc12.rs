//! Piece 13, its start: what Linux's `gfx_v12_0_hw_init` (gfx_v12_0.c:3767) does to the graphics block before its
//! memory hub, as registers, fields and arithmetic. The trace's section gc-boot (init.txt lines 9151-9191):
//!
//! - the wait for the RLC's autoload (`gfx_v12_0_wait_for_rlc_autoload_complete` :2331-2354): CP_STAT 0 and
//!   BOOTLOAD_COMPLETE, read together in one loop;
//! - the golden register (`golden_settings_gc_12_0` :259-261 through `soc15_program_register_sequence`,
//!   soc15.c:455-488): DB_MEM_CONFIG with bit 15 set. The list of revision 0 (:253-257) is not built: this card is
//!   revision 1 (`devinfo::CHIP_REV`; Linux's `rev_id` is STRAP0 bits 27:24, nbif_v6_3_1.c:112-122, which the
//!   trace's discovery line 6 reads as 0x310c7550);
//! - one read of GB_ADDR_CONFIG (`get_gb_addr_config` :3624-3658);
//! - where the three RS64 engines will start (`gfx_v12_0_config_gfx_rs64` :2142-2225): per pipe, selected through
//!   GRBM_GFX_CNTL (`soc24_grbm_select`, soc24.c:102-112), the program counter's start from the firmware file's
//!   header (`gfx_firmware_header_v2_0`, amdgpu_ucode.h:189-198: `ucode_start_addr_lo` at byte 52, `_hi` at 56),
//!   then a reset pulse of the engine's pipes so the new start is taken. No HALT bit is touched: the engines stay
//!   halted, their unhalt is `gfx_v12_0_cp_resume`'s (the trace's section gc-cp).
//!
//! Offsets and masks are gc_12_0_0_offset.h's and gc_12_0_0_sh_mask.h's (`rdna4-regs`' test holds each).

use crate::hubseq::Op;
use crate::ipfacts::hw;
use crate::nbreg::Field;
use crate::regmap::{self, Reg};

const fn gc(name: &'static str, base_index: u8, offset: u32) -> Reg {
    Reg { name, hw_id: hw::GC, base_index, offset }
}

pub const CP_STAT: Reg = regmap::GC_CP_STAT;
pub const RLC_RLCS_BOOTLOAD_STATUS: Reg = regmap::GC_RLC_BOOTLOAD_STATUS;
pub const DB_MEM_CONFIG: Reg = gc("DB_MEM_CONFIG", 0, 0x13D2);
pub const GB_ADDR_CONFIG: Reg = gc("GB_ADDR_CONFIG", 0, 0x13DE);
pub const CP_DEBUG: Reg = gc("CP_DEBUG", 0, 0x1E1F);
pub const CP_PFP_PRGRM_CNTR_START: Reg = gc("CP_PFP_PRGRM_CNTR_START", 0, 0x1E44);
pub const CP_ME_PRGRM_CNTR_START: Reg = gc("CP_ME_PRGRM_CNTR_START", 0, 0x1E45);
pub const CP_PFP_PRGRM_CNTR_START_HI: Reg = gc("CP_PFP_PRGRM_CNTR_START_HI", 0, 0x1E59);
pub const CP_ME_PRGRM_CNTR_START_HI: Reg = gc("CP_ME_PRGRM_CNTR_START_HI", 0, 0x1E79);
pub const CP_ME_CNTL: Reg = gc("CP_ME_CNTL", 1, 0x0803);
pub const GRBM_GFX_CNTL: Reg = gc("GRBM_GFX_CNTL", 1, 0x0900);
pub const CP_MEC_RS64_PRGRM_CNTR_START: Reg = gc("CP_MEC_RS64_PRGRM_CNTR_START", 1, 0x2900);
pub const CP_MEC_RS64_CNTL: Reg = gc("CP_MEC_RS64_CNTL", 1, 0x2904);
pub const CP_MEC_RS64_PRGRM_CNTR_START_HI: Reg = gc("CP_MEC_RS64_PRGRM_CNTR_START_HI", 1, 0x2938);

pub mod f {
    use super::Field;
    pub const BOOTLOAD_COMPLETE: Field = Field::new(31, 0x8000_0000);
    pub const CPG_UTCL1_ERROR_HALT_DISABLE: Field = Field::new(15, 0x0000_8000);
    pub const PIPEID: Field = Field::new(0, 0x0000_0003);
    pub const MEID: Field = Field::new(2, 0x0000_000C);
    pub const VMID: Field = Field::new(4, 0x0000_00F0);
    pub const QUEUEID: Field = Field::new(8, 0x0000_0700);
    // CP_ME_CNTL.
    pub const PFP_PIPE0_RESET: Field = Field::new(18, 0x0004_0000);
    pub const PFP_PIPE1_RESET: Field = Field::new(19, 0x0008_0000);
    pub const ME_PIPE0_RESET: Field = Field::new(20, 0x0010_0000);
    pub const ME_PIPE1_RESET: Field = Field::new(21, 0x0020_0000);
    pub const CE_HALT: Field = Field::new(24, 0x0100_0000);
    pub const PFP_HALT: Field = Field::new(26, 0x0400_0000);
    pub const ME_HALT: Field = Field::new(28, 0x1000_0000);
    // CP_MEC_RS64_CNTL.
    pub const MEC_PIPE0_RESET: Field = Field::new(16, 0x0001_0000);
    pub const MEC_PIPE1_RESET: Field = Field::new(17, 0x0002_0000);
    pub const MEC_PIPE2_RESET: Field = Field::new(18, 0x0004_0000);
    pub const MEC_PIPE3_RESET: Field = Field::new(19, 0x0008_0000);
    pub const MEC_HALT: Field = Field::new(30, 0x4000_0000);
}

/// The reset bits of each engine's pipes and the bits that keep the engines halted.
pub const PFP_PIPE_RESETS: u32 = f::PFP_PIPE0_RESET.mask | f::PFP_PIPE1_RESET.mask;
pub const ME_PIPE_RESETS: u32 = f::ME_PIPE0_RESET.mask | f::ME_PIPE1_RESET.mask;
pub const MEC_PIPE_RESETS: u32 = f::MEC_PIPE0_RESET.mask | f::MEC_PIPE1_RESET.mask | f::MEC_PIPE2_RESET.mask | f::MEC_PIPE3_RESET.mask;
pub const GFX_HALTS: u32 = f::CE_HALT.mask | f::PFP_HALT.mask | f::ME_HALT.mask;
/// Linux's loops: two pipes each for PFP and ME, four for MEC, fixed (gfx_v12_0.c:2156, :2179, :2202).
pub const GFX_PIPES: u32 = 2;
pub const MEC_PIPES: u32 = 4;
/// `golden_settings_gc_12_0` (gfx_v12_0.c:259-261): DB_MEM_CONFIG, and-mask and or-mask 0x8000.
pub const GOLDEN_DB_MEM_CONFIG: (u32, u32) = (0x0000_8000, 0x0000_8000);

/// Linux's loop ends when CP_STAT reads 0 and the bootload status has BOOTLOAD_COMPLETE (gfx_v12_0.c:2341-2345).
pub const fn autoload_complete(cp_stat: u32, bootload: u32) -> bool {
    cp_stat == 0 && f::BOOTLOAD_COMPLETE.get(bootload) == 1
}

/// `soc15_program_register_sequence` for one entry whose and-mask is not all ones (soc15.c:471-481).
pub fn golden_db_mem_config(v: u32) -> u32 {
    let (and, or) = GOLDEN_DB_MEM_CONFIG;
    (v & !and) | (or & and)
}

/// `gfxhub_v12_0_set_fault_enable_default`'s first step (gfxhub_v12_0.c:423-426).
pub fn cp_debug(v: u32) -> u32 {
    f::CPG_UTCL1_ERROR_HALT_DISABLE.put(v, 1)
}

/// `soc24_grbm_select` (soc24.c:102-112).
pub const fn grbm_select(me: u32, pipe: u32, queue: u32, vmid: u32) -> u32 {
    f::QUEUEID.put(f::VMID.put(f::MEID.put(f::PIPEID.put(0, pipe), me), vmid), queue)
}

/// Where an RS64 engine starts, from its file's header (`ucode_start_addr_lo`, `ucode_start_addr_hi`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Start {
    pub lo: u32,
    pub hi: u32,
}

impl Start {
    /// `(hi << 30) | (lo >> 2)` in Linux's 32-bit arithmetic (gfx_v12_0.c:2158-2160): bits of `hi` above the
    /// second fall out.
    pub const fn counter(self) -> u32 {
        (self.hi << 30) | (self.lo >> 2)
    }
    /// `hi >> 2` (:2161-2162).
    pub const fn counter_hi(self) -> u32 {
        self.hi >> 2
    }
}

/// The three files' starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Starts {
    pub pfp: Start,
    pub me: Start,
    pub mec: Start,
}

fn pipes<E>(me: u32, count: u32, start: Reg, start_hi: Reg, s: Start, op: &mut impl FnMut(Op) -> Result<(), E>) -> Result<(), E> {
    for pipe in 0..count {
        op(Op::Write(GRBM_GFX_CNTL, grbm_select(me, pipe, 0, 0)))?;
        op(Op::Write(start, s.counter()))?;
        op(Op::Write(start_hi, s.counter_hi()))?;
    }
    op(Op::Write(GRBM_GFX_CNTL, grbm_select(0, 0, 0, 0)))
}

/// `gfx_v12_0_config_gfx_rs64` (gfx_v12_0.c:2142-2225) as its register operations in Linux's order: PFP's two
/// pipes and their reset pulse, ME's, MEC's four and theirs. Each loop ends selected on pipe 0 again.
pub fn config_gfx_rs64<E>(s: &Starts, mut op: impl FnMut(Op) -> Result<(), E>) -> Result<(), E> {
    pipes(0, GFX_PIPES, CP_PFP_PRGRM_CNTR_START, CP_PFP_PRGRM_CNTR_START_HI, s.pfp, &mut op)?;
    op(Op::Pulse(CP_ME_CNTL, PFP_PIPE_RESETS))?;
    pipes(0, GFX_PIPES, CP_ME_PRGRM_CNTR_START, CP_ME_PRGRM_CNTR_START_HI, s.me, &mut op)?;
    op(Op::Pulse(CP_ME_CNTL, ME_PIPE_RESETS))?;
    pipes(1, MEC_PIPES, CP_MEC_RS64_PRGRM_CNTR_START, CP_MEC_RS64_PRGRM_CNTR_START_HI, s.mec, &mut op)?;
    op(Op::Pulse(CP_MEC_RS64_CNTL, MEC_PIPE_RESETS))
}

/// Every register `config_gfx_rs64` writes (a stage's write list), each once.
pub const RS64_REGS: [Reg; 9] = [
    GRBM_GFX_CNTL,
    CP_PFP_PRGRM_CNTR_START,
    CP_PFP_PRGRM_CNTR_START_HI,
    CP_ME_PRGRM_CNTR_START,
    CP_ME_PRGRM_CNTR_START_HI,
    CP_MEC_RS64_PRGRM_CNTR_START,
    CP_MEC_RS64_PRGRM_CNTR_START_HI,
    CP_ME_CNTL,
    CP_MEC_RS64_CNTL,
];
