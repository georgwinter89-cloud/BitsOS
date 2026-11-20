//! Wave 44: the command processor halted as Linux halts it, for a Reload: `gfx_v12_0_hw_fini` (gfx_v12_0.c:3914-3915)
//! is `gfx_v12_0_cp_enable(adev, false)` (:3593-3597: the graphics engines halted with Linux's wait for CP_STAT,
//! then MEC halted with its `udelay(50)`) and `gfx_v12_0_enable_gui_idle_interrupt(adev, false)`. An unhalted
//! engine runs on into the next kernel otherwise, and fetches from queues of a kernel that is gone.
//!
//! Not Linux's: the halt touches the block only once the stage `gccp` set out to write it in this boot
//! (`Written`); before that the block may be without power and nothing is read or written. A CP_STAT that does
//! not settle is Linux's logged error and the halt goes on, as Linux's does.

use super::ihoff::OffIo;
use super::nbregs::NbRegs;
use super::sdmaoff::{Stopped, Written};
use crate::gpucore::psp::LINUX_POLLS;
use rdna4_core::{gc12, gc12cp as cp};

/// The registers the halt touches, as BAR5 bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OffRegs {
    pub me_cntl: u32,
    pub cp_stat: u32,
    pub mec_cntl: u32,
    pub int_cntl: u32,
}

impl OffRegs {
    pub fn of(n: &NbRegs) -> Option<OffRegs> {
        Some(OffRegs { me_cntl: n.byte(gc12::CP_ME_CNTL)?, cp_stat: n.byte(gc12::CP_STAT)?, mec_cntl: n.byte(gc12::CP_MEC_RS64_CNTL)?, int_cntl: n.byte(cp::CP_INT_CNTL_RING0)? })
    }
}

pub fn halt(io: &mut dyn OffIo, r: &OffRegs, written: &Written) -> Stopped {
    if !written.get() {
        return Stopped::Untouched;
    }
    let change = |io: &mut dyn OffIo, byte: u32, f: fn(u32) -> u32| match io.read(byte) {
        Some(v) if v != u32::MAX => io.write(byte, f(v)),
        _ => false,
    };
    if !change(io, r.me_cntl, cp::halt_gfx) {
        return Stopped::Failed;
    }
    // gfx_v12_0_cp_gfx_enable's loop (:2371-2378): a CP_STAT that does not read 0 is logged and passed.
    for _ in 0..LINUX_POLLS {
        if io.read(r.cp_stat) == Some(0) {
            break;
        }
        io.delay_us(1);
    }
    if !change(io, r.mec_cntl, cp::halt_mec) {
        return Stopped::Failed;
    }
    io.delay_us(50);
    if !change(io, r.int_cntl, cp::gui_idle_off) {
        return Stopped::Failed;
    }
    Stopped::Halted
}
