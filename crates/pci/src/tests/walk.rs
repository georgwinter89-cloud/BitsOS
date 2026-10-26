//! The walk, BARs, capabilities, the untouched rule and the tree on well-formed spaces.

use super::fake::{bridge, root_port, Fake, FakeFn};
use super::world::{self, minipc, GPU, I226, NVME0, WLAN, WLAN_BRIDGE};
use super::{find, keys_off, no_keys, ALL};
use crate::caps::{ext, id, CapInfo, ExtInfo};
use crate::enumerate::Root;
use crate::node::{ancestors, Bar, Problem};
use crate::{build, guardview, optin, tree, Loc};
use alloc::vec::Vec;

#[test]
fn minipc_tree() {
    let w = minipc();
    let nodes = build(&w, &ALL, keys_off);
    assert_eq!(nodes.len(), world::MINIPC_COUNT, "{:#?}", nodes.iter().map(|n| n.loc).collect::<Vec<_>>());
    // Ghosts with vendor 0x0000 are absent; their siblings are there.
    assert!(find(&nodes, 0, 3, 1).is_some());
    for func in 2..=4 {
        assert!(find(&nodes, 0, 3, func).is_none(), "ghost 00:03.{func} listed");
    }
    // Bridges as the firmware numbered them, parents right.
    let rp = find(&nodes, 0, 2, 1).unwrap();
    assert_eq!(rp.bridge.map(|b| (b.secondary, b.subordinate)), Some((3, 3)));
    let nic = find(&nodes, I226.0, 0, 0).unwrap();
    assert_eq!(nodes[nic.parent.unwrap()].loc, Loc::new(0, 0, 2, 1));
    let gpu_i = nodes.iter().position(|n| n.loc == Loc::new(0, GPU.0, 0, 0)).unwrap();
    assert_eq!(ancestors(&nodes, gpu_i).iter().map(|&i| nodes[i].loc).collect::<Vec<_>>(), [Loc::new(0, 0, 8, 1)]);
    // A bridge always comes before what sits below it.
    for (i, n) in nodes.iter().enumerate() {
        if let Some(p) = n.parent {
            assert!(p < i);
            let b = nodes[p].bridge.unwrap();
            assert!((b.secondary..=b.subordinate).contains(&n.loc.bus));
        }
    }
    assert!(nodes.iter().all(|n| n.problems.is_empty()), "{:?}", nodes.iter().filter(|n| !n.problems.is_empty()).map(|n| (n.loc, &n.problems)).collect::<Vec<_>>());
}

#[test]
fn bars_64bit_above_4g_prefetch_and_io() {
    let w = minipc();
    let nodes = build(&w, &ALL, keys_off);
    let gpu = find(&nodes, GPU.0, 0, 0).unwrap();
    let bars = gpu.bars.unwrap();
    assert_eq!(bars[0], Some(Bar::Mem { phys: 0x7C_0000_0000, size: 0x1000_0000, prefetch: true, is64: true }));
    assert_eq!(bars[1], None, "upper half of a 64-bit BAR");
    assert_eq!(bars[2], Some(Bar::Mem { phys: 0x7C_1000_0000, size: 0x20_0000, prefetch: true, is64: true }));
    assert_eq!(bars[4], Some(Bar::Io { port: 0xE000, size: 0x100 }));
    assert_eq!(bars[5], Some(Bar::Mem { phys: 0xDC50_0000, size: 0x8_0000, prefetch: false, is64: false }));
    // The contract view filters nothing that is assigned.
    let f = gpu.function();
    assert_eq!(f.bar(0).map(|b| b.size()), Some(0x1000_0000));
    assert_eq!(f.bar(1), None);
    assert_eq!(f.bar(6), None);
    let nvme = find(&nodes, NVME0.0, 0, 0).unwrap();
    assert_eq!(nvme.bars.unwrap()[0], Some(Bar::Mem { phys: 0xDCF0_0000, size: 0x4000, prefetch: false, is64: true }));
}

#[test]
fn sizing_turns_decode_off_and_restores_everything() {
    let w = minipc();
    let before = w.snapshot();
    let nodes = build(&w, &ALL, keys_off);
    // Every register is as it was (BARs, command), on every function.
    assert_eq!(w.snapshot(), before);
    for (at, off, cmd) in w.cmd_at_bar_write.lock().unwrap().iter() {
        let n = nodes.iter().find(|n| n.loc == *at).unwrap();
        if n.class >> 8 != 0x0600 {
            assert_eq!(cmd & 3, 0, "{at} BAR {off:#x} written with decoding on");
        }
    }
    // The GPU had decoding on: its command register went off and back.
    let gpu_cmd: Vec<u32> = w.writes.lock().unwrap().iter().filter(|x| x.0 == Loc::new(0, GPU.0, 0, 0) && x.1 == 4).map(|x| x.2).collect();
    assert_eq!(gpu_cmd, [0x4, 0x7]);
    // A function with decoding already off gets no command write at all.
    assert!(!w.writes.lock().unwrap().iter().any(|x| x.0 == Loc::new(0, I226.0, 0, 0) && x.1 == 4));
}

#[test]
fn host_bridge_keeps_decoding() {
    let w = Fake::new();
    let mut hb = FakeFn::new(0x8086, 0x29C0, 0x06_00_00, 0);
    hb.cmd(0x7).bar32(0, 0xFED0_0000, 0x1000, false);
    w.add(0, 0, 0, hb);
    let nodes = build(&w, &ALL, no_keys);
    assert_eq!(nodes[0].bars.unwrap()[0].map(|b| b.size()), Some(0x1000));
    assert!(!w.writes.lock().unwrap().iter().any(|x| x.1 == 4));
}

#[test]
fn wlan_and_its_bridge_untouched() {
    let w = minipc();
    let nodes = build(&w, &ALL, keys_off);
    let wl = find(&nodes, WLAN.0, 0, 0).unwrap();
    assert_eq!(wl.untouched, Some("wlan"));
    assert_eq!(wl.bars, None);
    let br = find(&nodes, WLAN_BRIDGE.0, WLAN_BRIDGE.1, WLAN_BRIDGE.2).unwrap();
    assert_eq!(br.untouched, Some("wlan"));
    assert_eq!(w.writes_to(WLAN.0, 0, 0), 0);
    assert_eq!(w.writes_to(WLAN_BRIDGE.0, WLAN_BRIDGE.1, WLAN_BRIDGE.2), 0);
    // Its neighbours are sized as usual.
    assert!(find(&nodes, 0, 2, 1).unwrap().bars.is_some());
    assert!(w.writes_to(I226.0, 0, 0) > 0);
    // The contract view refuses: no BAR, and a write is refused before reaching the device.
    let f = wl.function();
    assert_eq!(f.untouched(), Some("wlan"));
    assert_eq!(f.bar(2), None);
    assert!(!f.write32_ext(4, 0x6));
    // With the key on (no key returned) the WLAN is sized.
    let w2 = minipc();
    let nodes = build(&w2, &ALL, no_keys);
    assert_eq!(find(&nodes, WLAN.0, 0, 0).unwrap().bars.unwrap()[2].map(|b| b.size()), Some(0x10_0000));
    assert_eq!(find(&nodes, WLAN.0, 0, 0).unwrap().bars.unwrap()[0], Some(Bar::Io { port: 0xF000, size: 0x100 }));
}

#[test]
fn bridge_untouched_only_when_everything_below_is() {
    // Switch: root port -> upstream port -> two downstream ports -> WLAN and a NIC.
    let w = Fake::new();
    w.add(0, 0, 0, FakeFn::new(0x8086, 0x29C0, 0x06_00_00, 0));
    w.add(0, 1, 0, root_port(0, 1, 4));
    let mut up = bridge(0x10B5, 0x8717, 1, 2, 4);
    up.pcie(0x68, 5, 0);
    w.add(1, 0, 0, up);
    let mut dn0 = bridge(0x10B5, 0x8717, 2, 3, 3);
    dn0.pcie(0x68, 6, 0);
    let mut dn1 = bridge(0x10B5, 0x8717, 2, 4, 4);
    dn1.pcie(0x68, 6, 0);
    w.add(2, 0, 0, dn0);
    w.add(2, 1, 0, dn1);
    w.add(3, 0, 0, FakeFn::new(0x8086, 0x2725, 0x02_80_00, 0));
    w.add(4, 0, 0, FakeFn::new(0x8086, 0x125C, 0x02_00_00, 0));
    let nodes = build(&w, &ALL, keys_off);
    let key = |b, d, f| find(&nodes, b, d, f).unwrap().untouched;
    assert_eq!(key(3, 0, 0), Some("wlan"));
    assert_eq!(key(2, 0, 0), Some("wlan"), "downstream port above only the WLAN");
    assert_eq!(key(2, 1, 0), None);
    assert_eq!(key(1, 0, 0), None, "upstream port also serves the NIC");
    assert_eq!(key(0, 1, 0), None);
    // An empty bridge is not untouched (nothing below it to protect).
    let w = Fake::new();
    w.add(0, 1, 0, root_port(0, 1, 1));
    assert_eq!(build(&w, &ALL, keys_off)[0].untouched, None);
}

#[test]
fn multifunction_needs_bit7_of_function_0() {
    let w = Fake::new();
    w.add(0, 5, 0, FakeFn::new(0x1AF4, 0x1000, 0x02_00_00, 0));
    w.add(0, 5, 1, FakeFn::new(0x1AF4, 0x1001, 0x01_00_00, 0));
    w.add(0, 6, 0, FakeFn::new(0x1022, 0x14EA, 0x06_00_00, 0x80));
    w.add(0, 6, 7, FakeFn::new(0x1022, 0x14EB, 0x08_80_00, 0));
    // Function 0 missing: the device is absent even if function 1 answers (PCI 3.0 §6.2.1).
    w.add(0, 7, 1, FakeFn::new(0x1022, 0x14EB, 0x08_80_00, 0));
    let nodes = build(&w, &ALL, no_keys);
    let locs: Vec<Loc> = nodes.iter().map(|n| n.loc).collect();
    assert_eq!(locs, [Loc::new(0, 0, 5, 0), Loc::new(0, 0, 6, 0), Loc::new(0, 0, 6, 7)]);
}

#[test]
fn only_device_0_below_root_and_downstream_ports() {
    let w = Fake::new();
    w.add(0, 1, 0, root_port(0, 1, 1));
    // A device that answers on every device number (Linux `only_one_child` exists for these).
    for dev in 0..32 {
        w.add(1, dev, 0, FakeFn::new(0x8086, 0x10D3, 0x02_00_00, 0));
    }
    // A conventional bridge (no PCIe capability): all 32 device numbers are scanned.
    w.add(0, 2, 0, bridge(0x8086, 0x244E, 0, 2, 2));
    w.add(2, 0, 0, FakeFn::new(0x8086, 0x100E, 0x02_00_00, 0));
    w.add(2, 3, 0, FakeFn::new(0x8086, 0x100E, 0x02_00_00, 0));
    let nodes = build(&w, &ALL, no_keys);
    assert_eq!(nodes.iter().filter(|n| n.loc.bus == 1).count(), 1);
    assert_eq!(nodes.iter().filter(|n| n.loc.bus == 2).count(), 2);
}

#[test]
fn bridge_chain_and_windows() {
    // pcie root port -> pcie-to-pci bridge -> pci bridge -> pci bridge -> device with a 1 GiB 64-bit BAR.
    let w = Fake::new();
    w.add(0, 0, 0, FakeFn::new(0x8086, 0x29C0, 0x06_00_00, 0));
    let mut rp = root_port(0, 1, 4);
    rp.w32(0x20, 0xFE00_FC00).w32(0x24, 0xFFF1_0001).w32(0x28, 0x1).w32(0x2C, 0x1).w16(0x1C, 0xF0F0).w32(0x30, 0);
    w.add(0, 1, 0, rp);
    let mut pb = bridge(0x1B36, 0x000E, 1, 2, 4);
    pb.pcie(0x48, 7, 0);
    w.add(1, 0, 0, pb);
    w.add(2, 1, 0, bridge(0x1B36, 0x0001, 2, 3, 4));
    w.add(3, 1, 0, bridge(0x1B36, 0x0001, 3, 4, 4));
    let mut td = FakeFn::new(0x1B36, 0x0005, 0x00_FF_00, 0);
    td.cmd(0x3).bar32(0, 0xFE00_0000, 0x1000, false).bario(1, 0xC000, 0x100, false).bar64(2, 0x1_8000_0000, 0x4000_0000, true);
    w.add(4, 2, 0, td);
    let nodes = build(&w, &ALL, no_keys);
    let i = nodes.iter().position(|n| n.loc == Loc::new(0, 4, 2, 0)).expect("device at the end of the chain");
    let chain: Vec<Loc> = ancestors(&nodes, i).iter().map(|&j| nodes[j].loc).collect();
    assert_eq!(chain, [Loc::new(0, 3, 1, 0), Loc::new(0, 2, 1, 0), Loc::new(0, 1, 0, 0), Loc::new(0, 0, 1, 0)]);
    let b = nodes[i].bars.unwrap();
    assert_eq!(b[2], Some(Bar::Mem { phys: 0x1_8000_0000, size: 0x4000_0000, prefetch: true, is64: true }));
    assert_eq!(b[1], Some(Bar::Io { port: 0xC000, size: 0x100 }));
    let rp = find(&nodes, 0, 1, 0).unwrap().bridge.unwrap();
    assert_eq!(rp.mem.map(|m| (m.start, m.end)), Some((0xFC00_0000, 0xFE0F_FFFF)));
    assert_eq!(rp.prefetch.map(|m| (m.start, m.end)), Some((0x1_0000_0000, 0x1_FFFF_FFFF)));
    assert_eq!(rp.io.map(|m| (m.start, m.end)), Some((0xF000, 0xFFFF)));
    // A PCIe-to-PCI bridge's bus is conventional: all device numbers scanned (device 1 found).
    assert!(find(&nodes, 2, 1, 0).is_some());
    let text = tree::lines(&nodes, "fake").join("\n");
    assert!(text.contains("    window mem 0xfc000000-0xfe0fffff prefetch 0x100000000-0x1ffffffff io 0xf000-0xffff"), "{text}");
}

#[test]
fn bars_of_every_kind() {
    let w = Fake::new();
    let mut f = FakeFn::new(0x1234, 0x5678, 0x0B_40_00, 0);
    f.cmd(0x3);
    // slot 0: 8 GiB 64-bit (lower dword has no writable address bits), slot 2: unimplemented,
    // slot 3: unassigned 32-bit (address 0), slot 4: 16-bit I/O decoder, slot 5: reserved type 3.
    f.bar64(0, 0x40_0000_0000, 0x2_0000_0000, true);
    f.bar32(3, 0, 0x1000, false);
    f.bario(4, 0x1000, 0x20, true);
    f.w32(0x24, 0x6);
    f.bar_mask[5] = 0xFFFF_F000;
    w.add(0, 1, 0, f);
    let mut g = FakeFn::new(0x1234, 0x5679, 0x0B_40_00, 0);
    g.bar32(0, 0xF000_0000, 0x1000, false);
    // 64-bit BAR in the last slot.
    g.w32(0x24, 0x4);
    g.bar_mask[5] = 0xFFFF_F000;
    w.add(0, 2, 0, g);
    let mut h = FakeFn::new(0x1234, 0x567A, 0x0B_40_00, 0);
    h.w32(0x10, 0x1_0001);
    h.bar_mask[0] = 0xFFFF_FFF0;
    w.add(0, 3, 0, h);
    let nodes = build(&w, &ALL, no_keys);
    let b = nodes[0].bars.unwrap();
    assert_eq!(b[0], Some(Bar::Mem { phys: 0x40_0000_0000, size: 0x2_0000_0000, prefetch: true, is64: true }));
    assert_eq!(b[2], None);
    assert_eq!(b[3], Some(Bar::Mem { phys: 0, size: 0x1000, prefetch: false, is64: false }));
    assert_eq!(nodes[0].function().bar(3), None, "unassigned BARs are not handed to drivers");
    assert_eq!(b[4], Some(Bar::Io { port: 0x1000, size: 0x20 }));
    assert_eq!(b[5], None);
    assert!(nodes[0].problems.contains(&Problem::BarReservedType { index: 5 }));
    assert!(nodes[1].problems.contains(&Problem::Bar64InLastSlot { index: 5 }));
    assert_eq!(nodes[1].bars.unwrap()[0].map(|b| b.size()), Some(0x1000));
    assert!(nodes[2].problems.contains(&Problem::BarIoTooHigh { index: 0, addr: 0x1_0000 }));
}

#[test]
fn capabilities_decoded() {
    let w = minipc();
    let nodes = build(&w, &ALL, keys_off);
    let nic = find(&nodes, I226.0, 0, 0).unwrap();
    let ids: Vec<(u8, u8)> = nic.caps.iter().map(|c| (c.offset, c.id)).collect();
    assert_eq!(ids, [(0x40, id::PM), (0x50, id::MSI), (0x70, id::MSIX), (0xA0, id::PCIE)]);
    assert_eq!(nic.cap(id::MSI).unwrap().info, CapInfo::Msi { enabled: false, is64: true, per_vector_mask: true, vectors: 16 });
    assert!(matches!(nic.cap(id::MSIX).unwrap().info, CapInfo::MsiX { table_size: 5, table_bir: 3, table_offset: 0, pba_bir: 3, pba_offset: 0x2000, .. }));
    assert_eq!(nic.pcie.unwrap().port_type, 0);
    assert_eq!((nic.pcie.unwrap().link_speed, nic.pcie.unwrap().link_width), (3, 4));
    let e: Vec<u16> = nic.ext_caps.iter().map(|c| c.id).collect();
    assert_eq!(e, [ext::AER, ext::DSN, ext::L1SS, ext::PTM]);
    assert!(matches!(nic.ext_cap(ext::L1SS).unwrap().info, ExtInfo::L1ss { ctrl1: 5, .. }));
    assert!(matches!(nic.ext_cap(ext::PTM).unwrap().info, ExtInfo::Ptm { .. }));
    assert!(matches!(nic.ext_cap(ext::AER).unwrap().info, ExtInfo::Aer { .. }));
    // The WLAN's capabilities are read (reads are allowed) though it is untouched.
    assert!(find(&nodes, WLAN.0, 0, 0).unwrap().cap(id::MSI).is_some());
    // MSI-X inside its BAR: no problem on the NIC or the NVMe.
    assert!(nic.problems.is_empty());
}

#[test]
fn legacy_space_has_no_extended_list() {
    let w = Fake::legacy();
    let mut f = FakeFn::new(0x8086, 0x125C, 0x02_00_00, 0);
    f.pcie(0x40, 0, 0).ext(0x100, ext::AER, 2, 0);
    w.add(0, 1, 0, f);
    let nodes = build(&w, &ALL, no_keys);
    assert_eq!(nodes[0].space, 0x100);
    assert!(nodes[0].ext_caps.is_empty());
    assert!(nodes[0].pcie.is_some());
}

#[test]
fn several_segments_and_root_ranges() {
    let w = Fake::new();
    w.add(0, 0, 0, FakeFn::new(0x8086, 0x29C0, 0x06_00_00, 0));
    w.add_at(Loc::new(1, 0x40, 0, 0), FakeFn::new(0x8086, 0x0001, 0x06_00_00, 0));
    // The fake reaches only segment 0, so segment 1 is listed as a root but answers nothing: no node, no panic.
    let roots = [Root { segment: 0, bus_start: 0, bus_end: 0x3F }, Root { segment: 1, bus_start: 0x40, bus_end: 0x7F }];
    let nodes = build(&w, &roots, no_keys);
    assert_eq!(nodes.len(), 1);
    assert_eq!(alloc::format!("{}", Loc::new(1, 0x40, 2, 3)), "0001:40:02.3");
    assert_eq!(alloc::format!("{}", Loc::new(0, 0xC7, 0, 6)), "c7:00.6");
}

#[test]
fn guard_view_has_real_storage_bars() {
    let w = minipc();
    let nodes = build(&w, &ALL, keys_off);
    let r = guardview::storage_ranges(&nodes).unwrap();
    assert_eq!(r.len(), 2);
    assert!(r.iter().any(|b| b.start == 0xDCF0_0000 && b.end == 0xDCF0_4000));
    let mut no_bars = nodes.clone();
    no_bars.iter_mut().filter(|n| n.is_storage()).for_each(|n| n.bars = None);
    assert!(guardview::storage_ranges(&no_bars).is_err());
}

#[test]
fn storage_binds_only_to_nvme() {
    let w = minipc();
    let nodes = build(&w, &ALL, keys_off);
    let nvme = find(&nodes, NVME0.0, 0, 0).unwrap();
    assert!(optin::may_bind("nvme", nvme));
    assert!(!optin::may_bind("ktest-storage", nvme));
    assert!(!optin::may_bind("usb-msd", nvme));
    assert!(optin::may_bind("igc", find(&nodes, I226.0, 0, 0).unwrap()));
}

#[test]
fn tree_text() {
    let w = minipc();
    let nodes = build(&w, &ALL, keys_off);
    let lines = tree::lines(&nodes, "fake");
    assert!(lines[0].starts_with("pci: 32 functions"), "{}", lines[0]);
    let text = lines.join("\n");
    assert!(text.contains("\n  04:00.0 [10ec:b852] 028000 network (wlan), untouched until wlan=on"), "{text}");
    assert!(text.contains("00:02.2 [1022:14ee] 060400 pci bridge -> bus 04-04, root port link 8GT/s x4, untouched until wlan=on"), "{text}");
    assert!(text.contains("      bar0 mem 0x7c00000000 256M 64bit prefetch"), "{text}");
    assert!(text.contains("caps pm(D0) msi(16) msix(5) pcie aer dsn l1ss@0x1c0(0x5) ptm(off)"), "{text}");
    assert!(!text.contains("00:03.2"));
}

#[test]
fn mcfg_windows_checked_and_deduplicated() {
    use crate::ecam::EcamSpace;
    use acpi::mcfg::Ecam;
    let e = |base, segment, bus_start, bus_end| Ecam { base, segment, bus_start, bus_end };
    let given = [
        e(0xE000_0000, 0, 0, 255),
        e(0xF_F000_0000, 1, 0, 255),       // ends exactly at 1 << 36: fits
        e(0x80_0000_0000, 2, 0, 0),        // 1 << 39: past 39 bits
        e(0xE008_0000, 3, 0, 0),           // not 1 MiB aligned
        e(u64::MAX - 0xF_FFFF, 4, 0, 255), // overflows
    ];
    let (ok, refused) = crate::usable_windows(&given, 39);
    assert_eq!(ok, [given[0], given[1]]);
    assert_eq!(refused.len(), 3);
    // Overlapping bus ranges of one segment: the first wins; another segment may reuse the numbers.
    let s = EcamSpace::new(&[e(0xE000_0000, 0, 0, 127), e(0xD000_0000, 0, 64, 255), e(0xC000_0000, 1, 0, 255), e(0xB000_0000, 0, 128, 255)]);
    assert_eq!(s.windows(), [e(0xE000_0000, 0, 0, 127), e(0xC000_0000, 1, 0, 255), e(0xB000_0000, 0, 128, 255)]);
    use crate::ConfigAccess;
    assert_eq!((s.space_size(0, 200), s.space_size(2, 0)), (0x1000, 0));
    // On the host nothing can be mapped: every read is "absent", no panic.
    assert_eq!(s.read32(Loc::new(0, 0, 0, 0), 0), u32::MAX);
    assert!(!s.write32(Loc::new(0, 0, 0, 0), 4, 0));
}
