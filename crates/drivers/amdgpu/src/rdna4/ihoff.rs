//! Wave 42: switching the IH off, Linux's `ih_v7_0_irq_disable` (ih_v7_0.c:433-440) in its order:
//! `force_update_wptr_for_self_int(adev, 0, 8, false)` (:92-118: IH_CNTL2 and IH_RB_CNTL_RING1 read, ring 1's
//! control written, then IH_CNTL2), `ih_v7_0_toggle_interrupts(adev, false)` (:127-186: per ring the control
//! without its enable bits, then both pointers 0), and `mdelay(1)` so that what the IH was writing has arrived.
//!
//! The card's Reload hook runs this (`liveih::IhQuiesce`). Not Linux's: it first reads both rings' controls and
//! does nothing at all when neither ring is on, so a card whose `ih` stage never enabled a ring gets no write at a
//! Reload.

use super::nbregs::NbRegs;
use rdna4_core::ih7::{self, Ring, f};

/// The registers as BAR5 bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OffRegs {
    pub cntl2: u32,
    /// Per ring: IH_RB_CNTL, IH_RB_RPTR, IH_RB_WPTR.
    pub rings: [(u32, u32, u32); 2],
}

impl OffRegs {
    pub fn of(n: &NbRegs) -> Option<OffRegs> {
        Some(OffRegs {
            cntl2: n.byte(ih7::CNTL2)?,
            rings: [(n.byte(ih7::RB_CNTL)?, n.byte(ih7::RB_RPTR)?, n.byte(ih7::RB_WPTR)?), (n.byte(ih7::RB_CNTL_RING1)?, n.byte(ih7::RB_RPTR_RING1)?, n.byte(ih7::RB_WPTR_RING1)?)],
        })
    }
}

/// The card's registers for the hook: no lock, no allocation, no sleep (`delay_us` busy-waits).
pub trait OffIo {
    fn read(&mut self, byte: u32) -> Option<u32>;
    fn write(&mut self, byte: u32, v: u32) -> bool;
    fn delay_us(&mut self, us: u64);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Off {
    /// Neither ring was on: nothing was written.
    NotOn,
    /// Linux's sequence ran.
    Done,
    /// A register did not read or a write did not arrive: the sequence ended there.
    Gone,
}

/// `mdelay(1)` of `ih_v7_0_irq_disable`.
pub const SETTLE_US: u64 = 1000;

pub fn irq_disable(io: &mut dyn OffIo, r: &OffRegs) -> Off {
    let rd = |io: &mut dyn OffIo, byte| io.read(byte).filter(|v| *v != u32::MAX);
    let (Some(c0), Some(c1)) = (rd(io, r.rings[0].0), rd(io, r.rings[1].0)) else { return Off::Gone };
    if f::RB_ENABLE.get(c0) == 0 && f::RB_ENABLE.get(c1) == 0 {
        return Off::NotOn;
    }
    let (Some(cntl2), Some(ring1)) = (rd(io, r.cntl2), rd(io, r.rings[1].0)) else { return Off::Gone };
    let (cntl2, ring1) = ih7::self_int_off(cntl2, ring1);
    if !io.write(r.rings[1].0, ring1) || !io.write(r.cntl2, cntl2) {
        return Off::Gone;
    }
    for (ring, (cntl, rptr, wptr)) in [Ring::Ring0, Ring::Ring1].into_iter().zip(r.rings) {
        let Some(v) = rd(io, cntl) else { return Off::Gone };
        if !io.write(cntl, ih7::toggled(ring, v, false)) || !io.write(rptr, 0) || !io.write(wptr, 0) {
            return Off::Gone;
        }
    }
    io.delay_us(SETTLE_US);
    Off::Done
}
