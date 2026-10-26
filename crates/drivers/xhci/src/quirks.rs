//! Quirks by PCI ID (Linux `drivers/usb/host/xhci-pci.c`, `xhci_pci_quirks`). The hook every later quirk goes
//! through; US-1 needs no bring-up quirk for the Mini-PC's four controllers (1022:15B9/15BA/15C0/15C1, see
//! plan/notes/us-US-1.md for what was checked and what is still to confirm against the Linux source).

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
