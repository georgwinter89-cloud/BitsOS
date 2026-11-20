//! Wave 43, item HB: GFXHUB switched off for a Reload, Linux's `gfxhub_v12_0_gart_disable` (gfxhub_v12_0.c:388-409),
//! which `gfx_v12_0_hw_fini` runs as its last step on the card (gfx_v12_0.c:3917, before `gfx.is_poweron = false`):
//! all sixteen contexts off, the L1 TLB and the L2 cache off, L2_CNTL3 0 (`hub41::gart_disable` over `hub41::GC`;
//! MMHUB's `gart_disable` is the same steps and Linux runs it from `gmc_v12_0_hw_fini`, which is not built).
//!
//! The card's Reload hook runs this (`livegc::GcHubQuiesce`), before the IH's (devmodel runs the newest hook
//! first, and Linux's fini runs gfx before ih). Before it Linux's fini halts the engines (`gfx_v12_0_cp_enable`
//! :3914) and switches the idle interrupt off (:3915): neither engine nor interrupt was started by this wave, so
//! the wave that unhalts adds them here. Not Linux's: it first reads GCVM_CONTEXT0_CNTL and writes nothing while
//! context 0 is off, so a card whose `gchub` never switched the hub on gets no write at a Reload, and a block
//! that answers all ones (no power) is left alone.

use super::ihoff::{Off, OffIo};
use super::nbregs::NbRegs;
use rdna4_core::hub41::{self, Hub, Op};

/// One step on the card; `None` when a register has no place, does not read or takes no write.
fn step(io: &mut dyn OffIo, n: &NbRegs, op: Op) -> Option<()> {
    let b = n.byte(op.reg())?;
    let mut read = || io.read(b).filter(|v| *v != u32::MAX);
    let writes = match op {
        Op::Write(_, v) => [Some(v), None],
        Op::Modify(_, f) => [Some(f(read()?)), None],
        Op::Pulse(_, bits) => {
            let v = read()?;
            [Some(v | bits), Some(v & !bits)]
        }
    };
    writes.into_iter().flatten().all(|v| io.write(b, v)).then_some(())
}

pub fn gart_disable(io: &mut dyn OffIo, n: &NbRegs, h: &Hub) -> Off {
    let Some(cntl0) = n.byte(h.context0_cntl).and_then(|b| io.read(b)).filter(|v| *v != u32::MAX) else { return Off::Gone };
    if hub41::f::ENABLE_CONTEXT.get(cntl0) == 0 {
        return Off::NotOn;
    }
    match hub41::gart_disable(h, |op| step(io, n, op).ok_or(())) {
        Ok(()) => Off::Done,
        Err(()) => Off::Gone,
    }
}
