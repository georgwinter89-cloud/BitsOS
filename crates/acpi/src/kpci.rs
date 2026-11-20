//! The guard's default view of PCI: configuration space through 0xCF8/0xCFC, read only (V1
//! `acpi/storage.asm` `store_scan`). PC registers a view over its own device list (and its lock) with
//! `live::set_pci_view` once PC-1 keeps one. This scan runs once, in `live::init` (one CPU, interrupts off,
//! before the initcalls: nobody else uses 0xCF8 then); the guard works from the snapshot it leaves and never
//! touches 0xCF8 again without PC's view (and PC's lock).

use crate::guard::{bar_range, bar_range_until, BarRange, PciFn, PciUnreadable, PciView, CLASS_MASS_STORAGE};
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
        // GP-GUARD CR10: on an Intel CPU (the Gaming-PC; the Mini-PC is AMD and keeps the 16 MiB rule) a storage
        // BAR's guess ends at the next BAR value of any function rather than at 16 MiB.
        let until = hal::cpu::detect().intel;
        let mut storage: Vec<Bar> = Vec::new();
        let mut known: Vec<u64> = Vec::new();
        for bus in 0..=255u8 {
            for dev in 0..32u8 {
                let Some(f0) = self.function(bus, dev, 0) else { continue };
                let multi = (rd(bus, dev, 0, 0x0C) >> 16) & 0x80 != 0;
                let funcs = if multi { 8 } else { 1 };
                for func in 0..funcs {
                    let f = if func == 0 { Some(f0) } else { self.function(bus, dev, func) };
                    let Some(f) = f else { continue };
                    if f.class == CLASS_MASS_STORAGE {
                        bars(bus, dev, func, &mut storage);
                    } else if until && (rd(bus, dev, func, 0x0C) >> 16) & 0x7F == 0 {
                        // Only the BAR values, of type-0 headers: the reads an untouched function allows too.
                        let mut other = Vec::new();
                        bars(bus, dev, func, &mut other);
                        known.extend(decoding(&other));
                    }
                }
            }
        }
        known.extend(decoding(&storage));
        Ok(storage
            .into_iter()
            .filter_map(|b| if until { bar_range_until(b.space, b.addr, &known) } else { bar_range(b.space, b.addr) })
            .collect())
    }
}

/// One BAR value the scan read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bar {
    pub space: Space,
    pub addr: u64,
    /// The function decodes it: every base BAR, the expansion ROM only with its enable bit (bit 0).
    pub decodes: bool,
}

/// The expansion ROM register (0x30): the address in bits 31:11 and whether it is enabled.
pub fn rom(reg: u32) -> Bar {
    Bar { space: Space::Memory, addr: (reg & !0x7FF) as u64, decodes: reg & 1 != 0 }
}

/// The memory addresses that may end another BAR's guess (`bar_range_until`): only those a function decodes. A
/// disabled ROM may hold a leftover address, or share its decoder with a BAR of the same function (PCI 3.0
/// 6.2.5.2), inside a storage BAR; ending the guess there would leave part of the disk's registers unguarded.
pub fn decoding(bars: &[Bar]) -> impl Iterator<Item = u64> + '_ {
    bars.iter().filter(|b| b.space == Space::Memory && b.decodes).map(|b| b.addr)
}

/// The six BARs of a type-0 header (a 64-bit BAR takes two slots) and the expansion ROM BAR; address 0 where a
/// BAR is not set. A storage function's ROM is guarded whether enabled or not (a wider guess is always safe).
fn bars(bus: u8, dev: u8, func: u8, out: &mut Vec<Bar>) {
    out.push(rom(rd(bus, dev, func, 0x30)));
    let mut off = 0x10u8;
    while off <= 0x24 {
        let lo = rd(bus, dev, func, off);
        if lo & 1 != 0 {
            out.push(Bar { space: Space::Io, addr: (lo & 0xFFFC) as u64, decodes: true });
            off += 4;
            continue;
        }
        let mut addr = (lo & !0xF) as u64;
        if (lo >> 1) & 3 == 2 && off < 0x24 {
            addr |= (rd(bus, dev, func, off + 4) as u64) << 32;
            off += 4;
        }
        out.push(Bar { space: Space::Memory, addr, decodes: true });
        off += 4;
    }
}
