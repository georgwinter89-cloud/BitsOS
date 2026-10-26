//! The PC-1 checker's findings: BARs the CPU cannot reach, conventional functions with junk at 0x100,
//! ports without BARs left alone, overlapping root ranges, functions not ready yet (CRS), D3hot, ARI.

use super::fake::{bridge, root_port, Fake, FakeFn};
use super::world::{minipc, WLAN_BRIDGE};
use super::{find, keys_off, no_keys, ALL};
use crate::caps::{ext, id};
use crate::enumerate::{self, Root, CRS_VENDOR};
use crate::node::{Bar, Problem};
use crate::{build, finish, tree, Loc};
use alloc::vec::Vec;

fn one(f: FakeFn) -> (Fake, crate::Node) {
    let w = Fake::new();
    w.add(0, 1, 0, f);
    let n = build(&w, &ALL, no_keys).remove(0);
    (w, n)
}

#[test]
fn bar_past_the_physical_address_bits_is_not_handed_out() {
    // 64-bit BAR whose end is exactly 2^64 (wraps), and one past 39 address bits.
    let mut f = FakeFn::new(0x1234, 0x2, 0x02_00_00, 0);
    f.bar64(0, 0xFFFF_FFFF_F000_0000, 0x1000_0000, true).bar64(2, 1 << 40, 0x4000, false).bar64(4, 0, 0x1000, false);
    let w = Fake::new();
    w.add(0, 1, 0, f);
    let nodes = finish(&w, enumerate::enumerate(&w, &ALL), no_keys, hal::mmio::phys_limit(39));
    let n = &nodes[0];
    let bars = n.bars.unwrap();
    assert_eq!(bars[0], None);
    assert_eq!(bars[2], None);
    assert!(n.problems.contains(&Problem::BarBeyondPhys { index: 0, phys: 0xFFFF_FFFF_F000_0000, size: 0x1000_0000 }));
    assert!(n.problems.contains(&Problem::BarBeyondPhys { index: 2, phys: 1 << 40, size: 0x4000 }));
    // Unassigned stays visible (and `bar()` filters it), no problem for it.
    assert_eq!(bars[4], Some(Bar::Mem { phys: 0, size: 0x1000, prefetch: false, is64: true }));
    assert_eq!(n.function().bar(0), None);
    assert_eq!(n.problems.len(), 2);
    // The same BAR at 2^40 is fine with 48 address bits.
    let nodes = finish(&w, enumerate::enumerate(&w, &ALL), no_keys, hal::mmio::phys_limit(48));
    assert_eq!(nodes[0].bars.unwrap()[2], Some(Bar::Mem { phys: 1 << 40, size: 0x4000, prefetch: false, is64: true }));
}

#[test]
fn conventional_function_has_no_extended_space() {
    // A conventional device behind a PCIe-to-PCI bridge with junk (or an alias of 0x00) at 0x100.
    let mut f = FakeFn::new(0x1234, 0x2, 0x02_00_00, 0);
    f.cap(0x40, id::PM, 0).w32(0x100, 0x1234_0001).w32(0x104, 0xFFFF_FFFF);
    let (_, n) = one(f);
    assert!(n.ext_caps.is_empty());
    assert_eq!(n.space, 0x100);
    assert!(n.problems.is_empty(), "{:?}", n.problems);
    assert_eq!(n.function().config_size(), 0x100);
    // No capability list at all: the same.
    let mut f = FakeFn::new(0x1234, 0x2, 0x02_00_00, 0);
    f.ext(0x100, ext::AER, 1, 0x100);
    let (_, n) = one(f);
    assert!(n.ext_caps.is_empty() && n.problems.is_empty());
    // PCI-X mode 2 (266 MHz capable) has the extended space; plain PCI-X has not.
    let mut f = FakeFn::new(0x1234, 0x2, 0x02_00_00, 0);
    f.cap(0x40, id::PCIX, 0).w32(0x44, 0x4000_0000).ext(0x100, ext::AER, 1, 0);
    let (_, n) = one(f);
    assert_eq!((n.space, n.ext_caps.len()), (0x1000, 1));
    let mut f = FakeFn::new(0x1234, 0x2, 0x02_00_00, 0);
    f.cap(0x40, id::PCIX, 0).ext(0x100, ext::AER, 1, 0);
    let (_, n) = one(f);
    assert_eq!((n.space, n.ext_caps.len()), (0x100, 0));
}

#[test]
fn ports_without_bars_are_not_written() {
    let w = minipc();
    let nodes = build(&w, &ALL, keys_off);
    // Every root port and internal bridge of the Mini-PC has two BAR registers reading 0: no write at all,
    // so none of them stops forwarding to the GPU or the NVMe during the scan.
    for n in nodes.iter().filter(|n| n.is_bridge()) {
        assert_eq!(w.writes_to(n.loc.bus, n.loc.dev, n.loc.func), 0, "{} written", n.loc);
        // Untouched (the WLAN's bridge) is not sized at all; the others are, with nothing found.
        let expect = if n.untouched.is_some() { None } else { Some([None; 6]) };
        assert_eq!(n.bars, expect, "{}", n.loc);
    }
    assert!(find(&nodes, WLAN_BRIDGE.0, WLAN_BRIDGE.1, WLAN_BRIDGE.2).unwrap().untouched.is_some());
    // A function with nothing implemented: the same.
    let (w, n) = one(FakeFn::new(0x1234, 0x2, 0x08_80_00, 0));
    assert_eq!(w.writes_to(0, 1, 0), 0);
    assert_eq!(n.bars, Some([None; 6]));
    // A bridge with a real BAR is still sized with decoding off.
    let mut b = root_port(0, 1, 1);
    b.bar32(0, 0xFE00_0000, 0x1000, false);
    let (w, n) = one(b);
    assert_eq!(n.bars.unwrap()[0], Some(Bar::Mem { phys: 0xFE00_0000, size: 0x1000, prefetch: false, is64: false }));
    assert!(w.cmd_at_bar_write.lock().unwrap().iter().all(|&(_, _, cmd)| cmd & 3 == 0));
}

#[test]
fn root_range_inside_a_walked_one_is_skipped() {
    let w = Fake::new();
    w.add(0, 0, 0, FakeFn::new(0x8086, 0x29C0, 0x06_00_00, 0));
    w.add(0, 1, 0, bridge(0x1B36, 0x000C, 0, 1, 1));
    w.add(1, 0, 0, FakeFn::new(0x8086, 0x10D3, 0x02_00_00, 0));
    // Bus 0x40 is reached by no bridge; a second range starting there is not a root of its own.
    w.add(0x40, 0, 0, FakeFn::new(0x8086, 0x10D3, 0x02_00_00, 0));
    let roots = [ALL[0], Root { segment: 0, bus_start: 0x40, bus_end: 0xFF }, ALL[0]];
    let nodes = build(&w, &roots, no_keys);
    assert_eq!(nodes.iter().map(|n| n.loc).collect::<Vec<_>>(), [Loc::new(0, 0, 0, 0), Loc::new(0, 0, 1, 0), Loc::new(0, 1, 0, 0)]);
    // Disjoint ranges (MCFG 0-0x3F and 0x40-0xFF) are both walked.
    let roots = [Root { segment: 0, bus_start: 0, bus_end: 0x3F }, Root { segment: 0, bus_start: 0x40, bus_end: 0xFF }];
    assert_eq!(build(&w, &roots, no_keys).len(), 4);
}

#[test]
fn function_not_ready_is_reported_never_listed() {
    let w = Fake::new();
    w.add(0, 0, 0, FakeFn::new(0x8086, 0x29C0, 0x06_00_00, 0));
    w.add(0, 2, 0, FakeFn::new(CRS_VENDOR, 0x1234, 0x02_00_00, 0));
    let mut mf = FakeFn::new(0x8086, 0x1234, 0x0C_03_30, 0x80);
    mf.cmd(0);
    w.add(0, 3, 0, mf);
    w.add(0, 3, 1, FakeFn::new(CRS_VENDOR, 0x5678, 0x0C_03_30, 0));
    let walk = enumerate::walk(&w, &ALL);
    assert_eq!(walk.not_ready, [Loc::new(0, 0, 2, 0), Loc::new(0, 0, 3, 1)]);
    assert!(walk.nodes.iter().all(|n| n.vendor != CRS_VENDOR));
    assert_eq!(walk.nodes.len(), 2);
    // Once the functions answer, the next walk has them.
    w.fns.lock().unwrap().get_mut(&Loc::new(0, 0, 2, 0)).unwrap().w16(0, 0x8086);
    w.fns.lock().unwrap().get_mut(&Loc::new(0, 0, 3, 1)).unwrap().w16(0, 0x8086);
    let walk = enumerate::walk(&w, &ALL);
    assert!(walk.not_ready.is_empty());
    assert_eq!(walk.nodes.len(), 4);
}

#[test]
fn d3hot_function_is_not_sized() {
    let mut f = FakeFn::new(0x1234, 0x2, 0x01_08_02, 0);
    f.cmd(0x6).bar64(0, 0xDCF0_0000, 0x4000, false).cap(0x40, id::PM, 0).w16(0x44, 3);
    let (w, n) = one(f);
    assert_eq!(n.bars, None);
    assert_eq!(n.problems, [Problem::BarPowerState { state: 3 }]);
    assert_eq!(w.writes_to(0, 1, 0), 0);
    // The guard refuses rather than guesses when a storage function's BARs are unknown.
    assert!(crate::guardview::storage_ranges(core::slice::from_ref(&n)).is_err());
    assert!(tree::lines(core::slice::from_ref(&n), "fake").iter().any(|l| l.contains("problem bar.power_state")));
}

#[test]
fn ari_behind_a_forwarding_port_is_noted() {
    let w = Fake::new();
    let mut rp = root_port(0, 1, 1);
    rp.w16(0x58 + 0x28, 1 << 5);
    w.add(0, 1, 0, rp);
    // ARI with no next function (QEMU's igb: one physical function): nothing is lost, nothing to note.
    let mut ep = FakeFn::new(0x8086, 0x10C9, 0x02_00_00, 0x80);
    ep.pcie(0x40, 0, 0).ext(0x100, ext::ARI, 1, 0);
    w.add(1, 0, 0, ep.clone());
    assert!(build(&w, &ALL, no_keys).iter().all(|n| n.problems.is_empty()));
    // Next function 1 (found by the multi-function scan), whose next is 9: function 9 is lost.
    ep.w32(0x104, 1 << 8);
    w.add(1, 0, 0, ep);
    let mut f1 = FakeFn::new(0x8086, 0x10C9, 0x02_00_00, 0);
    f1.pcie(0x40, 0, 0).ext(0x100, ext::ARI, 1, 0).w32(0x104, 9 << 8);
    w.add(1, 0, 1, f1);
    let nodes = build(&w, &ALL, no_keys);
    assert!(find(&nodes, 1, 0, 0).unwrap().problems.is_empty());
    assert_eq!(find(&nodes, 1, 0, 1).unwrap().problems, [Problem::AriNotEnumerated]);
    // Forwarding off (what client firmware leaves): nothing to note.
    w.fns.lock().unwrap().get_mut(&Loc::new(0, 0, 1, 0)).unwrap().w16(0x58 + 0x28, 0);
    assert!(build(&w, &ALL, no_keys).iter().all(|n| n.problems.is_empty()));
}

#[test]
fn dropped_subtree_shows_in_the_tree_text() {
    let w = Fake::new();
    w.add(0, 0, 0, FakeFn::new(0x8086, 0x29C0, 0x06_00_00, 0));
    w.add(0, 1, 0, root_port(0, 0, 0));
    let nodes = build(&w, &ALL, no_keys);
    let text = tree::lines(&nodes, "fake").join("\n");
    assert!(text.contains("problem bridge.unconfigured"), "{text}");
}
