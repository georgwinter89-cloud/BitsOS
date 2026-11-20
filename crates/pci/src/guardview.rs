//! PCI as CP-2's register guard sees it (CP-2 change request 3): the scanned device list instead of the
//! guard's own unlocked 0xCF8 brute scan, with the real (sized) BARs of every mass-storage function.

use crate::node::Bar;
use acpi::guard::{BarRange, PciFn, PciUnreadable, PciView};
use acpi::sdt::Space;
use alloc::vec::Vec;

pub struct GuardView;

impl PciView for GuardView {
    fn function(&self, bus: u8, dev: u8, func: u8) -> Option<PciFn> {
        let nodes = crate::NODES.get();
        let n = nodes.iter().find(|n| n.loc.segment == 0 && n.loc.bus == bus && n.loc.dev == dev && n.loc.func == func)?;
        Some(PciFn { vendor: n.vendor, class: n.class_code() })
    }

    fn storage_bars(&self) -> Result<Vec<BarRange>, PciUnreadable> {
        let nodes = crate::NODES.get();
        // The snapshot is a reference to the tree, no lock is held: the header reads take `pci.cfg` themselves.
        storage_ranges_with(&nodes, Some(&|n: &crate::Node, off: u8| n.function().read32(off)))
    }
}

/// Every assigned BAR of every class 01 function. A storage function whose BARs were not sized makes the
/// whole answer `Err`: the guard then refuses rather than guesses (its V1 rule).
pub fn storage_ranges(nodes: &[crate::Node]) -> Result<Vec<BarRange>, PciUnreadable> {
    storage_ranges_with(nodes, None)
}

/// [`storage_ranges`], with one case more when `header` can read a function's 64-byte header (GP-GUARD,
/// plan/spec/gaming-pc.md §2): a storage function left **untouched** (`nvme_bind=off` on the Gaming-PC) is never
/// sized, on purpose, and must not make the guard refuse every ACPI register (the reset register, the PM timer, the
/// IOAPIC check all ask it). Its extents are then taken from the BAR values the firmware left, read only
/// ([`extent`]): as large as the address's alignment (never smaller than the real BAR), cut where the bridge above
/// stops forwarding and where the next address the tree knows begins, so the disk controller's registers stay refused
/// and a neighbour's do not become "disk controller". Any other unsized storage function (D3hot) is `Err` as before.
pub fn storage_ranges_with(nodes: &[crate::Node], header: Option<&dyn Fn(&crate::Node, u8) -> u32>) -> Result<Vec<BarRange>, PciUnreadable> {
    let mut out = Vec::new();
    // (node, space, address) of every BAR of an untouched storage function, as the firmware left it.
    let mut left: Vec<(usize, Space, u64)> = Vec::new();
    for (i, n) in nodes.iter().enumerate().filter(|(_, n)| n.is_storage()) {
        match (n.bars, header) {
            (Some(bars), _) => {
                for b in bars.iter().flatten().filter(|b| b.assigned()) {
                    let space = if matches!(b, Bar::Io { .. }) { Space::Io } else { Space::Memory };
                    out.push(BarRange { space, start: b.start(), end: b.start().saturating_add(b.size()) });
                }
            }
            (None, Some(read)) if n.untouched.is_some() && n.header_type & 0x7F == 0 => raw_bars(&|off| read(n, off), &mut |space, addr| left.push((i, space, addr))),
            (None, _) => return Err(PciUnreadable),
        }
    }
    // Only a machine with untouched storage comes here (the Mini-PC has none): the neighbours are read once.
    if let Some(read) = header
        && !left.is_empty()
    {
        let fences = fences(nodes, read);
        out.extend(left.iter().filter_map(|&(i, space, addr)| extent(nodes, i, space, addr, &fences)));
    }
    Ok(out)
}

/// Where the fixed chipset registers begin on every PC (I/O APIC 0xFEC00000, HPET, TPM, local APIC, flash up to
/// 4 GiB): no disk BAR reaches in there, and a guess that did would make the guard refuse the I/O APIC.
const CHIPSET_FIXED: u64 = 0xFEC0_0000;

/// PCI I/O BARs decode at most 256 ports (PCI Local Bus 3.0 §6.2.5.1; `acpi::guard::bar_range` has the same bound).
const IO_BAR_MAX: u64 = 0x100;

/// Every address the tree knows something to begin at: sized BARs, the BAR values of functions that were not
/// sized (read from the header, never written), every bridge's forwarding windows, and the fixed chipset range.
fn fences(nodes: &[crate::Node], read: &dyn Fn(&crate::Node, u8) -> u32) -> Vec<(Space, u64)> {
    let mut v = alloc::vec![(Space::Memory, CHIPSET_FIXED)];
    for n in nodes {
        match n.bars {
            Some(bars) => {
                for b in bars.iter().flatten().filter(|b| b.assigned()) {
                    v.push((if matches!(b, Bar::Io { .. }) { Space::Io } else { Space::Memory }, b.start()));
                }
            }
            None if n.header_type & 0x7F == 0 => raw_bars(&|off| read(n, off), &mut |space, addr| v.push((space, addr))),
            None => {}
        }
        if let Some(b) = n.bridge {
            v.extend(b.io.map(|w| (Space::Io, w.start)));
            v.extend([b.mem, b.prefetch].iter().flatten().map(|w| (Space::Memory, w.start)));
        }
    }
    v
}

/// The extent taken for the unsized BAR at `addr` of node `i`. A BAR's address is a multiple of its size, so the
/// address's alignment is never smaller than the real BAR (Intel VMD's 32 MiB configuration BAR on the Gaming-PC
/// included, which a fixed 16 MiB bound cut in half). It ends earlier where something else begins (`fences`) and
/// where the bridge above stops forwarding: a 16 KiB NVMe BAR at the start of its root port's 1 MiB window must
/// not swallow the network card behind the next port. A BAR its bridge does not forward at all was never
/// assigned; it keeps the guard's first-scan bound.
fn extent(nodes: &[crate::Node], i: usize, space: Space, addr: u64, fences: &[(Space, u64)]) -> Option<BarRange> {
    if addr == 0 {
        return None;
    }
    let align = 1u64.checked_shl(addr.trailing_zeros()).unwrap_or(u64::MAX);
    let mut end = addr.saturating_add(if space == Space::Io { align.min(IO_BAR_MAX) } else { align });
    if let Some(next) = fences.iter().filter(|(s, a)| *s == space && *a > addr).map(|(_, a)| *a).min() {
        end = end.min(next);
    }
    if let Some(b) = nodes[i].parent.and_then(|p| nodes.get(p)).and_then(|p| p.bridge) {
        let windows = if space == Space::Io { [b.io, None] } else { [b.mem, b.prefetch] };
        end = match windows.iter().flatten().find(|w| w.start <= addr && addr <= w.end) {
            Some(w) => end.min(w.end.saturating_add(1)),
            None => end.min(acpi::guard::bar_range(space, addr).map_or(addr, |r| r.end)),
        };
    }
    Some(BarRange { space, start: addr, end })
}

/// The six BARs and the expansion ROM BAR of a type 0 header as they stand, without sizing (a 64-bit BAR takes
/// two slots): the same walk as the guard's own first scan (`acpi/src/kpci.rs`). All-ones (the function does not
/// answer) and zero (not assigned) give nothing.
fn raw_bars(read: &dyn Fn(u8) -> u32, each: &mut dyn FnMut(Space, u64)) {
    let mut put = |space, addr: u64| {
        if addr != 0 {
            each(space, addr)
        }
    };
    let rom = read(0x30);
    if rom != u32::MAX {
        put(Space::Memory, (rom & !0x7FF) as u64);
    }
    let mut off = 0x10u8;
    while off <= 0x24 {
        let lo = read(off);
        if lo == u32::MAX {
            off += 4;
            continue;
        }
        if lo & 1 != 0 {
            put(Space::Io, (lo & 0xFFFC) as u64);
            off += 4;
            continue;
        }
        let mut addr = (lo & !0xF) as u64;
        if (lo >> 1) & 3 == 2 && off < 0x24 {
            addr |= (read(off + 4) as u64) << 32;
            off += 4;
        }
        put(Space::Memory, addr);
        off += 4;
    }
}
