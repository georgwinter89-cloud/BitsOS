//! The chip's interrupt masks and status for MSI (WL-4). Linux: rtw89_pci_config_intr_mask
//! (WZ:linux pci.c:3895), rtw89_pci_enable_intr / _disable_intr (pci.c:853-867) and
//! rtw89_pci_recognize_intrs (pci.c:774), the 8852BE's chip ops (rtw8852be.c:64-67). The golden
//! trace never writes a mask (the JS driver polled), so nothing here runs in a trace test; the
//! RX poll clears the RX bits of R_AX_PCIE_HISR00 itself as the JS did.
//!
//! The kernel's use is Linux's hard-IRQ/thread split without NAPI: on a wake by MSI the thread
//! masks ([`disable`]), takes and clears the causes ([`recognize`]), serves the chip, then unmasks
//! ([`enable`]); a cause still pending then sends the next MSI.

use crate::bus::{Bus, BusResult};

/// pci.h:162-165, 183, 213, 258, 263
pub const R_AX_HIMR0: u32 = 0x01A0;
pub const R_AX_HISR0: u32 = 0x01A4;
pub const R_AX_PCIE_HIMR00: u32 = 0x10B0;
pub const R_AX_PCIE_HISR00: u32 = 0x10B4;
pub const R_AX_PCIE_HIMR10: u32 = 0x13B0;
pub const R_AX_PCIE_HISR10: u32 = 0x13B4;
/// pci.h:164 (HIMR0/HISR0)
pub const B_AX_HALT_C2H_INT_EN: u32 = 1 << 21;
/// pci.h:188-194, 209-211 (PCIE_HIMR00/HISR00)
pub const B_AX_HS0ISR_IND_INT_EN: u32 = 1 << 24;
pub const B_AX_RPQBD_FULL_INT_EN: u32 = 1 << 20;
pub const B_AX_RDU_INT_EN: u32 = 1 << 19;
pub const B_AX_RXDMA_STUCK_INT_EN: u32 = 1 << 18;
pub const B_AX_TXDMA_STUCK_INT_EN: u32 = 1 << 17;
pub const B_AX_RPQDMA_INT_EN: u32 = 1 << 2;
pub const B_AX_RXP1DMA_INT_EN: u32 = 1 << 1;
pub const B_AX_RXDMA_INT_EN: u32 = 1 << 0;
/// pci.h:259 (PCIE_HIMR10)
pub const B_AX_HC10ISR_IND_INT_EN: u32 = 1 << 28;

/// rtwpci->halt_c2h_intrs and intrs[0..2] (not under recovery)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Masks {
    pub halt_c2h: u32,
    pub intrs: [u32; 2],
}

// Linux: rtw89_pci_config_intr_mask (pci.c:3895), 8852B (not 8851B), under_recovery false
pub const fn config_intr_mask() -> Masks {
    Masks {
        halt_c2h: B_AX_HALT_C2H_INT_EN,
        intrs: [
            B_AX_TXDMA_STUCK_INT_EN
                | B_AX_RXDMA_INT_EN
                | B_AX_RXP1DMA_INT_EN
                | B_AX_RPQDMA_INT_EN
                | B_AX_RXDMA_STUCK_INT_EN
                | B_AX_RDU_INT_EN
                | B_AX_RPQBD_FULL_INT_EN
                | B_AX_HS0ISR_IND_INT_EN,
            B_AX_HC10ISR_IND_INT_EN,
        ],
    }
}

/// What one look at the causes found (struct rtw89_pci_isrs)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Isrs {
    pub halt_c2h: u32,
    pub isrs: [u32; 2],
}

impl Isrs {
    /// the firmware posted a halt (rtw89_pci_interrupt_threadfn -> rtw89_ser_notify)
    pub fn halt(&self) -> bool {
        self.halt_c2h & B_AX_HALT_C2H_INT_EN != 0
    }
    /// the RX ring ran out of buffers (rtw89_pci_isr_rxd_unavail)
    pub fn rx_unavailable(&self) -> bool {
        self.isrs[0] & B_AX_RDU_INT_EN != 0
    }
}

// Linux: rtw89_pci_enable_intr (pci.c:853)
pub fn enable<B: Bus + ?Sized>(bus: &mut B, m: &Masks) -> BusResult<()> {
    bus.w32(R_AX_HIMR0, m.halt_c2h)?;
    bus.w32(R_AX_PCIE_HIMR00, m.intrs[0])?;
    bus.w32(R_AX_PCIE_HIMR10, m.intrs[1])
}

// Linux: rtw89_pci_disable_intr (pci.c:861)
pub fn disable<B: Bus + ?Sized>(bus: &mut B) -> BusResult<()> {
    bus.w32(R_AX_HIMR0, 0)?;
    bus.w32(R_AX_PCIE_HIMR00, 0)?;
    bus.w32(R_AX_PCIE_HIMR10, 0)
}

// Linux: rtw89_pci_recognize_intrs (pci.c:774): read, keep what the masks enable, write back to clear
pub fn recognize<B: Bus + ?Sized>(bus: &mut B, m: &Masks) -> BusResult<Isrs> {
    let s = Isrs {
        halt_c2h: bus.r32(R_AX_HISR0)? & m.halt_c2h,
        isrs: [
            bus.r32(R_AX_PCIE_HISR00)? & m.intrs[0],
            bus.r32(R_AX_PCIE_HISR10)? & m.intrs[1],
        ],
    };
    bus.w32(R_AX_HISR0, s.halt_c2h)?;
    bus.w32(R_AX_PCIE_HISR00, s.isrs[0])?;
    bus.w32(R_AX_PCIE_HISR10, s.isrs[1])?;
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::Width;
    use crate::membus::MemBus;

    #[test]
    fn masks_and_causes_as_linux() {
        let m = config_intr_mask();
        assert_eq!(m.intrs[0], 0x011e_0007);
        assert_eq!(m.halt_c2h, 0x0020_0000);
        let mut b = MemBus::new();
        enable(&mut b, &m).unwrap();
        assert_eq!(b.peek(R_AX_PCIE_HIMR00, Width::W32), m.intrs[0]);
        assert_eq!(
            b.peek(R_AX_PCIE_HIMR10, Width::W32),
            B_AX_HC10ISR_IND_INT_EN
        );
        b.poke(R_AX_HISR0, Width::W32, B_AX_HALT_C2H_INT_EN | 1);
        b.poke(
            R_AX_PCIE_HISR00,
            Width::W32,
            B_AX_RXDMA_INT_EN | B_AX_RDU_INT_EN | (1 << 30),
        );
        let s = recognize(&mut b, &m).unwrap();
        assert!(s.halt() && s.rx_unavailable());
        assert_eq!(
            s.isrs[0],
            B_AX_RXDMA_INT_EN | B_AX_RDU_INT_EN,
            "only the enabled causes"
        );
        disable(&mut b).unwrap();
        assert_eq!(b.peek(R_AX_HIMR0, Width::W32), 0);
    }
}
