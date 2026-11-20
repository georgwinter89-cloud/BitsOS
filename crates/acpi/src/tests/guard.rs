//! The register guard against V1's lists (`acpi/guard.asm:11-150`).

#[allow(unused_imports)]
use alloc::{string::String, vec, vec::Vec};
use crate::guard::{bar_range, bar_range_until, BarRange, FIXED_RANGE, Guard, PciFn, PciSnapshot, PciUnreadable, PciView, Refusal, BAR_GUESS_MAX};
use crate::mcfg::Ecam;
use crate::memmap::{MemMap, Region};
use crate::sdt::{Gas, Space};
use bootinfo::memtype;

struct FakePci {
    /// (dev, func, class) on bus 0.
    fns: Vec<(u8, u8, u8)>,
    bars: Result<Vec<BarRange>, PciUnreadable>,
}

impl PciView for FakePci {
    fn function(&self, bus: u8, dev: u8, func: u8) -> Option<PciFn> {
        self.fns.iter().find(|f| bus == 0 && f.0 == dev && f.1 == func).map(|f| PciFn { vendor: 0x1022, class: f.2 })
    }
    fn storage_bars(&self) -> Result<Vec<BarRange>, PciUnreadable> {
        self.bars.clone()
    }
}

fn minipc_map() -> MemMap {
    MemMap::new(vec![
        Region { start: 0, end: 0xA0000, ty: memtype::CONVENTIONAL },
        Region { start: 0x10_0000, end: 0x9000_0000, ty: memtype::CONVENTIONAL },
        Region { start: 0x9000_0000, end: 0x9700_0000, ty: memtype::ACPI_NVS },
        Region { start: 0x9700_0000, end: 0x9800_0000, ty: memtype::ACPI_RECLAIM },
        Region { start: 0xE000_0000, end: 0xF000_0000, ty: 0 },
        Region { start: 0xFED0_0000, end: 0xFED0_1000, ty: memtype::MMIO },
        Region { start: 0x1_0000_0000, end: 0x6_5E30_0000, ty: memtype::CONVENTIONAL },
    ])
}

fn pci() -> FakePci {
    FakePci {
        // 00:14.0 SMBus (class 0x0C), 00:08.1 bridge, 01:00.0 would be NVMe but bus 0 only here: 00:1F.0 is
        // a fake SATA controller (class 1).
        fns: vec![(0x14, 0, 0x0C), (8, 1, 0x06), (0x1F, 0, 0x01)],
        bars: Ok(vec![
            BarRange { space: Space::Memory, start: 0xDD60_0000, end: 0xDD60_4000 },
            BarRange { space: Space::Io, start: 0xE000, end: 0xE100 },
        ]),
    }
}

fn with<R>(f: impl FnOnce(&Guard<'_>) -> R) -> R {
    let map = minipc_map();
    let ecam = [Ecam { base: 0xE000_0000, segment: 0, bus_start: 0, bus_end: 255 }];
    let p = pci();
    f(&Guard { map: &map, phys_bits: 48, ecam: &ecam, pci: &p })
}

#[test]
fn minipc_reset_and_pm_registers_pass() {
    with(|g| {
        assert_eq!(g.check_gas(&Gas::io(0xB2, 1), 1, true), Ok(()), "reset register 0xB2");
        assert_eq!(g.check_io(0x804, 2, false), Ok(()), "PM1a control");
        assert_eq!(g.check_io(0x820, 4, false), Ok(()), "GPE0");
        assert_eq!(g.check_io(0xCF9, 1, true), Ok(()), "0xCF9 alone as the reset register");
        assert_eq!(g.check_mem(0xFED0_0010, 8), Ok(()), "HPET configuration");
        assert_eq!(g.check_mem(0xFEC0_0000, 4), Ok(()), "I/O APIC (a hole in the map)");
    });
}

#[test]
fn io_refusals() {
    with(|g| {
        for (port, bytes) in [(0x60u64, 1), (0x64, 1), (0x61, 1), (0x70, 1), (0x77, 1), (0x170, 1), (0x1F7, 1), (0x376, 1), (0x3F0, 1), (0xCF8, 4), (0xCFC, 4), (0x5E, 4)] {
            assert!(matches!(g.check_io(port, bytes, false), Err(Refusal::LegacyPort(_))), "{port:#x}+{bytes}");
        }
        assert_eq!(g.check_io(0xCF9, 1, false), Err(Refusal::LegacyPort(0xCF8)), "0xCF9 only as the reset register");
        assert_eq!(g.check_io(0xCF9, 2, true), Err(Refusal::LegacyPort(0xCF8)), "0xCF9 one byte only");
        assert_eq!(g.check_io(0, 1, false), Err(Refusal::ZeroAddress));
        assert_eq!(g.check_io(0xFFFF, 2, false), Err(Refusal::OutOfRange));
        assert_eq!(g.check_io(0x1_0000, 1, false), Err(Refusal::OutOfRange));
        assert_eq!(g.check_io(0x80, 0, false), Err(Refusal::Width));
        assert_eq!(g.check_io(0x80, 8, false), Err(Refusal::Width));
        assert_eq!(g.check_io(0xE0FF, 2, false), Err(Refusal::StorageBar), "a SATA controller's I/O BAR");
        assert_eq!(g.check_io(0xE100, 2, false), Ok(()));
    });
}

#[test]
fn memory_refusals() {
    with(|g| {
        assert_eq!(g.check_mem(0x1000, 4), Err(Refusal::Ram));
        assert_eq!(g.check_mem(0x9000_0000, 4), Err(Refusal::Ram), "ACPI NVS");
        assert_eq!(g.check_mem(0x9700_0000, 4), Err(Refusal::Ram), "ACPI reclaim");
        assert_eq!(g.check_mem(0x8FFF_FFFE, 4), Err(Refusal::Ram), "across the RAM end");
        assert_eq!(g.check_mem(0x9FFF_FFFE, 4), Ok(()), "hole after the ACPI ranges");
        assert_eq!(g.check_mem(0xE000_0000, 4), Err(Refusal::Ecam));
        assert_eq!(g.check_mem(0xEFFF_FFFC, 4), Err(Refusal::Ecam));
        assert_eq!(g.check_mem(0xDD60_3FFC, 4), Err(Refusal::StorageBar), "NVMe BAR");
        assert_eq!(g.check_mem(0x1_0000_0000_0000, 4), Err(Refusal::PastPhysBits));
        assert_eq!(g.check_mem(u64::MAX - 1, 4), Err(Refusal::OutOfRange));
        assert_eq!(g.check_mem(0, 4), Err(Refusal::ZeroAddress));
        assert_eq!(g.check_mem(0xFED0_0000, 16), Err(Refusal::Width));
        assert_eq!(g.check_gas(&Gas { space: Space::Other(3), bit_width: 8, bit_offset: 0, access_size: 1, address: 0x62 }, 1, false), Err(Refusal::Space(3)));
    });
}

#[test]
fn pci_config_refusals() {
    let addr = |dev: u64, func: u64, off: u64| dev << 32 | func << 16 | off;
    with(|g| {
        assert_eq!(g.check_pci(addr(0x14, 0, 0x48), 4), Ok(()), "SMBus register");
        assert_eq!(g.check_pci(addr(0x14, 0, 0x04), 2), Err(Refusal::PciHeader), "command register");
        assert_eq!(g.check_pci(addr(0x14, 0, 0x10), 4), Err(Refusal::PciHeader), "a BAR");
        assert_eq!(g.check_pci(addr(0x1F, 0, 0x80), 4), Err(Refusal::PciMassStorage));
        assert_eq!(g.check_pci(addr(0x02, 0, 0x80), 4), Err(Refusal::PciNoDevice));
        assert_eq!(g.check_pci(addr(32, 0, 0x80), 4), Err(Refusal::PciAddress));
        assert_eq!(g.check_pci(addr(0x14, 8, 0x80), 4), Err(Refusal::PciAddress));
        assert_eq!(g.check_pci(addr(0x14, 0, 0xFE), 4), Err(Refusal::PciAddress), "past 256 bytes");
        assert_eq!(g.check_pci(1 << 48 | addr(0x14, 0, 0x48), 4), Err(Refusal::PciAddress), "bits above the device");
    });
}

#[test]
fn unknown_storage_or_map_refuses() {
    let map = minipc_map();
    let p = FakePci { fns: vec![], bars: Err(PciUnreadable) };
    let g = Guard { map: &map, phys_bits: 48, ecam: &[], pci: &p };
    assert_eq!(g.check_io(0xB2, 1, true), Err(Refusal::StorageUnknown));
    assert_eq!(g.check_mem(0xFED0_0000, 4), Err(Refusal::StorageUnknown));
    let empty = MemMap::default();
    let p = pci();
    let g = Guard { map: &empty, phys_bits: 48, ecam: &[], pci: &p };
    assert_eq!(g.check_mem(0xFED0_0000, 4), Err(Refusal::NoMemoryMap));
    assert_eq!(g.check_io(0xB2, 1, true), Ok(()), "ports need no memory map");
}

#[test]
fn bar_sizes_from_alignment() {
    assert_eq!(bar_range(Space::Memory, 0xDD60_0000).map(|b| b.end), Some(0xDD60_0000 + 0x20_0000));
    assert_eq!(bar_range(Space::Io, 0xE000).map(|b| b.end), Some(0xE100), "I/O at most 256 ports");
    assert_eq!(bar_range(Space::Io, 0xE008).map(|b| b.end), Some(0xE010));
    assert_eq!(bar_range(Space::Memory, 0), None);
    assert_eq!(bar_range(Space::Memory, 1 << 63).map(|b| b.end), Some((1 << 63) + BAR_GUESS_MAX), "capped");
    assert_eq!(bar_range(Space::Memory, 0xFFFF_FFFF_FF00_0000).map(|b| b.end), Some(u64::MAX), "no overflow");
}

/// GP-GUARD CR10, with the Gaming-PC's BAR values (read from the Gaming-PC's sysfs):
/// VMD's 32 MiB BAR is whole, the NVMe guesses end before the SPI controller and the network card.
#[test]
fn the_first_scan_cuts_at_the_next_bar_on_the_gaming_pc() {
    // 01:00.0, 86:00.0, 81:00.0 (NVMe), VMD 00:0e.0's two BARs, SPI 00:1f.5, RTL8126 83:00.0, WLAN 85:00.0.
    let known = [0x8d20_0000, 0xb800_0000, 0xb850_0000, 0xb4_3000_0000, 0xb4_3200_0000, 0x8d30_0000, 0xb830_0000, 0xb831_0000, 0xb860_0000];
    let end = |a: u64| bar_range_until(Space::Memory, a, &known).map(|b| b.end);
    assert_eq!(end(0xb4_3000_0000), Some(0xb4_3200_0000), "VMD's 32 MiB BAR whole (the old guess stopped at 16 MiB)");
    assert_eq!(end(0x8d20_0000), Some(0x8d30_0000), "01:00.0 stops before the SPI controller");
    assert_eq!(end(0xb800_0000), Some(0xb830_0000), "86:00.0 stops before the network card");
    assert_eq!(end(0xb850_0000), Some(0xb860_0000), "81:00.0 stops before the WLAN card");
    for a in known {
        let r = bar_range_until(Space::Memory, a, &known).unwrap();
        assert!(r.end > r.start && known.iter().all(|&k| k <= r.start || k >= r.end), "{a:#x}: {r:?} covers another BAR");
    }
    // Nothing after it: the alignment, but never into the chipset's fixed range.
    assert_eq!(bar_range_until(Space::Memory, 0xC000_0000, &[]).map(|b| b.end), Some(FIXED_RANGE));
    assert_eq!(bar_range_until(Space::Memory, 0xC000_0000, &[0xC000_0000]).map(|b| b.end), Some(FIXED_RANGE), "itself is not the next");
    assert_eq!(bar_range_until(Space::Memory, 0, &known), None);
    assert_eq!(bar_range_until(Space::Io, 0xE000, &known), bar_range(Space::Io, 0xE000), "I/O as before");
    assert_eq!(bar_range_until(Space::Memory, 0xFFFF_FFFF_FF00_0000, &[]).map(|b| b.end), Some(u64::MAX), "no overflow");
    // Never smaller than the old guess where the next BAR is farther than 16 MiB away and the alignment allows.
    let far = bar_range_until(Space::Memory, 0x80_0000_0000, &[0x90_0000_0000]).unwrap();
    assert!(far.end - far.start >= BAR_GUESS_MAX);
}

/// Checker finding 8 (fixer of GP-ROUND): a disabled expansion ROM's leftover address inside a storage BAR does not
/// end that BAR's guess; an enabled ROM does, as any decoding BAR.
#[test]
fn a_disabled_rom_inside_a_storage_bar_does_not_cut_it() {
    use crate::kpci::{decoding, rom, Bar};
    let nvme = Bar { space: Space::Memory, addr: 0x8d20_0000, decodes: true };
    // A neighbour's ROM register: address 0x8d208000, enable bit clear; then the same address enabled.
    let off = rom(0x8d20_8000);
    assert_eq!((off.addr, off.decodes), (0x8d20_8000, false));
    let spi = Bar { space: Space::Memory, addr: 0x8d30_0000, decodes: true };
    let known: Vec<u64> = decoding(&[nvme, off, spi]).collect();
    assert_eq!(known, vec![0x8d20_0000, 0x8d30_0000], "the disabled ROM is not a boundary");
    assert_eq!(bar_range_until(Space::Memory, nvme.addr, &known).map(|b| b.end), Some(0x8d30_0000), "the NVMe's guess runs to the next decoding BAR");
    let on = rom(0x8d20_8001);
    assert!(on.decodes);
    let known: Vec<u64> = decoding(&[nvme, on, spi]).collect();
    assert_eq!(bar_range_until(Space::Memory, nvme.addr, &known).map(|b| b.end), Some(0x8d20_8000), "an enabled ROM decodes there, so the disk's BAR ends before it");
}

#[test]
fn highly_aligned_storage_bar_does_not_swallow_the_apics() {
    // A disk BAR at 0xC0000000 (1 GiB alignment): without the cap the guess would run to 4 GiB and cover the
    // I/O APICs, the HPET and the local APIC; with it, it ends at 16 MiB.
    let map = minipc_map();
    let p = FakePci { fns: vec![], bars: Ok(bar_range(Space::Memory, 0xC000_0000).into_iter().collect()) };
    let g = Guard { map: &map, phys_bits: 48, ecam: &[], pci: &p };
    assert_eq!(g.check_mem(0xC000_0010, 4), Err(Refusal::StorageBar));
    assert_eq!(g.check_mem(0xC0FF_FFFC, 4), Err(Refusal::StorageBar));
    assert_eq!(g.check_mem(0xFED0_0010, 8), Ok(()), "HPET is not inside a 1 GiB-aligned guess any more");
    assert_eq!(g.check_mem(0xFEC0_0000, 4), Ok(()));
}

#[test]
fn pci_index_pair_and_cf9_widths() {
    let map = minipc_map();
    let p = pci();
    let g = Guard { map: &map, phys_bits: 48, ecam: &[], pci: &p };
    assert_eq!(g.check_io(0xCF9, 1, true), Ok(()), "the reset register, one byte");
    assert_eq!(g.check_io(0xCF9, 2, true), Err(Refusal::LegacyPort(0xCF8)));
    assert_eq!(g.check_io(0xCF9, 4, true), Err(Refusal::LegacyPort(0xCF8)));
    assert_eq!(g.check_io(0xCF9, 1, false), Err(Refusal::LegacyPort(0xCF8)), "only as the reset register");
    assert_eq!(g.check_io(0xCFC, 4, false), Err(Refusal::LegacyPort(0xCF8)));
    assert_eq!(g.check_io(0xCFC, 1, true), Err(Refusal::LegacyPort(0xCF8)));
    assert_eq!(g.check_gas(&Gas::io(0xCF9, 2), 2, true), Err(Refusal::LegacyPort(0xCF8)));
    assert_eq!(g.check_gas(&Gas::io(0xCFE, 1), 1, true), Err(Refusal::LegacyPort(0xCF8)));
}

#[test]
fn snapshot_answers_like_its_view_without_asking_it_again() {
    use core::sync::atomic::{AtomicU32, Ordering};
    struct Counting(FakePci, AtomicU32);
    impl PciView for Counting {
        fn function(&self, bus: u8, dev: u8, func: u8) -> Option<PciFn> {
            self.0.function(bus, dev, func)
        }
        fn storage_bars(&self) -> Result<Vec<BarRange>, PciUnreadable> {
            self.1.fetch_add(1, Ordering::Relaxed);
            self.0.storage_bars()
        }
    }
    let v = Counting(pci(), AtomicU32::new(0));
    let snap = PciSnapshot::take(&v);
    assert_eq!(v.1.load(Ordering::Relaxed), 1);
    let map = minipc_map();
    let g = Guard { map: &map, phys_bits: 48, ecam: &[], pci: &snap };
    let direct = Guard { map: &map, phys_bits: 48, ecam: &[], pci: &v.0 };
    for port in [0xB2u64, 0x804, 0xE000, 0xE0F0, 0xF000] {
        assert_eq!(g.check_io(port, 1, false), direct.check_io(port, 1, false), "port {port:#x}");
    }
    for addr in [0xFED0_0000u64, 0xDD60_0000, 0xDD60_1000, 0xFEC0_0000] {
        assert_eq!(g.check_mem(addr, 4), direct.check_mem(addr, 4), "addr {addr:#x}");
    }
    for a in [(0x14u64 << 32) | 0x80, (0x11u64 << 32) | 0x80, (0x1Fu64 << 32) | 0x40, (0x02u64 << 32) | 0x80] {
        assert_eq!(g.check_pci(a, 1), direct.check_pci(a, 1), "pci {a:#x}");
    }
    assert_eq!(v.1.load(Ordering::Relaxed), 1, "checks never rescan");
    assert_eq!(snap.function(1, 0x14, 0), None, "bus 0 only");
    // An unknown snapshot refuses whatever needs PCI.
    let u = PciSnapshot::unknown();
    let g = Guard { map: &map, phys_bits: 48, ecam: &[], pci: &u };
    assert_eq!(g.check_io(0xB2, 1, true), Err(Refusal::StorageUnknown));
    assert_eq!(g.check_pci((0x14u64 << 32) | 0x80, 1), Err(Refusal::PciNoDevice));
}

#[test]
fn memmap_lookup() {
    let m = minipc_map();
    assert!(m.covers(0x9000_0000, 0x9800_0000, crate::memmap::is_ram), "NVS then reclaim, no hole");
    assert!(!m.covers(0x9700_0000, 0x9800_1000, crate::memmap::is_ram), "runs into a hole");
    assert!(m.covers(0xE000_0000, 0xE000_1000, crate::memmap::is_reserved));
    assert_eq!(m.find(0xFED0_0800).map(|r| r.ty), Some(memtype::MMIO));
    assert_eq!(m.find(0xFEC0_0000), None);
}

#[test]
fn overlapping_firmware_map_is_cut_not_trusted() {
    // A broken map: RAM overlapping reserved memory, a region inside another, an empty one, unsorted.
    let m = MemMap::new(vec![
        Region { start: 0x3000, end: 0x5000, ty: 0 },
        Region { start: 0x1000, end: 0x4000, ty: memtype::CONVENTIONAL },
        Region { start: 0x2000, end: 0x2800, ty: memtype::ACPI_NVS },
        Region { start: 0x9000, end: 0x9000, ty: 0 },
        Region { start: 0x5000, end: 0x6000, ty: 0 },
    ]);
    let r: Vec<(u64, u64, u32)> = m.regions().iter().map(|r| (r.start, r.end, r.ty)).collect();
    assert_eq!(r, vec![(0x1000, 0x4000, memtype::CONVENTIONAL), (0x4000, 0x6000, 0)]);
    assert!(m.covers(0x1000, 0x6000, |_| true));
    assert!(!m.covers(0x1000, 0x6001, |_| true));
    for w in m.regions().windows(2) {
        assert!(w[0].end <= w[1].start);
    }
}
