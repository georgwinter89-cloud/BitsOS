//! Wave 44, item RG: the kernel rings' interrupt enables cleared as Linux's suspend clears them, for a Reload. Linux's
//! shutdown before a kexec is `amdgpu_device_suspend` (amdgpu_drv.c:2577-2599): its fence driver's fini puts each
//! ring's end-of-pipe source (amdgpu_device.c:4577, amdgpu_fence.c:533-560), then `gfx_v12_0_hw_fini` puts the three
//! fault sources (gfx_v12_0.c:3888-3890) before MES's fini (:3902) and the command processor's halt (:3914). The hook
//! is registered at `gcring`, after MES's, so it runs before MES's and the command processor's (hooks run newest
//! first) and after the copy engines' stop: Linux's order but for the end-of-pipe pair, which Linux clears before
//! the copy engines' suspend and the hook after their stop (the copy engines raise none of the CP's sources).
//!
//! Each step is Linux's read and write of its one change (`gc12ring::EOP_DISABLE_OPS`, `FAULT_DISABLES`); a step
//! whose bits are already clear writes nothing, as Linux puts only a source it got. Until `gcring` set out to write
//! in this boot (`Written`) the hook reads and writes nothing.

use super::ihoff::OffIo;
use super::nbregs::NbRegs;
use super::sdmaoff::{Stopped, Written};
use rdna4_core::gc12ring as rg;
use rdna4_core::hubseq::Op;
use rdna4_core::regmap::Reg;

/// The hook's write list: the three pipes' interrupt controls, as BAR5 bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OffRegs {
    pub ring0: u32,
    pub pipe0: u32,
    pub pipe1: u32,
}

impl OffRegs {
    pub fn of(n: &NbRegs) -> Option<OffRegs> {
        Some(OffRegs { ring0: n.byte(rg::CP_INT_CNTL_RING0)?, pipe0: n.byte(rg::CP_ME1_PIPE0_INT_CNTL)?, pipe1: n.byte(rg::CP_ME1_PIPE1_INT_CNTL)? })
    }

    fn byte(&self, r: Reg) -> Option<u32> {
        [(rg::CP_INT_CNTL_RING0, self.ring0), (rg::CP_ME1_PIPE0_INT_CNTL, self.pipe0), (rg::CP_ME1_PIPE1_INT_CNTL, self.pipe1)].into_iter().find(|(x, _)| *x == r).map(|(_, b)| b)
    }
}

pub fn off(io: &mut dyn OffIo, r: &OffRegs, written: &Written) -> Stopped {
    if !written.get() {
        return Stopped::Untouched;
    }
    for op in rg::EOP_DISABLE_OPS.iter().chain(rg::FAULT_DISABLES.iter()) {
        let Op::Modify(reg, change) = *op else { continue };
        let Some(byte) = r.byte(reg) else { return Stopped::Failed };
        match io.read(byte) {
            Some(v) if v != u32::MAX => {
                if change(v) != v && !io.write(byte, change(v)) {
                    return Stopped::Failed;
                }
            }
            _ => return Stopped::Failed,
        }
    }
    Stopped::Halted
}
