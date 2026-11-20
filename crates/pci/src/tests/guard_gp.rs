//! GP-GUARD: the ACPI guard's view of untouched storage with the Gaming-PC's own addresses (read from
//! `/sys/bus/pci/devices/*/resource` on that machine under CachyOS, 14.11.2026): the three NVMe controllers, the
//! two ASMedia SATA controllers and Intel VMD, with the neighbours a careless guess would swallow. The chipset's
//! ports sit on bus 0x80 there (a second root); here they hang off bus 0, which changes nothing for the rule.

use super::fake::{bridge, Fake, FakeFn};
use super::ALL;
use crate::build;
use crate::guardview::{storage_ranges, storage_ranges_with};
use crate::registry::{storage_kept_off, storage_kept_off_at};
use crate::ConfigAccess;
use acpi::guard::BarRange;
use acpi::sdt::Space;
use alloc::vec::Vec;

const MIB: u64 = 1 << 20;
const VMD: (u8, u8, u8) = (0, 0x0E, 0);
const NVME_CPU: (u8, u8, u8) = (0x01, 0, 0);
const NVME_LEXAR: (u8, u8, u8) = (0x81, 0, 0);
const SATA_A: (u8, u8, u8) = (0x82, 0, 0);
const NIC: (u8, u8, u8) = (0x83, 0, 0);
const SATA_B: (u8, u8, u8) = (0x84, 0, 0);
const WLAN: (u8, u8, u8) = (0x85, 0, 0);
const NVME_980: (u8, u8, u8) = (0x86, 0, 0);

/// A root port with a 1 MiB memory window at `mem` and, when given, a 4 KiB I/O window at `io`.
fn port(secondary: u8, mem: u32, io: Option<u16>) -> FakeFn {
    let mut f = bridge(0x8086, 0x7F38, 0, secondary, secondary);
    f.w32(0x20, (mem >> 16) & 0xFFF0 | ((mem + 0xF_FFFF) & 0xFFF0_0000));
    if let Some(io) = io {
        f.w8(0x1C, (io >> 8) as u8 & 0xF0).w8(0x1D, (io >> 8) as u8 & 0xF0);
    }
    f
}

fn nvme(device: u16, bar: u64) -> FakeFn {
    nvme_of(0x144D, device, bar)
}

fn nvme_of(vendor: u16, device: u16, bar: u64) -> FakeFn {
    let mut f = FakeFn::new(vendor, device, 0x01_08_02, 0);
    f.cmd(0x6).bar64(0, bar, 0x4000, false);
    f
}

fn sata(io: u32, mem: u32) -> FakeFn {
    let mut f = FakeFn::new(0x1B21, 0x0612, 0x01_06_01, 0);
    f.cmd(0x7).bario(0, io + 0x50, 8, true).bario(1, io + 0x40, 4, true).bario(2, io + 0x30, 8, true).bario(3, io + 0x20, 4, true);
    f.bario(4, io, 0x20, true).bar32(5, mem, 0x200, false);
    f
}

fn gaming_pc() -> Fake {
    let w = Fake::new();
    w.add(0, 0, 0, FakeFn::new(0x8086, 0x7D1B, 0x06_00_00, 0));
    w.add(0, 1, 0, port(0x01, 0x8D20_0000, None));
    // Intel VMD, class 0104: 32 MiB at 0xb430000000, 16 MiB at 0x8c000000, 16 MiB at 0xb432000000.
    let mut vmd = FakeFn::new(0x8086, 0xAD0B, 0x01_04_00, 0);
    vmd.cmd(0x6).bar64(0, 0xB4_3000_0000, 32 * MIB, true).bar32(2, 0x8C00_0000, 16 * MIB as u32, false).bar64(4, 0xB4_3200_0000, 16 * MIB, false);
    w.add(VMD.0, VMD.1, VMD.2, vmd);
    // The SPI flash controller right behind the first NVMe's window.
    let mut spi = FakeFn::new(0x8086, 0x7F24, 0x0C_80_00, 0);
    spi.cmd(0x2).bar32(0, 0x8D30_0000, 0x1000, false);
    w.add(0, 0x1F, 0, FakeFn::new(0x8086, 0xAE0D, 0x06_01_00, 0x80));
    w.add(0, 0x1F, 5, spi);
    w.add(0, 0x1B, 0, port(0x81, 0xB850_0000, None));
    let mut first = port(0x82, 0xB840_0000, Some(0x5000));
    first.w8(0x0E, 0x81);
    w.add(0, 0x1C, 0, first);
    w.add(0, 0x1C, 1, port(0x83, 0xB830_0000, Some(0x4000)));
    w.add(0, 0x1C, 2, port(0x84, 0xB820_0000, Some(0x3000)));
    w.add(0, 0x1C, 3, port(0x85, 0xB810_0000, None));
    w.add(0, 0x1D, 0, port(0x86, 0xB800_0000, None));
    w.add(NVME_CPU.0, 0, 0, nvme(0xA80D, 0x8D20_0000));
    w.add(NVME_LEXAR.0, 0, 0, nvme_of(0x1D97, 0x5236, 0xB850_0000));
    w.add(NVME_980.0, 0, 0, nvme(0xA80A, 0xB800_0000));
    w.add(SATA_A.0, 0, 0, sata(0x5000, 0xB840_0000));
    w.add(SATA_B.0, 0, 0, sata(0x3000, 0xB820_0000));
    // RTL8126: 256 ports, 64 KiB and 16 KiB.
    let mut nic = FakeFn::new(0x10EC, 0x8126, 0x02_00_00, 0);
    nic.cmd(0x7).bario(0, 0x4000, 0x100, true).bar64(2, 0xB830_0000, 0x1_0000, false).bar64(4, 0xB831_0000, 0x4000, false);
    w.add(NIC.0, 0, 0, nic);
    let mut wlan = FakeFn::new(0x8086, 0x272B, 0x02_80_00, 0);
    wlan.cmd(0x6).bar64(0, 0xB810_0000, 0x4000, false);
    w.add(WLAN.0, 0, 0, wlan);
    w
}

fn covered(r: &[BarRange], space: Space, start: u64, end: u64) -> bool {
    // Every byte of [start, end) lies in some range of that space (the ranges of one BAR never leave a hole).
    r.iter().any(|g| g.space == space && g.start <= start && end <= g.end)
}

fn hit(r: &[BarRange], space: Space, start: u64, end: u64) -> bool {
    r.iter().any(|g| g.space == space && g.start < end && start < g.end)
}

#[test]
fn untouched_storage_on_the_gaming_pc_is_covered_whole_and_no_neighbour_with_it() {
    // The profile: every class 01 function untouched; the network card as without gp_net (untouched too) and
    // as with it (sized): the answer must not depend on it.
    for net_off in [true, false] {
        let key = |n: &crate::node::Node| {
            if storage_kept_off(n.class, Some("off")) {
                Some("nvme_bind")
            } else {
                crate::gp::network_key(true, !net_off, n.class)
            }
        };
        let w = gaming_pc();
        let nodes = build(&w, &ALL, key);
        let storage: Vec<_> = nodes.iter().filter(|n| n.is_storage()).collect();
        assert_eq!(nodes.len(), 18, "the whole fake tree was walked");
        assert_eq!(storage.len(), 6);
        assert!(storage.iter().all(|n| n.untouched == Some("nvme_bind") && n.bars.is_none()));
        assert!(storage_ranges(&nodes).is_err(), "without a header reader the old rule stands");
        let read = |n: &crate::node::Node, o: u8| w.read32(n.loc, o as u16);
        let r = storage_ranges_with(&nodes, Some(&read)).expect("untouched storage is read from the header, not refused");

        // Every real BAR, whole. VMD's 32 MiB BAR is the one a 16 MiB bound cut in half.
        let mem = [
            (0xB4_3000_0000u64, 32 * MIB),
            (0x8C00_0000, 16 * MIB),
            (0xB4_3200_0000, 16 * MIB),
            (0x8D20_0000, 0x4000),
            (0xB850_0000, 0x4000),
            (0xB800_0000, 0x4000),
            (0xB840_0000, 0x200),
            (0xB820_0000, 0x200),
        ];
        for (start, size) in mem {
            assert!(covered(&r, Space::Memory, start, start + size), "{start:#x}+{size:#x} is not wholly covered (net_off={net_off})");
        }
        assert!(covered(&r, Space::Memory, 0xB4_3100_0000, 0xB4_3200_0000), "the upper half of VMD's configuration BAR");
        for base in [0x5000u64, 0x3000] {
            for (off, size) in [(0x50, 8), (0x40, 4), (0x30, 8), (0x20, 4), (0, 0x20)] {
                assert!(covered(&r, Space::Io, base + off, base + off + size), "ports {:#x}+{size}", base + off);
            }
        }

        // No neighbour is taken for a disk controller: the SPI controller behind 01:00.0's window, the network
        // card and the WLAN card behind the ports next to 86:00.0's, the fixed chipset range.
        for (what, start, size) in [
            ("SPI 00:1f.5", 0x8D30_0000u64, 0x1000u64),
            ("RTL8126 BAR2", 0xB830_0000, 0x1_0000),
            ("RTL8126 BAR4", 0xB831_0000, 0x4000),
            ("WLAN BAR0", 0xB810_0000, 0x4000),
            ("I/O APIC", 0xFEC0_0000, 0x1000),
            ("HPET", 0xFED0_0000, 0x400),
            ("local APIC", 0xFEE0_0000, 0x1000),
        ] {
            assert!(!hit(&r, Space::Memory, start, start + size), "{what} would be refused as a disk controller (net_off={net_off})");
        }
        assert!(!hit(&r, Space::Io, 0x4000, 0x4100), "the network card's ports");
        // The PM timer and reset ports of this board (FADT: 0x1808, 0xCF9) and the legacy ranges stay free.
        for port in [0x1808u64, 0x0CF9, 0x0070, 0x0080] {
            assert!(!hit(&r, Space::Io, port, port + 4), "port {port:#x}");
        }
        // A function behind a port never reaches past that port's window.
        for g in r.iter().filter(|g| g.space == Space::Memory && (0xB800_0000..0xB860_0000).contains(&g.start)) {
            assert!(g.end <= (g.start & !(MIB - 1)) + MIB, "{:#x}..{:#x} leaves its 1 MiB window", g.start, g.end);
        }
        // Read only: nothing was written to any storage function for it.
        for (b, d, f) in [VMD, NVME_CPU, NVME_LEXAR, NVME_980, SATA_A, SATA_B] {
            assert_eq!(w.writes_to(b, d, f), 0, "{b:02x}:{d:02x}.{f} was written");
        }
    }
}

/// A root-bus BAR is cut by the next address the tree knows, never by a fixed size, and never reaches the fixed
/// chipset range; an address the bridge above does not forward keeps the old bound.
#[test]
fn extents_end_at_the_next_known_address() {
    let w = Fake::new();
    w.add(0, 0, 0, FakeFn::new(0x8086, 0x7D1B, 0x06_00_00, 0));
    // A RAID controller on the root bus with a BAR at 2 GiB (alignment 2 GiB) and one at 0xFE000000.
    let mut raid = FakeFn::new(0x8086, 0x2826, 0x01_04_00, 0);
    raid.cmd(0x6).bar32(0, 0x8000_0000, 0x8_0000, false).bar32(1, 0xFE00_0000, 0x2000, false);
    w.add(0, 0x17, 0, raid);
    let mut other = FakeFn::new(0x8086, 0x1234, 0x0C_03_30, 0);
    other.cmd(0x6).bar32(0, 0x9000_0000, 0x1_0000, false);
    w.add(0, 0x14, 0, other);
    // A port with no window at all (all zero) above an NVMe whose BAR the firmware left: not forwarded.
    w.add(0, 1, 0, bridge(0x8086, 0x7F38, 0, 1, 1));
    w.add(1, 0, 0, nvme(0xA80A, 0xA000_0000));
    let key = |n: &crate::node::Node| if storage_kept_off(n.class, Some("off")) { Some("nvme_bind") } else { crate::gp::network_key(true, true, n.class) };
    let nodes = build(&w, &ALL, key);
    let read = |n: &crate::node::Node, o: u8| w.read32(n.loc, o as u16);
    let r = storage_ranges_with(&nodes, Some(&read)).unwrap();
    let range = |start: u64| r.iter().find(|g| g.start == start).map(|g| g.end).unwrap();
    assert_eq!(range(0x8000_0000), 0x9000_0000, "up to the xHCI BAR, not 2 GiB and not 16 MiB");
    assert_eq!(range(0xFE00_0000), 0xFEC0_0000, "up to the fixed chipset range");
    assert_eq!(range(0xA000_0000), 0xA000_0000 + acpi::guard::BAR_GUESS_MAX, "not forwarded: the first-scan bound");
    assert_eq!(r.len(), 3);
}

/// GP-DISK: with `gp_nvme=81:00.0/1d97:5236` the Lexar and its root port are scanned and sized as on any machine;
/// the two Samsungs, both SATA controllers and VMD stay untouched with their ports, nothing is written to them, and
/// the ACPI guard still covers them. Any field of the line wrong: the tree is what `nvme_bind=off` alone makes.
#[test]
fn gp_nvme_releases_the_lexar_alone() {
    let key_for = |line: &'static str| {
        move |n: &crate::node::Node| {
            let place = Some((n.loc.bus, n.loc.dev, n.loc.func));
            if storage_kept_off_at(n.class, Some("off"), Some(line), place, (n.vendor, n.device)) {
                Some("nvme_bind")
            } else if storage_kept_off(n.class, Some("off")) {
                None
            } else {
                crate::gp::network_key(true, true, n.class)
            }
        }
    };
    let w = gaming_pc();
    let nodes = build(&w, &ALL, key_for("81:00.0/1d97:5236"));
    let at = |p: (u8, u8, u8)| nodes.iter().find(|n| (n.loc.bus, n.loc.dev, n.loc.func) == p).unwrap();
    let lexar = at(NVME_LEXAR);
    assert!(lexar.untouched.is_none() && lexar.bars.is_some(), "the named controller is scanned and sized");
    assert!(at((0, 0x1B, 0)).untouched.is_none(), "its root port too");
    for p in [VMD, NVME_CPU, NVME_980, SATA_A, SATA_B] {
        let n = at(p);
        assert_eq!(n.untouched, Some("nvme_bind"), "{}", n.loc);
        assert!(n.bars.is_none());
        assert_eq!(w.writes_to(p.0, p.1, p.2), 0, "{} was written", n.loc);
    }
    for port in [(0, 1, 0), (0, 0x1D, 0), (0, 0x1C, 0), (0, 0x1C, 2)] {
        assert_eq!(at(port).untouched, Some("nvme_bind"), "the port above an untouched disk {port:?}");
    }
    let read = |n: &crate::node::Node, o: u8| w.read32(n.loc, o as u16);
    let r = storage_ranges_with(&nodes, Some(&read)).expect("the guard view holds with one sized controller");
    for (start, size) in [(0x8D20_0000u64, 0x4000u64), (0xB800_0000, 0x4000), (0xB850_0000, 0x4000)] {
        assert!(covered(&r, Space::Memory, start, start + size), "{start:#x}");
    }
    // Wrong in any field, or naming a Samsung: as without the line.
    let all_off = build(&gaming_pc(), &ALL, |n: &crate::node::Node| if storage_kept_off(n.class, Some("off")) { Some("nvme_bind") } else { crate::gp::network_key(true, true, n.class) });
    for line in ["81:00.0/1d97:5237", "82:00.0/1d97:5236", "81:00.1/1d97:5236", "86:00.0/144d:a80a", "01:00.0/144d:a80d", "81:00.0", "on"] {
        let w = gaming_pc();
        let nodes = build(&w, &ALL, key_for(line));
        for (a, b) in nodes.iter().zip(&all_off) {
            assert_eq!((a.loc, a.untouched, a.bars.is_some()), (b.loc, b.untouched, b.bars.is_some()), "{line}");
        }
        for p in [VMD, NVME_CPU, NVME_LEXAR, NVME_980, SATA_A, SATA_B] {
            assert_eq!(w.writes_to(p.0, p.1, p.2), 0, "{line}: {p:?} was written");
        }
    }
    // The plain rule without a place is the old one.
    assert!(storage_kept_off_at(0x01_08_02, Some("off"), Some("81:00.0/1d97:5236"), None, (0x1D97, 0x5236)));
    assert!(!storage_kept_off_at(0x01_08_02, None, Some("81:00.0/1d97:5236"), Some((0x81, 0, 0)), (0x1D97, 0x5236)), "the Mini-PC: no line, nothing kept");
}
