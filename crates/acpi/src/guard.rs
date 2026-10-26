//! The register guard (V1 `acpi/guard.asm:11-150`, `acpi/storage.asm`): before anything writes to an
//! address a firmware table names (reset register, PM1/GPE blocks, SMI command, HPET, ...), the guard says
//! whether that address is safe. A hostile or broken table must never steer a write into a disk
//! controller (legacy ports, BARs, configuration space), the PCI index pair, the keyboard controller, the
//! CMOS, an ECAM window or RAM. This module is the pure decision; `live::check_*` is what other lanes call.

use crate::memmap::{is_ram, MemMap};
use crate::mcfg::Ecam;
use crate::sdt::{Gas, Space};
use alloc::vec::Vec;

/// Port ranges no table may name (inclusive): 8042 incl. 0x61-0x63, CMOS/RTC, ATA and floppy (with their
/// control ports), the PCI configuration pair (0xCF9 alone may pass as the reset register).
pub const IO_REFUSED: [(u16, u16); 7] =
    [(0x60, 0x64), (0x70, 0x77), (0x170, 0x177), (0x1F0, 0x1F7), (0x370, 0x377), (0x3F0, 0x3F7), (0xCF8, 0xCFF)];

/// The chipset reset control port (Intel/AMD "CF9").
pub const RESET_CF9: u64 = 0xCF9;

/// PCI class code of mass storage controllers (IDE, SATA, RAID, NVMe, ...).
pub const CLASS_MASS_STORAGE: u8 = 0x01;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    ZeroAddress,
    /// Address space other than memory, system I/O or PCI configuration.
    Space(u8),
    /// Access width 0 or wider than the space allows.
    Width,
    /// Past 0xFFFF (I/O) or overflowing (memory).
    OutOfRange,
    LegacyPort(u16),
    Ram,
    /// No firmware memory map known: memory cannot be told apart from RAM.
    NoMemoryMap,
    PastPhysBits,
    Ecam,
    StorageBar,
    /// The disk controllers' BARs could not be read: refuse rather than guess.
    StorageUnknown,
    /// PCI address outside segment 0, bus 0, device 0-31, function 0-7, offset 0x40-0xFF.
    PciAddress,
    /// Offset inside the standard header (command register, BARs).
    PciHeader,
    PciNoDevice,
    PciMassStorage,
}

impl Refusal {
    pub fn name(self) -> &'static str {
        match self {
            Refusal::ZeroAddress => "zero_address",
            Refusal::Space(_) => "space",
            Refusal::Width => "width",
            Refusal::OutOfRange => "out_of_range",
            Refusal::LegacyPort(_) => "legacy_port",
            Refusal::Ram => "ram",
            Refusal::NoMemoryMap => "no_memory_map",
            Refusal::PastPhysBits => "past_phys_bits",
            Refusal::Ecam => "ecam",
            Refusal::StorageBar => "storage_bar",
            Refusal::StorageUnknown => "storage_unknown",
            Refusal::PciAddress => "pci_address",
            Refusal::PciHeader => "pci_header",
            Refusal::PciNoDevice => "pci_no_device",
            Refusal::PciMassStorage => "pci_mass_storage",
        }
    }
}

/// A BAR of a mass-storage function: `start..end` in memory or I/O space.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BarRange {
    pub space: Space,
    pub start: u64,
    pub end: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PciFn {
    pub vendor: u16,
    pub class: u8,
}

/// The disk controllers' BARs could not all be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PciUnreadable;

/// What the guard needs to know about PCI. The kernel's default reads configuration space through 0xCF8
/// (`kpci`) once at boot; PC registers its own view with `live::set_pci_view` once it keeps the device list.
/// The guard never asks a view directly: it asks a `PciSnapshot` taken from one (at `init`, at
/// `set_pci_view`, at `live::refresh_pci`), so a guard check costs no configuration-space access at all.
pub trait PciView: Sync {
    /// The function at segment 0, or `None` if nothing answers (vendor 0xFFFF or 0, spec trap 3).
    fn function(&self, bus: u8, dev: u8, func: u8) -> Option<PciFn>;
    /// BARs of every mass-storage function (BAR0-5 and the expansion ROM). Sizes are the real ones where the
    /// view knows them (PC sizes BARs), else `bar_range`'s guess. `Err` if they could not all be read.
    fn storage_bars(&self) -> Result<Vec<BarRange>, PciUnreadable>;
    /// Does `start..end` in `space` touch a mass-storage BAR? Views that keep the list override this to
    /// answer without allocating.
    fn storage_hit(&self, space: Space, start: u64, end: u64) -> Result<bool, PciUnreadable> {
        Ok(self.storage_bars()?.iter().any(|b| b.hits(space, start, end)))
    }
}

impl BarRange {
    pub fn hits(&self, space: Space, start: u64, end: u64) -> bool {
        self.space == space && start < self.end && self.start < end
    }
}

/// What the guard knows about PCI, read once from a `PciView`: bus 0's functions (configuration-space
/// registers a table names are on bus 0 only) and the disk controllers' BARs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PciSnapshot {
    bus0: Vec<(u8, u8, PciFn)>,
    bars: Result<Vec<BarRange>, PciUnreadable>,
}

impl PciSnapshot {
    /// 256 function reads on bus 0 plus the view's storage scan.
    pub fn take(v: &dyn PciView) -> PciSnapshot {
        let mut bus0 = Vec::new();
        for dev in 0..32u8 {
            for func in 0..8u8 {
                if let Some(f) = v.function(0, dev, func) {
                    bus0.push((dev, func, f));
                }
            }
        }
        PciSnapshot { bus0, bars: v.storage_bars() }
    }

    /// Knows nothing: every check that needs PCI is refused (fail closed).
    pub fn unknown() -> PciSnapshot {
        PciSnapshot { bus0: Vec::new(), bars: Err(PciUnreadable) }
    }

    pub fn storage_count(&self) -> Option<usize> {
        self.bars.as_ref().ok().map(|b| b.len())
    }
}

impl PciView for PciSnapshot {
    fn function(&self, bus: u8, dev: u8, func: u8) -> Option<PciFn> {
        if bus != 0 {
            return None;
        }
        self.bus0.iter().find(|(d, f, _)| *d == dev && *f == func).map(|(_, _, p)| *p)
    }
    fn storage_bars(&self) -> Result<Vec<BarRange>, PciUnreadable> {
        self.bars.clone()
    }
    fn storage_hit(&self, space: Space, start: u64, end: u64) -> Result<bool, PciUnreadable> {
        match &self.bars {
            Ok(b) => Ok(b.iter().any(|b| b.hits(space, start, end))),
            Err(e) => Err(*e),
        }
    }
}

pub struct Guard<'a> {
    pub map: &'a MemMap,
    /// CPU physical address bits (CPUID 0x80000008); 0 = unknown, then 52 (the architectural maximum).
    pub phys_bits: u32,
    pub ecam: &'a [Ecam],
    pub pci: &'a dyn PciView,
}

impl Guard<'_> {
    /// A write of `bytes` bytes to the register `g`. `allow_cf9`: the caller writes the FADT reset register.
    pub fn check_gas(&self, g: &Gas, bytes: u32, allow_cf9: bool) -> Result<(), Refusal> {
        match g.space {
            Space::Memory => self.check_mem(g.address, bytes),
            Space::Io => self.check_io(g.address, bytes, allow_cf9),
            Space::PciConfig => self.check_pci(g.address, bytes),
            Space::Other(s) => Err(Refusal::Space(s)),
        }
    }

    pub fn check_io(&self, port: u64, bytes: u32, allow_cf9: bool) -> Result<(), Refusal> {
        if port == 0 {
            return Err(Refusal::ZeroAddress);
        }
        if bytes == 0 || bytes > 4 {
            return Err(Refusal::Width);
        }
        let last = port.checked_add(bytes as u64 - 1).ok_or(Refusal::OutOfRange)?;
        if last > 0xFFFF {
            return Err(Refusal::OutOfRange);
        }
        let cf9_alone = allow_cf9 && port == RESET_CF9 && bytes == 1;
        if !cf9_alone
            && let Some(&(lo, _)) = IO_REFUSED.iter().find(|&&(lo, hi)| port <= hi as u64 && lo as u64 <= last)
        {
            return Err(Refusal::LegacyPort(lo));
        }
        self.storage_hit(Space::Io, port, last + 1)
    }

    pub fn check_mem(&self, addr: u64, bytes: u32) -> Result<(), Refusal> {
        if addr == 0 {
            return Err(Refusal::ZeroAddress);
        }
        if bytes == 0 || bytes > 8 {
            return Err(Refusal::Width);
        }
        let end = addr.checked_add(bytes as u64).ok_or(Refusal::OutOfRange)?;
        self.check_mem_range(addr, end)?;
        if self.map.touches(addr, end, is_ram) {
            return Err(Refusal::Ram);
        }
        Ok(())
    }

    /// The non-RAM part of the memory rule, also used before reading a table from reserved memory.
    pub fn check_mem_range(&self, start: u64, end: u64) -> Result<(), Refusal> {
        if self.map.is_empty() {
            return Err(Refusal::NoMemoryMap);
        }
        let bits = if self.phys_bits == 0 { 52 } else { self.phys_bits.min(64) };
        if bits < 64 && end > 1u64 << bits {
            return Err(Refusal::PastPhysBits);
        }
        if self.ecam.iter().any(|e| e.contains(start, end)) {
            return Err(Refusal::Ecam);
        }
        self.storage_hit(Space::Memory, start, end)
    }

    /// GAS PCI address: device in bits 47:32, function in 31:16, offset in 15:0; segment 0, bus 0 (V1).
    pub fn check_pci(&self, addr: u64, bytes: u32) -> Result<(), Refusal> {
        if bytes == 0 || bytes > 4 {
            return Err(Refusal::Width);
        }
        let dev = addr >> 32;
        let func = (addr >> 16) & 0xFFFF;
        let off = addr & 0xFFFF;
        if dev > 31 || func > 7 || off + bytes as u64 > 0x100 {
            return Err(Refusal::PciAddress);
        }
        if off < 0x40 {
            return Err(Refusal::PciHeader);
        }
        match self.pci.function(0, dev as u8, func as u8) {
            None => Err(Refusal::PciNoDevice),
            Some(f) if f.class == CLASS_MASS_STORAGE => Err(Refusal::PciMassStorage),
            Some(_) => Ok(()),
        }
    }

    fn storage_hit(&self, space: Space, start: u64, end: u64) -> Result<(), Refusal> {
        match self.pci.storage_hit(space, start, end) {
            Err(PciUnreadable) => Err(Refusal::StorageUnknown),
            Ok(true) => Err(Refusal::StorageBar),
            Ok(false) => Ok(()),
        }
    }
}

/// The largest memory BAR `bar_range` guesses. Disk controllers' register BARs are 8 KiB (AHCI) to 16 KiB
/// (NVMe), RAID controllers' a few MiB; an alignment-sized guess for a BAR at 0xFE000000 would otherwise
/// cover the I/O APICs, the HPET and the local APIC and make the guard refuse them.
pub const BAR_GUESS_MAX: u64 = 16 << 20;

/// A BAR's extent without sizing it (sizing writes the BAR): as large as its address's alignment, which is
/// never smaller than the real size, but at most `BAR_GUESS_MAX`; I/O BARs at most 256 ports (V1
/// `store_add`). PC's view replaces the guess with the real size.
pub fn bar_range(space: Space, addr: u64) -> Option<BarRange> {
    if addr == 0 {
        return None;
    }
    let align = 1u64.checked_shl(addr.trailing_zeros()).unwrap_or(u64::MAX);
    let size = if space == Space::Io { align.min(0x100) } else { align.min(BAR_GUESS_MAX) };
    Some(BarRange { space, start: addr, end: addr.saturating_add(size) })
}
