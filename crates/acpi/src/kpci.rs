//! The guard's default view of PCI: configuration space through 0xCF8/0xCFC, read only (V1
//! `acpi/storage.asm` `store_scan`). PC registers a view over its own device list (and its lock) with
//! `live::set_pci_view` once PC-1 keeps one. This scan runs once, in `live::init` (one CPU, interrupts off,
//! before the initcalls: nobody else uses 0xCF8 then); the guard works from the snapshot it leaves and never
//! touches 0xCF8 again without PC's view (and PC's lock).

use crate::guard::{bar_range, BarRange, PciFn, PciUnreadable, PciView, CLASS_MASS_STORAGE};
use crate::sdt::Space;
use alloc::vec::Vec;
use hal::portio::pci_config_read32 as rd;

pub struct PortPci;

impl PciView for PortPci {
    fn function(&self, bus: u8, dev: u8, func: u8) -> Option<PciFn> {
        if dev > 31 || func > 7 {
            return None;
        }
        let id = rd(bus, dev, func, 0);
        let vendor = id as u16;
        if vendor == 0xFFFF || vendor == 0 {
            return None;
        }
        Some(PciFn { vendor, class: (rd(bus, dev, func, 8) >> 24) as u8 })
    }

    fn storage_bars(&self) -> Result<Vec<BarRange>, PciUnreadable> {
        let mut out = Vec::new();
        for bus in 0..=255u8 {
            for dev in 0..32u8 {
                let Some(f0) = self.function(bus, dev, 0) else { continue };
                let multi = (rd(bus, dev, 0, 0x0C) >> 16) & 0x80 != 0;
                let funcs = if multi { 8 } else { 1 };
                for func in 0..funcs {
                    let f = if func == 0 { Some(f0) } else { self.function(bus, dev, func) };
                    if f.is_some_and(|f| f.class == CLASS_MASS_STORAGE) {
                        bars(bus, dev, func, &mut out);
                    }
                }
            }
        }
        Ok(out)
    }
}

/// The six BARs of a type-0 header (a 64-bit BAR takes two slots) and the expansion ROM BAR.
fn bars(bus: u8, dev: u8, func: u8, out: &mut Vec<BarRange>) {
    // Expansion ROM (0x30): address in bits 31:11, whether or not the enable bit is set.
    out.extend(bar_range(Space::Memory, (rd(bus, dev, func, 0x30) & !0x7FF) as u64));
    let mut off = 0x10u8;
    while off <= 0x24 {
        let lo = rd(bus, dev, func, off);
        if lo & 1 != 0 {
            out.extend(bar_range(Space::Io, (lo & 0xFFFC) as u64));
            off += 4;
            continue;
        }
        let mut addr = (lo & !0xF) as u64;
        if (lo >> 1) & 3 == 2 && off < 0x24 {
            addr |= (rd(bus, dev, func, off + 4) as u64) << 32;
            off += 4;
        }
        out.extend(bar_range(Space::Memory, addr));
        off += 4;
    }
}
