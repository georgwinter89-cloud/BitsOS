//! PC-4 host tests: the defined driver order, the CRS loop, the rescan merge (never re-sizes a known
//! function), the /sys/bus/pci view and the driver note in the tree text.

use super::fake::{Fake, FakeFn};
use super::world::{minipc, GPU, I226, I226_UNUSED, NVME0, WLAN, WLAN_BRIDGE};
use super::{find, keys_off, ALL};
use crate::enumerate::{self, Walk};
use crate::registry::{duplicates, ordered, rank, Candidate};
use crate::rescan::{crs_loop, merge, same, CRS_POLL_NS, CRS_WAIT_NS};
use crate::sysfs::{self, FnInfo, SysKind};
use crate::{build, Loc, Match, PciFunction};
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::Cell;

fn ok(_: &PciFunction) -> Result<(), String> {
    Ok(())
}

fn cand(name: &'static str, matches: &'static [Match]) -> Candidate {
    Candidate { name, matches, opt_in_key: None, probe: ok }
}

const XHCI_CLASS: Match = Match::Class { class: 0x0C_03_30, mask: 0xFF_FF_FF };
const USB_CLASS: Match = Match::Class { class: 0x0C_03_00, mask: 0xFF_FF_00 };
const SERIAL_BUS: Match = Match::Class { class: 0x0C_00_00, mask: 0xFF_00_00 };
const QEMU_XHCI: Match = Match::Id { vendor: 0x1B36, device: 0x000D };

#[test]
fn rank_id_before_class_narrow_before_wide() {
    assert_eq!(rank(&[QEMU_XHCI], 0x1B36, 0x000D, 0x0C0330), Some((0, 0)));
    assert_eq!(rank(&[XHCI_CLASS], 0x1B36, 0x000D, 0x0C0330), Some((1, 8)));
    assert_eq!(rank(&[USB_CLASS], 0x1B36, 0x000D, 0x0C0330), Some((1, 16)));
    assert_eq!(rank(&[SERIAL_BUS], 0x1B36, 0x000D, 0x0C0330), Some((1, 24)));
    assert_eq!(rank(&[XHCI_CLASS], 0x1B36, 0x000D, 0x0C0320), None);
    // A driver with several matches ranks by its best one.
    assert_eq!(rank(&[SERIAL_BUS, QEMU_XHCI], 0x1B36, 0x000D, 0x0C0330), Some((0, 0)));
    // The upper byte of a mask beyond the 24-bit class does not count.
    assert_eq!(rank(&[Match::Class { class: 0x0C_03_30, mask: 0xFFFF_FFFF }], 0, 0, 0x0C0330), Some((1, 8)));
}

#[test]
fn ordered_is_independent_of_registry_order() {
    let a = [cand("wide", &[SERIAL_BUS]), cand("usb", &[USB_CLASS]), cand("xhci", &[XHCI_CLASS]), cand("qemu", &[QEMU_XHCI]), cand("other", &[Match::Id { vendor: 1, device: 2 }])];
    let mut b = a;
    b.reverse();
    for list in [&a[..], &b[..]] {
        let names: Vec<&str> = ordered(list, 0x1B36, 0x000D, 0x0C0330).iter().map(|(c, _)| c.name).collect();
        assert_eq!(names, ["qemu", "xhci", "usb", "wide"]);
    }
    // Same rank: by name.
    let t = [cand("zeta", &[XHCI_CLASS]), cand("alpha", &[XHCI_CLASS])];
    assert_eq!(ordered(&t, 0, 0, 0x0C0330).iter().map(|(c, _)| c.name).collect::<Vec<_>>(), ["alpha", "zeta"]);
    // Nothing matches: empty.
    assert!(ordered(&a, 0x8086, 0x125C, 0x020000).is_empty());
}

#[test]
fn duplicate_names_tried_once() {
    let t = [cand("igc", &[XHCI_CLASS]), cand("igc", &[QEMU_XHCI]), cand("xhci", &[XHCI_CLASS])];
    assert_eq!(duplicates(&t), ["igc"]);
    let o = ordered(&t, 0x1B36, 0x000D, 0x0C0330);
    assert_eq!(o.iter().map(|(c, r)| (c.name, *r)).collect::<Vec<_>>(), [("igc", (0, 0)), ("xhci", (1, 8))]);
    assert!(duplicates(&[cand("a", &[]), cand("b", &[])]).is_empty());
}

fn walk_with(nodes: usize, not_ready: usize) -> Walk {
    let mut w = Walk::default();
    let n = minipc();
    w.nodes = build(&n, &ALL, keys_off).into_iter().take(nodes).collect();
    w.not_ready = (0..not_ready).map(|i| Loc::new(0, 6, i as u8, 0)).collect();
    w
}

#[test]
fn crs_loop_sleeps_without_the_lock_until_ready() {
    let clock = Cell::new(0u64);
    let walks = Cell::new(0);
    let locked = Cell::new(false);
    let mut walk = || {
        locked.set(true);
        walks.set(walks.get() + 1);
        let w = walk_with(3, if walks.get() < 3 { 1 } else { 0 });
        locked.set(false);
        w
    };
    let mut slept = Vec::new();
    let mut wait = |ns: u64, nr: &[Loc]| {
        assert!(!locked.get(), "waited inside the walk's lock");
        assert_eq!(nr, [Loc::new(0, 6, 0, 0)]);
        slept.push(ns);
        clock.set(clock.get() + ns);
    };
    let r = crs_loop(&mut walk, &|| clock.get(), true, &mut wait);
    assert_eq!(walks.get(), 3);
    assert_eq!(slept, [CRS_POLL_NS, CRS_POLL_NS]);
    assert_eq!(r.waits, 2);
    assert!(r.timed_out.is_empty());
    assert_eq!(r.nodes.len(), 3);
}

#[test]
fn crs_loop_gives_up_at_the_deadline() {
    let clock = Cell::new(0u64);
    let mut walk = || walk_with(2, 2);
    let mut waits = 0;
    let mut wait = |ns: u64, _: &[Loc]| {
        waits += 1;
        clock.set(clock.get() + ns);
    };
    let r = crs_loop(&mut walk, &|| clock.get(), true, &mut wait);
    assert_eq!(waits as u64, CRS_WAIT_NS / CRS_POLL_NS);
    assert_eq!(r.timed_out.len(), 2);
    assert_eq!(r.nodes.len(), 2);
    // No clock: one walk, no wait.
    let mut never = |_: u64, _: &[Loc]| panic!("waited without a clock");
    let r = crs_loop(&mut || walk_with(2, 1), &|| 0, false, &mut never);
    assert_eq!((r.waits, r.timed_out.len()), (0, 1));
}

fn nic(bar0: u32) -> FakeFn {
    let mut f = FakeFn::new(0x8086, 0x10D3, 0x02_00_00, 0);
    f.cmd(0).bar32(0, bar0, 0x2_0000, false).cap(0x40, 0x01, 0);
    f
}

/// Boot scan, then a card appears on the empty bus 6 (below 00:02.5) and the unused I226 disappears.
#[test]
fn rescan_sizes_only_new_functions() {
    let w = minipc();
    let old = build(&w, &ALL, keys_off);
    w.writes.lock().unwrap().clear();
    w.add(6, 0, 0, nic(0xDD20_0000));
    w.fns.lock().unwrap().remove(&Loc::new(0, I226_UNUSED.0, 0, 0));
    let walked = enumerate::enumerate(&w, &ALL);
    let m = merge(&w, &old, walked, &[], keys_off, 1 << 48);
    let new_loc = Loc::new(0, 6, 0, 0);
    assert_eq!(m.added.iter().map(|&i| m.nodes[i].loc).collect::<Vec<_>>(), [new_loc]);
    assert_eq!(m.gone.iter().map(|n| n.loc).collect::<Vec<_>>(), [Loc::new(0, I226_UNUSED.0, 0, 0)]);
    assert_eq!((m.kept, m.sized), (old.len() - 1, 1));
    // Only the new function was written (the sizing protocol); not one write to a known function.
    let writes = w.writes.lock().unwrap().clone();
    assert!(!writes.is_empty());
    assert!(writes.iter().all(|x| x.0 == new_loc), "{:?}", writes.iter().filter(|x| x.0 != new_loc).collect::<Vec<_>>());
    let n = m.nodes.iter().find(|n| n.loc == new_loc).unwrap();
    assert_eq!(n.bars.unwrap()[0].map(|b| (b.start(), b.size())), Some((0xDD20_0000, 0x2_0000)));
    // Known functions keep their nodes; parents point into the new list correctly.
    for k in m.nodes.iter().filter(|n| n.loc != new_loc) {
        let o = old.iter().find(|o| o.loc == k.loc).unwrap();
        assert_eq!((k.bars, k.untouched, k.command_at_scan, &k.problems), (o.bars, o.untouched, o.command_at_scan, &o.problems));
    }
    for (i, n) in m.nodes.iter().enumerate() {
        if let Some(p) = n.parent {
            assert!(p < i);
            let b = m.nodes[p].bridge.unwrap();
            assert!((b.secondary..=b.subordinate).contains(&n.loc.bus), "{}", n.loc);
        }
    }
    assert_eq!(m.nodes[n.parent.unwrap()].loc, Loc::new(0, 0, 2, 5));
    // The WLAN and its bridge stay untouched.
    assert_eq!(find(&m.nodes, WLAN.0, WLAN.1, WLAN.2).unwrap().untouched, Some("wlan"));
    assert_eq!(find(&m.nodes, WLAN_BRIDGE.0, WLAN_BRIDGE.1, WLAN_BRIDGE.2).unwrap().untouched, Some("wlan"));
}

#[test]
fn rescan_of_an_unchanged_bus_writes_nothing() {
    let w = minipc();
    let old = build(&w, &ALL, keys_off);
    w.writes.lock().unwrap().clear();
    let m = merge(&w, &old, enumerate::enumerate(&w, &ALL), &[], keys_off, 1 << 48);
    assert!(m.added.is_empty() && m.gone.is_empty() && m.sized == 0);
    assert_eq!(m.nodes, old);
    assert!(w.writes.lock().unwrap().is_empty());
}

#[test]
fn rescan_keeps_untouched_state_whatever_appears() {
    // A second function appears below the WLAN's bridge (a multi-function card): the bridge stays untouched
    // (a fresh boot would mark it touched), the WLAN too, the new function is touched and sized.
    let w = minipc();
    let old = build(&w, &ALL, keys_off);
    {
        let mut fns = w.fns.lock().unwrap();
        let wl = fns.get_mut(&Loc::new(0, WLAN.0, 0, 0)).unwrap();
        wl.w8(0x0E, 0x80);
    }
    w.add(WLAN.0, 0, 1, nic(0xDD30_0000));
    w.writes.lock().unwrap().clear();
    let walked = enumerate::enumerate(&w, &ALL);
    // The header byte changed (multi-function bit): the WLAN itself counts as the same function.
    let m = merge(&w, &old, walked, &[], keys_off, 1 << 48);
    let added: Vec<Loc> = m.added.iter().map(|&i| m.nodes[i].loc).collect();
    assert_eq!(added, [Loc::new(0, WLAN.0, 0, 1)]);
    assert_eq!(find(&m.nodes, WLAN_BRIDGE.0, WLAN_BRIDGE.1, WLAN_BRIDGE.2).unwrap().untouched, Some("wlan"));
    assert_eq!(find(&m.nodes, WLAN.0, WLAN.1, WLAN.2).unwrap().untouched, Some("wlan"));
    assert_eq!(find(&m.nodes, WLAN.0, 0, 1).unwrap().untouched, None);
    assert_eq!(w.writes_to(WLAN.0, 0, 0) + w.writes_to(WLAN_BRIDGE.0, WLAN_BRIDGE.1, WLAN_BRIDGE.2), 0);
    // And a new WLAN card elsewhere is untouched from the start.
    let mut wl2 = FakeFn::new(0x10EC, 0xB852, 0x02_80_00, 0);
    wl2.bar64(2, 0xDD40_0000, 0x10_0000, false);
    w.add(6, 0, 0, wl2);
    w.writes.lock().unwrap().clear();
    let m2 = merge(&w, &m.nodes, enumerate::enumerate(&w, &ALL), &[], keys_off, 1 << 48);
    assert_eq!(find(&m2.nodes, 6, 0, 0).unwrap().untouched, Some("wlan"));
    assert!(w.writes.lock().unwrap().is_empty(), "the new WLAN or its bridge was written");
}

#[test]
fn another_card_in_the_slot_is_gone_plus_new() {
    let w = minipc();
    let old = build(&w, &ALL, keys_off);
    w.add(I226.0, 0, 0, nic(0xDCA0_0000));
    let m = merge(&w, &old, enumerate::enumerate(&w, &ALL), &[], keys_off, 1 << 48);
    let at = Loc::new(0, I226.0, 0, 0);
    assert_eq!(m.gone.iter().map(|n| (n.loc, n.device)).collect::<Vec<_>>(), [(at, 0x125C)]);
    assert_eq!(m.added.iter().map(|&i| (m.nodes[i].loc, m.nodes[i].device)).collect::<Vec<_>>(), [(at, 0x10D3)]);
    assert!(!same(&m.gone[0], &m.nodes[m.added[0]]));
}

fn info_none(_: Loc) -> FnInfo {
    FnInfo::default()
}

#[test]
fn sysfs_view_of_the_minipc() {
    let w = minipc();
    let nodes = build(&w, &ALL, keys_off);
    let nic_at = Loc::new(0, I226.0, 0, 0);
    let info = |at: Loc| if at == nic_at { FnInfo { driver: Some("igc"), enabled: true, irq: 0x31 } } else { FnInfo::default() };
    let e = sysfs::view(&nodes, &info, &["igc", "xhci", "nvme"]);
    let get = |p: &str| e.iter().find(|x| x.path == p).map(|x| x.kind.clone());
    let dir = "devices/pci0000:00/0000:00:02.1/0000:03:00.0";
    assert_eq!(get(dir), Some(SysKind::Dir));
    assert_eq!(get("devices/pci0000:00"), Some(SysKind::Dir));
    assert_eq!(get(&alloc::format!("{dir}/vendor")), Some(SysKind::Text("0x8086\n".into())));
    assert_eq!(get(&alloc::format!("{dir}/device")), Some(SysKind::Text("0x125c\n".into())));
    assert_eq!(get(&alloc::format!("{dir}/class")), Some(SysKind::Text("0x020000\n".into())));
    assert_eq!(get(&alloc::format!("{dir}/enable")), Some(SysKind::Text("1\n".into())));
    assert_eq!(get(&alloc::format!("{dir}/irq")), Some(SysKind::Text("49\n".into())), "PC-3: the first MSI vector");
    assert_eq!(get("devices/pci0000:00/0000:00:02.1/irq"), Some(SysKind::Text("0\n".into())));
    assert_eq!(get(&alloc::format!("{dir}/modalias")), Some(SysKind::Text("pci:v00008086d0000125Csv00000000sd00000000bc02sc00i00\n".into())));
    assert_eq!(get("bus/pci/devices/0000:03:00.0"), Some(SysKind::Link("../../../devices/pci0000:00/0000:00:02.1/0000:03:00.0".into())));
    assert_eq!(get(&alloc::format!("{dir}/driver")), Some(SysKind::Link("../../../../bus/pci/drivers/igc".into())));
    assert_eq!(get("bus/pci/drivers/igc/0000:03:00.0"), Some(SysKind::Link("../../../../devices/pci0000:00/0000:00:02.1/0000:03:00.0".into())));
    assert_eq!(get("bus/pci/drivers/nvme"), Some(SysKind::Dir));
    assert_eq!(get(&alloc::format!("{dir}/config")), Some(SysKind::Config { loc: nic_at, size: 0x1000 }));
    // resource: 13 lines, BAR0 1 MiB memory, BAR3 16 KiB.
    let Some(SysKind::Text(res)) = get(&alloc::format!("{dir}/resource")) else { panic!() };
    let lines: Vec<&str> = res.lines().collect();
    assert_eq!(lines.len(), 13);
    assert_eq!(lines[0], "0x00000000dca00000 0x00000000dcafffff 0x0000000000040200");
    assert_eq!(lines[1], "0x0000000000000000 0x0000000000000000 0x0000000000000000");
    assert_eq!(lines[3], "0x00000000dcb00000 0x00000000dcb03fff 0x0000000000040200");
    // The GPU's 64-bit prefetchable BAR0 above 4 GiB.
    let gpu = "devices/pci0000:00/0000:00:08.1/0000:c7:00.0";
    let Some(SysKind::Text(res)) = get(&alloc::format!("{gpu}/resource")) else { panic!() };
    assert_eq!(res.lines().next(), Some("0x0000007c00000000 0x0000007c0fffffff 0x000000000014220c"));
    // A bridge: 17 lines, bus numbers.
    let rp = "devices/pci0000:00/0000:00:02.1";
    let Some(SysKind::Text(res)) = get(&alloc::format!("{rp}/resource")) else { panic!() };
    assert_eq!(res.lines().count(), 17);
    assert_eq!(get(&alloc::format!("{rp}/secondary_bus_number")), Some(SysKind::Text("3\n".into())));
    // The WLAN (untouched): listed, no config file, no driver; the NVMe has no driver link.
    let wl = "devices/pci0000:00/0000:00:02.2/0000:04:00.0";
    assert_eq!(get(wl), Some(SysKind::Dir));
    assert_eq!(get(&alloc::format!("{wl}/config")), None);
    assert_eq!(get(&alloc::format!("{wl}/vendor")), Some(SysKind::Text("0x10ec\n".into())));
    assert_eq!(get("devices/pci0000:00/0000:00:01.1/0000:01:00.0/driver"), None);
    let _ = NVME0;
    // Root-bus functions sit directly below the root directory.
    assert_eq!(get("bus/pci/devices/0000:00:00.0"), Some(SysKind::Link("../../../devices/pci0000:00/0000:00:00.0".into())));
    // Sorted, no path twice.
    let paths: Vec<&String> = e.iter().map(|x| &x.path).collect();
    let mut sorted = paths.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(paths, sorted);
    let _ = GPU;
    // No driver bound anywhere: no driver links at all.
    let e2 = sysfs::view(&nodes, &info_none, &[]);
    assert!(e2.iter().all(|x| !x.path.ends_with("/driver")));
}

#[test]
fn tree_text_names_the_driver() {
    let w = minipc();
    let nodes = build(&w, &ALL, keys_off);
    let nic_at = Loc::new(0, I226.0, 0, 0);
    let lines = crate::tree::lines_with(&nodes, "fake", &|at| (at == nic_at).then(|| String::from("driver igc")));
    let l = lines.iter().find(|l| l.contains("03:00.0 [8086:125c]")).unwrap();
    assert!(l.ends_with(", driver igc"), "{l}");
    assert_eq!(lines.iter().filter(|l| l.contains("driver ")).count(), 1);
    assert_eq!(crate::tree::lines(&nodes, "fake"), crate::tree::lines_with(&nodes, "fake", &|_| None));
}

#[test]
fn modalias_format() {
    let w = Fake::new();
    let mut f = FakeFn::new(0x8086, 0x10C9, 0x02_00_00, 0);
    f.w32(0x2C, 0xA03C_8086);
    w.add(0, 3, 0, f);
    let nodes = build(&w, &ALL, keys_off);
    assert_eq!(sysfs::modalias(&nodes[0]), "pci:v00008086d000010C9sv00008086sd0000A03Cbc02sc00i00\n");
}

fn set_vendor(w: &super::fake::Fake, at: Loc, v: u16) {
    w.fns.lock().unwrap().get_mut(&at).unwrap().w16(0, v);
}

fn parents_consistent(nodes: &[crate::Node]) {
    for (i, n) in nodes.iter().enumerate() {
        if let Some(p) = n.parent {
            assert!(p < i, "{}: parent after child", n.loc);
            let b = nodes[p].bridge.unwrap();
            assert!((b.secondary..=b.subordinate).contains(&n.loc.bus), "{}: wrong parent {}", n.loc, nodes[p].loc);
        }
    }
}

/// A function still "not ready" (CRS) at the deadline is held, not gone: itself, the other functions of its
/// device when it is function 0, everything below it when it is a bridge. Held nodes are unchanged and
/// nothing is written. Without the not-ready list the same walk would report them gone.
#[test]
fn rescan_holds_functions_still_not_ready() {
    let w = minipc();
    let old = build(&w, &ALL, keys_off);
    let nvme = Loc::new(0, NVME0.0, 0, 0);
    let gpu_bridge = Loc::new(0, 0, 8, 1);
    let gpu0 = Loc::new(0, GPU.0, 0, 0);
    let behind_bridge: Vec<Loc> = [0u8, 1, 3, 6].iter().map(|&f| Loc::new(0, GPU.0, 0, f)).collect();
    // (what is not ready, what must be held)
    let cases: [(Vec<Loc>, Vec<Loc>); 3] = [
        (alloc::vec![nvme], alloc::vec![nvme]),
        (alloc::vec![gpu_bridge], [alloc::vec![gpu_bridge], behind_bridge.clone()].concat()),
        (alloc::vec![gpu0], behind_bridge.clone()),
    ];
    for (crs, want) in cases {
        let w = minipc();
        for at in &crs {
            set_vendor(&w, *at, enumerate::CRS_VENDOR);
        }
        w.writes.lock().unwrap().clear();
        let walk = enumerate::walk(&w, &ALL);
        assert_eq!(walk.not_ready, crs);
        // The old behaviour: everything the walk missed counts as gone.
        let blind = merge(&w, &old, walk.nodes.clone(), &[], keys_off, 1 << 48);
        assert_eq!(blind.gone.len(), want.len());
        let m = merge(&w, &old, walk.nodes, &walk.not_ready, keys_off, 1 << 48);
        let mut held = m.held.clone();
        held.sort();
        let mut want_sorted = want.clone();
        want_sorted.sort();
        assert_eq!(held, want_sorted, "not ready: {crs:?}");
        assert!(m.gone.is_empty() && m.added.is_empty() && m.sized == 0, "gone {:?}", m.gone.iter().map(|n| n.loc).collect::<Vec<_>>());
        assert_eq!((m.nodes.len(), m.kept), (old.len(), old.len() - want.len()));
        for n in &m.nodes {
            let o = old.iter().find(|o| same(o, n)).unwrap();
            assert_eq!((n.bars, n.untouched, n.command_at_scan, &n.problems), (o.bars, o.untouched, o.command_at_scan, &o.problems));
        }
        parents_consistent(&m.nodes);
        for h in &want {
            let n = m.nodes.iter().find(|n| n.loc == *h).unwrap();
            let o = old.iter().find(|o| o.loc == *h).unwrap();
            let (np, op) = (n.parent.map(|p| m.nodes[p].loc), o.parent.map(|p| old[p].loc));
            assert_eq!(np, op, "{h}: parent changed");
        }
        assert!(w.writes.lock().unwrap().is_empty(), "a rescan wrote to a not-ready function");
        // Ready again on the next rescan: found again, nothing held, still no write.
        for at in &crs {
            let real = old.iter().find(|o| o.loc == *at).unwrap().vendor;
            set_vendor(&w, *at, real);
        }
        w.writes.lock().unwrap().clear();
        let walk = enumerate::walk(&w, &ALL);
        let m2 = merge(&w, &m.nodes, walk.nodes, &walk.not_ready, keys_off, 1 << 48);
        assert!(m2.held.is_empty() && m2.gone.is_empty() && m2.added.is_empty());
        assert_eq!(m2.kept, old.len());
        assert!(w.writes.lock().unwrap().is_empty());
    }
    // A new function that is not ready is neither added nor held.
    let mut f = nic(0xDD20_0000);
    f.w16(0, enumerate::CRS_VENDOR);
    w.add(6, 0, 0, f);
    let walk = enumerate::walk(&w, &ALL);
    let m = merge(&w, &old, walk.nodes, &walk.not_ready, keys_off, 1 << 48);
    assert!(m.added.is_empty() && m.held.is_empty() && m.gone.is_empty());
}

/// Hostile: a function (a bound NIC) and a bridge with four functions behind it read all ones. The rescan
/// neither panics nor writes, reports them gone, and removes children before their bridge.
#[test]
fn rescan_of_all_ones_reports_gone_without_writes() {
    let w = minipc();
    let old = build(&w, &ALL, keys_off);
    let nic_at = Loc::new(0, I226.0, 0, 0);
    let gpu_bridge = Loc::new(0, 0, 8, 1);
    for at in [nic_at, gpu_bridge] {
        w.fns.lock().unwrap().get_mut(&at).unwrap().cfg.iter_mut().for_each(|b| *b = 0xFF);
    }
    w.writes.lock().unwrap().clear();
    let walk = enumerate::walk(&w, &ALL);
    assert!(walk.not_ready.is_empty());
    let m = merge(&w, &old, walk.nodes, &walk.not_ready, keys_off, 1 << 48);
    assert!(w.writes.lock().unwrap().is_empty(), "wrote to an all-ones function: {:?}", w.writes.lock().unwrap());
    assert!(m.added.is_empty() && m.held.is_empty());
    let mut gone: Vec<Loc> = m.gone.iter().map(|n| n.loc).collect();
    gone.sort();
    let mut want = alloc::vec![nic_at, gpu_bridge];
    want.extend([0u8, 1, 3, 6].iter().map(|&f| Loc::new(0, GPU.0, 0, f)));
    want.sort();
    assert_eq!(gone, want);
    parents_consistent(&m.nodes);
    // Removal order: every child before the bridge that carries it.
    let order: Vec<Loc> = crate::rescan::removal_order(&m.gone).map(|n| n.loc).collect();
    let bridge_pos = order.iter().position(|l| *l == gpu_bridge).unwrap();
    for f in [0u8, 1, 3, 6] {
        assert!(order.iter().position(|l| *l == Loc::new(0, GPU.0, 0, f)).unwrap() < bridge_pos, "{order:?}");
    }
}

/// BT-5: `net_drivers=off` keeps network-class functions (and only those) untouched.
#[test]
fn net_drivers_off_keeps_network_class() {
    use crate::registry::net_kept_off;
    assert!(net_kept_off(0x02_00_00, Some("off")));
    assert!(net_kept_off(0x02_80_00, Some(" off\n")));
    assert!(!net_kept_off(0x02_00_00, Some("on")));
    assert!(!net_kept_off(0x02_00_00, None));
    assert!(!net_kept_off(0x01_08_02, Some("off")));
    assert!(!net_kept_off(0x0C_03_30, Some("off")));
}
