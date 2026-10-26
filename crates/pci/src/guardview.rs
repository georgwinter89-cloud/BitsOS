//! PCI as CP-2's register guard sees it (CP-2 change request 3): the scanned device list instead of the
//! guard's own unlocked 0xCF8 brute scan, with the real (sized) BARs of every mass-storage function.

use crate::node::Bar;
use acpi::guard::{BarRange, PciFn, PciUnreadable, PciView};
use acpi::sdt::Space;
use alloc::vec::Vec;

pub struct GuardView;

impl PciView for GuardView {
    fn function(&self, bus: u8, dev: u8, func: u8) -> Option<PciFn> {
        let nodes = crate::NODES.lock();
        let n = nodes.iter().find(|n| n.loc.segment == 0 && n.loc.bus == bus && n.loc.dev == dev && n.loc.func == func)?;
        Some(PciFn { vendor: n.vendor, class: n.class_code() })
    }

    fn storage_bars(&self) -> Result<Vec<BarRange>, PciUnreadable> {
        let nodes = crate::NODES.lock();
        storage_ranges(&nodes)
    }
}

/// Every assigned BAR of every class 01 function. A storage function whose BARs were not sized makes the
/// whole answer `Err`: the guard then refuses rather than guesses (its V1 rule).
pub fn storage_ranges(nodes: &[crate::Node]) -> Result<Vec<BarRange>, PciUnreadable> {
    let mut out = Vec::new();
    for n in nodes.iter().filter(|n| n.is_storage()) {
        let Some(bars) = n.bars else { return Err(PciUnreadable) };
        for b in bars.iter().flatten().filter(|b| b.assigned()) {
            let space = if matches!(b, Bar::Io { .. }) { Space::Io } else { Space::Memory };
            out.push(BarRange { space, start: b.start(), end: b.start().saturating_add(b.size()) });
        }
    }
    Ok(out)
}
