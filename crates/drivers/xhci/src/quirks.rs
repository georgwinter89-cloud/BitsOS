//! Quirks by PCI ID (Linux `drivers/usb/host/xhci-pci.c`, `xhci_pci_quirks`). The hook every later quirk goes
//! through; US-1 needs no bring-up quirk for the Mini-PC's four controllers (1022:15B9/15BA/15C0/15C1:
//! linux-7.3.1 `xhci_pci_quirks` has no entry for these IDs; its AMD-wide ones need xHCI 0.96 or old chipsets).

pub const VENDOR_AMD: u16 = 0x1022;
/// The Mini-PC's xHCI controllers (`usb.md` §2).
pub const MINIPC_IDS: [(u16, u16); 4] = [(VENDOR_AMD, 0x15B9), (VENDOR_AMD, 0x15BA), (VENDOR_AMD, 0x15C0), (VENDOR_AMD, 0x15C1)];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Quirks {
    /// A short transfer may complete as Success with a non-zero residual (Linux `XHCI_TRUST_TX_LENGTH`, set for
    /// every AMD controller; V1 `xhcievent.asm:146-176`). `trb::classify_transfer` treats both forms as short for
    /// every vendor (harmless where it never happens), so the flag does not change behaviour: the first such
    /// completion of a controller is logged (`xhci.success_with_residue`), as expected with the flag and as a
    /// warning without it, and every one is counted (`Stats::success_residue`).
    pub trust_tx_length: bool,
    /// The controller handles a full event ring poorly: an isochronous request asks for an event interrupt at
    /// least every 8th TD instead of only at its last (Linux `XHCI_AVOID_BEI`, set for every Intel controller;
    /// `AVOID_BEI_INTERVAL_MIN`).
    pub avoid_bei: bool,
}

pub const VENDOR_INTEL: u16 = 0x8086;

pub fn for_id(vendor: u16, _device: u16) -> Quirks {
    Quirks { trust_tx_length: vendor == VENDOR_AMD, avoid_bei: vendor == VENDOR_INTEL }
}

/// The Gaming-PC's two controllers (plan/spec/gaming-pc.md §5): the CPU's and the chipset's (keyboard, mouse, stick).
pub const GAMING_PC_IDS: [(u16, u16); 2] = [(VENDOR_INTEL, 0x7EC0), (VENDOR_INTEL, 0x7F6E)];

/// How long the bring-up waits for the controller. `cnr_ns`: `None` = what is left of `hcrst_ns`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timing {
    /// HCH after RS off, and HCH clear after RS on.
    pub halt_ns: u64,
    /// HCRST clear after the reset was written.
    pub hcrst_ns: u64,
    /// CNR clear after HCRST cleared.
    pub cnr_ns: Option<u64>,
}

impl Timing {
    /// V1's bounds, what every controller before the Gaming-PC's runs with (the Mini-PC's four, QEMU's).
    pub const V1: Timing = Timing { halt_ns: crate::hc::HALT_NS, hcrst_ns: crate::hc::RESET_NS, cnr_ns: None };
    /// Linux's bounds: `XHCI_MAX_HALT_USEC` 32 ms (linux-7.3.1 drivers/usb/host/xhci-ext-caps.h:12, used by
    /// `xhci_halt` and `xhci_start`), `XHCI_RESET_LONG_USEC` 10 s for HCRST and again for CNR (xhci.h:155,
    /// xhci.c:216 and :229, called from `xhci_gen_setup` xhci.c:5513).
    pub const LINUX: Timing = Timing { halt_ns: 32 * crate::hc::MS, hcrst_ns: 10_000 * crate::hc::MS, cnr_ns: Some(10_000 * crate::hc::MS) };
}

/// Intel controllers get Linux's bounds: none of them ever ran under V1's, and a chipset controller with 22 ports
/// that the firmware's keyboard support still drives may take longer to halt than the Mini-PC's. A controller
/// that misses even these is parked with its reason, as before. Everything else keeps V1's bounds unchanged.
pub fn timing_for(vendor: u16) -> Timing {
    if vendor == VENDOR_INTEL { Timing::LINUX } else { Timing::V1 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minipc_ids_get_the_amd_residue_quirk_only_and_intel_avoids_bei() {
        for (v, d) in MINIPC_IDS {
            assert_eq!(for_id(v, d), Quirks { trust_tx_length: true, avoid_bei: false });
        }
        assert_eq!(for_id(0x1B36, 0x000D), Quirks::default(), "QEMU qemu-xhci");
        assert_eq!(for_id(0x1033, 0x0194), Quirks::default(), "NEC uPD720200");
        assert_eq!(for_id(0x8086, 0xA36D), Quirks { trust_tx_length: false, avoid_bei: true }, "Intel Cannon Lake PCH");
    }
}
