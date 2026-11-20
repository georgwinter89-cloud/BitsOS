//! Wave 43: the IH rings read from inside a stage. On the card the thread `amdgpu-ih7` reads them at each message
//! under the session's lock (`liveih`), which a running stage holds: a stage that waits for an interrupt of its
//! own would wait for a reader that waits for the stage. So the stage reads the rings itself, with the session's
//! one `ihring::Reader`, through its own card: the rings' memory through the host side, the two IH doorbells and
//! (only when a ring overflowed) the ring's control register, and for a page fault the faulting hub's status and
//! its fault control (the clear), through the stage's guard, which lists exactly these for the stage
//! `sdmatest`. The thread finds the rings read when the stage ends.

use super::host::Card;
use super::ihring::{IhIo, IhUp, Pass};
use super::nbregs::NbRegs;
use rdna4_core::hub41;
use rdna4_core::ih7::{self, Iv, Ring};
use rdna4_core::sdma7;

struct StageIh<'a> {
    card: &'a mut dyn Card,
    n: &'a NbRegs,
    sys: u64,
}

impl StageIh<'_> {
    fn reg(&self, ring: Ring, cntl: bool) -> Option<u32> {
        self.n.byte(match (ring, cntl) {
            (Ring::Ring0, true) => ih7::RB_CNTL,
            (Ring::Ring0, false) => ih7::RB_WPTR,
            (Ring::Ring1, true) => ih7::RB_CNTL_RING1,
            (Ring::Ring1, false) => ih7::RB_WPTR_RING1,
        })
    }
}

impl IhIo for StageIh<'_> {
    fn mem32(&mut self, off: u64) -> Option<u32> {
        let mut b = [0u8; 4];
        self.card.sys_read(self.sys + off, &mut b).then(|| u32::from_le_bytes(b))
    }
    fn set_mem32(&mut self, off: u64, v: u32) -> bool {
        self.card.sys_write(self.sys + off, &v.to_le_bytes())
    }
    fn wptr_reg(&mut self, ring: Ring) -> Option<u32> {
        let b = self.reg(ring, false)?;
        self.card.hw().reg_read(b)
    }
    fn cntl(&mut self, ring: Ring) -> Option<u32> {
        let b = self.reg(ring, true)?;
        self.card.hw().reg_read(b)
    }
    fn set_cntl(&mut self, ring: Ring, v: u32) -> bool {
        self.reg(ring, true).is_some_and(|b| self.card.hw().reg_write(b, v))
    }
    fn doorbell(&mut self, ring: Ring, rptr: u32) -> bool {
        self.card.hw().doorbell_write32(if ring == Ring::Ring0 { ih7::DOORBELL_RING0 } else { ih7::DOORBELL_RING1 }, rptr)
    }
    // A page fault that arrives while the stage reads the rings (an engine that named a bad address): decoded
    // here as the IH thread would decode it (`ihfault`), the status read and the clear through the stage's guard.
    fn fault_status(&mut self, hub: hub41::Kind) -> Option<u32> {
        let b = self.n.byte(hub41::of(hub).fault_status)?;
        self.card.hw().reg_read(b)
    }
    fn fault_clear(&mut self, hub: hub41::Kind) -> bool {
        let Some(b) = self.n.byte(hub41::of(hub).fault_cntl) else { return false };
        // A fault control that reads all ones is a hub that does not answer: nothing is written back.
        self.card.hw().reg_read(b).filter(|v| *v != u32::MAX).is_some_and(|v| self.card.hw().reg_write(b, hub41::fault_clear(v)))
    }
    fn now_us(&mut self) -> u64 {
        self.card.hw().now_us()
    }
}

/// SDMA takes the entries of the two sources `sdma_v7_0_sw_init` registers (sdma_v7_0.c:1302-1312): its trap,
/// and the user fence, which Linux's handler takes and does nothing with while MES has no user queues
/// (`process_fence_irq` :1573-1585). Counted per ring id like the trap, so one that arrives is seen as SDMA's.
pub fn register(ih: &mut IhUp) {
    ih.reader.register(sdma7::CLIENT_GFX, sdma7::SRC_TRAP, "sdma trap");
    ih.reader.register(sdma7::CLIENT_GFX, sdma7::SRC_FENCE, "sdma user fence");
}

/// The trap handler's own check (`sdma_v7_0_process_trap_irq` :1554-1557): a trap entry whose ring id names an
/// engine this card does not have is refused, with Linux's "IH: wrong ring_ID detected, as wrong sdma instance".
/// The session's reader asks this for every entry of a registered source (`ihring::Reader`, also when the IH
/// thread reads), so such an entry never counts as an engine's trap.
pub fn wrong_ring_id(iv: &Iv) -> bool {
    (iv.client_id, iv.src_id) == (sdma7::CLIENT_GFX, sdma7::SRC_TRAP) && sdma7::trap_of(iv.ring_id).is_none()
}

/// One look at the rings from the stage's own card.
pub fn read(ih: &mut IhUp, card: &mut dyn Card, n: &NbRegs) -> [Pass; 2] {
    let sys = ih.place.sys;
    ih.reader.process(&mut StageIh { card, n, sys })
}

/// Traps of engine `i`'s queue 0 that arrived through the reader (`sdma_v7_0_process_trap_irq` :1544-1571).
pub fn traps(ih: &IhUp, i: u32) -> u64 {
    ih.reader.taken(sdma7::CLIENT_GFX, sdma7::SRC_TRAP, sdma7::trap_ring_id(i))
}
