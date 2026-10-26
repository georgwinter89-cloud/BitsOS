//! Interrupt causes. Two ways, chosen by what PC-3's `alloc_irqs` gives (NW-4):
//!
//! **MSI-X, two vectors** (Linux `igc_configure_msix` + `igc_irq_enable`,
//! linux-6.6/drivers/net/ethernet/intel/igc/igc_main.c:4026-4084; igb does the same for the 82576): vector 0 takes
//! the "other" causes (ICR, link change) through IVAR_MISC, vector 1 queue 0's receive and transmit through IVAR0.
//! GPIE = MSIX_MODE | PBA | EIAME | NSICR; both bits in EIAC (the EICR bit is cleared when the message goes) and
//! EIAM (the vector masks itself in EIMS when it fires). The driver writes EIMS again once it has looked: the
//! queue vector when its ring is empty (NAPI), the other vector after reading ICR (`igc_msix_other`, :5385-5410).
//! MSI-X has to be enabled in the capability before these registers are written, or they do not hold (:4037);
//! the PCI core armed MSI-X before this.
//! With one MSI-X vector both go to vector 0.
//!
//! **MSI, one message** (measured on the real I226, spec net.md §2.2, `V1:igcirq.asm:13-89`):
//! - with only IMS the I226 never raises MSI, not even for a cause forced through ICS. It raises one only for an
//!   EICR cause EIMS lets through, and the next only after that EICR bit was cleared;
//! - so queue 0 goes to EICR bit 0 through IVAR0 (Linux `igc_assign_vector` for MSI), EIMS takes bit 0 and bit
//!   31 ("other causes"), GPIE.NSICR makes every ICR read clear it, EITR0 moderates, IMS takes Linux's causes;
//! - acknowledging = reading ICR **and** writing EICR = ~0 before looking at descriptors, so a frame arriving
//!   after the look raises the next MSI.
//!
//! Without either (no vectors, `net_irq=off`) the card runs polled with every cause masked (`reset::quiet`).

use crate::hw::Hw;
use crate::regs::{self, Model};

/// The vector of the "other" causes and of queue 0 in two-vector MSI-X mode.
pub const VEC_OTHER: u16 = 0;
pub const VEC_QUEUE: u16 = 1;

/// How the card's interrupts arrive (what `Hw::setup_irqs` got from the PCI core).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IrqSetup {
    Polled,
    /// Plain MSI (one message used).
    Msi,
    /// MSI-X with this many vectors (1 or 2 used).
    MsiX(u16),
}

impl IrqSetup {
    pub fn name(self) -> &'static str {
        match self {
            IrqSetup::Polled => "polled",
            IrqSetup::Msi => "msi",
            IrqSetup::MsiX(_) => "msix",
        }
    }
    /// (vector of the other causes, vector of queue 0) in MSI-X mode.
    pub fn msix_vectors(self) -> Option<(u16, u16)> {
        match self {
            IrqSetup::MsiX(n) if n >= 2 => Some((VEC_OTHER, VEC_QUEUE)),
            IrqSetup::MsiX(_) => Some((0, 0)),
            _ => None,
        }
    }
}

/// MSI-X causes (module text). `itr`: EITR value of the queue vector. Call with the rings running and MSI-X armed.
pub fn prepare_msix<H: Hw + ?Sized>(hw: &H, model: Model, other: u16, queue: u16, itr: u32) {
    hw.wr(regs::GPIE, regs::GPIE_MSIX_MODE | regs::GPIE_PBA | regs::GPIE_EIAME | regs::GPIE_NSICR);
    hw.wr(regs::IVAR_MISC, (other as u32 | regs::IVAR_VALID) << 8);
    let q = queue as u32 | regs::IVAR_VALID;
    hw.wr(regs::IVAR0, q | q << 8);
    hw.wr(regs::eitr(queue), itr | regs::EITR_CNT_IGNR);
    let mask = (1u32 << other) | (1u32 << queue);
    hw.wr(regs::EIAC, mask);
    hw.wr(regs::EIAM, mask);
    hw.wr(regs::EIMS, mask);
    hw.wr(model.ims(), regs::ICR_LSC);
}

/// MSI causes (`V1:igcirq.asm:39-60`). Call with the rings running and the card's MSI armed by the PCI core.
pub fn prepare_msi<H: Hw + ?Sized>(hw: &H, model: Model, itr: u32) {
    hw.wr(regs::EITR0, itr | regs::EITR_CNT_IGNR);
    hw.wr(regs::IVAR0, regs::IVAR0_Q0_TO_0);
    hw.wr(regs::EIMS, regs::EIMS_Q0_OTHER);
    hw.set_bits(regs::GPIE, regs::GPIE_NSICR);
    hw.wr(model.ims(), regs::IMS_CAUSES);
}

/// MSI: acknowledge before looking at descriptors (`V1:igcirq.asm:73-89`); returns ICR for the link-change cause.
pub fn ack<H: Hw + ?Sized>(hw: &H, model: Model) -> u32 {
    let icr = hw.rd(model.icr());
    hw.wr(regs::EICR, !0);
    icr
}

/// ICR's link status change cause.
pub const ICR_LSC: u32 = regs::ICR_LSC;
