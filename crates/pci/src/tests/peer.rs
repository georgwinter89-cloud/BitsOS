//! GP-BUS host tests (plan/spec/gaming-pc.md §2 and §5): the second root bus of the Gaming-PC is found by cutting the
//! MCFG range, nothing is written on the way, the never-touch list is honoured through the untouched state, and with
//! the profile off no read and no decision differs from before.

use super::fake::{root_port, Fake, FakeFn};
use super::gpworld::{self, gaming_pc, NEVER, WINDOW};
use super::world::minipc;
use super::{find, keys_off, ALL};
use crate::access::{ConfigAccess, Loc};
use crate::enumerate::{self, Root};
use crate::node::Node;
use crate::peer;
use crate::registry::{Candidate, Until};
use crate::{bind, build, gp, guardview, BindError, Match, PciFunction};
use alloc::string::String;
use alloc::vec::Vec;
use gp_platform::facts;
use gp_platform::peer_bus::{Range, Refusal};

const CUT: [Root; 2] = [Root { segment: 0, bus_start: 0x00, bus_end: 0x7f }, Root { segment: 0, bus_start: 0x80, bus_end: 0xdf }];

fn plan(c: &dyn ConfigAccess, roots: &[Root], gaming_pc: bool) -> (Vec<Root>, Vec<peer::Planned>) {
    peer::plan_all(c, roots.to_vec(), gaming_pc, &mut |r| enumerate::walk(c, r).nodes)
}

/// The untouched rule as `registry::untouched_key` applies it under the profile with `gp_net=on`.
fn gp_keys(n: &Node) -> Option<&'static str> {
    gp::never_touch_key(true, n.vendor, n.device, n.class).or_else(|| gp::network_key(true, true, n.class))
}

fn places(nodes: &[Node]) -> Vec<(u8, u8, u8)> {
    nodes.iter().map(|n| (n.loc.bus, n.loc.dev, n.loc.func)).collect()
}

#[test]
fn host_tests_have_no_profile() {
    assert!(!gp::active());
}

#[test]
fn without_the_cut_the_walk_stops_at_bus_7f() {
    let w = gaming_pc();
    let nodes = build(&w, &[WINDOW], keys_off);
    assert_eq!(nodes.len(), facts::FUNCTIONS.iter().filter(|x| x.bus < 0x80).count());
    assert!(nodes.iter().all(|n| n.loc.bus < 0x80), "the keyboard's xHCI, the network card and two SSDs are invisible");
}

#[test]
fn the_gaming_pc_is_cut_and_every_function_is_found() {
    let w = gaming_pc();
    let (roots, planned) = plan(&w, &[WINDOW], true);
    assert_eq!(roots, CUT);
    assert_eq!(planned.len(), 1);
    let p = &planned[0];
    assert!(p.cut() && !p.from_facts && p.plan.refused.is_none());
    assert_eq!(p.roots_text(), "00-7f 80-df");
    assert_eq!((p.plan.peers[0].bus, p.plan.peers[0].first_device, p.plan.peers[0].reach), (0x80, 0x14, 0x86));
    let nodes = build(&w, &roots, gp_keys);
    let want: Vec<(u8, u8, u8)> = facts::FUNCTIONS.iter().map(|x| (x.bus, x.dev, x.func)).collect();
    let mut got = places(&nodes);
    got.sort_unstable();
    assert_eq!(got, want, "exactly lspci's list, nothing twice");
    assert!(nodes.iter().all(|n| n.problems.is_empty()), "{:?}", nodes.iter().filter(|n| !n.problems.is_empty()).map(|n| (n.loc, n.problems.clone())).collect::<Vec<_>>());
    // Bus 80's functions hang below no bridge; 81-86 below bus 80's root ports.
    for n in &nodes {
        match n.loc.bus {
            0x00 | 0x80 => assert!(n.parent.is_none(), "{}", n.loc),
            b => assert_eq!(n.parent.map(|p| nodes[p].bridge.unwrap().secondary), Some(b), "{}", n.loc),
        }
    }
}

#[test]
fn the_first_roots_part_of_the_tree_is_the_same_with_and_without_the_cut() {
    let w = gaming_pc();
    let whole = build(&w, &[WINDOW], gp_keys);
    let cut = build(&w, &CUT, gp_keys);
    let low: Vec<&Node> = cut.iter().filter(|n| n.loc.bus < 0x80).collect();
    assert_eq!(whole.len(), low.len());
    for (a, b) in whole.iter().zip(low) {
        assert_eq!(alloc::format!("{a:?}"), alloc::format!("{b:?}"));
    }
}

#[test]
fn planning_and_the_walk_write_nothing_and_sizing_puts_every_register_back() {
    let w = gaming_pc();
    let before = w.snapshot();
    let (roots, _) = plan(&w, &[WINDOW], true);
    assert!(w.writes.lock().unwrap().is_empty(), "planning only reads");
    let _ = enumerate::walk(&w, &roots);
    assert!(w.writes.lock().unwrap().is_empty(), "the walk only reads");
    let nodes = build(&w, &roots, gp_keys);
    // The firmware's assignment stands: after BAR sizing every byte of every function is what it was.
    assert_eq!(w.snapshot(), before);
    // Every write was a BAR sizing step or the command register around it; no bus number, no bridge window.
    for (at, off, _) in w.writes.lock().unwrap().iter() {
        let n = nodes.iter().find(|n| n.loc == *at).unwrap();
        assert!(*off == 4 || (0x10..0x28).contains(off), "{at} offset {off:#x}");
        assert!(!n.is_bridge(), "a bridge register was written: {at} offset {off:#x}");
        assert!(n.untouched.is_none(), "an untouched function was written: {at}");
    }
    // Decoding was off at every BAR write.
    assert!(w.cmd_at_bar_write.lock().unwrap().iter().all(|(_, _, cmd)| cmd & 3 == 0));
}

#[test]
fn bars_behind_the_second_root_are_sized_at_the_firmwares_addresses() {
    let w = gaming_pc();
    let nodes = build(&w, &CUT, gp_keys);
    let bar = |at: (u8, u8, u8), i: usize| find(&nodes, at.0, at.1, at.2).unwrap().bars.unwrap()[i].unwrap();
    assert_eq!((bar(gpworld::XHCI_PCH, 0).start(), bar(gpworld::XHCI_PCH, 0).size()), (0x80_0020_0000, 0x1_0000));
    assert_eq!((bar(gpworld::XHCI_CPU, 0).start(), bar(gpworld::XHCI_CPU, 0).size()), (0xb4_3306_0000, 0x1_0000));
    assert_eq!((bar(gpworld::NIC, 2).start(), bar(gpworld::NIC, 2).size()), (0xbb10_0000, 0x1_0000));
    // Card A's 16 GiB BAR far above 512 GiB fits the CPU's 46 address bits (the host default limit is 2^52).
    let g = bar(gpworld::GPU_A, 0);
    assert_eq!((g.start(), g.size()), (0xa0_0000_0000, 16 << 30));
    assert!(g.start() + g.size() <= 1 << facts::PHYS_ADDR_BITS);
    // An unassigned BAR (the I2C functions) stays visible as unassigned and is handed to nobody.
    let i2c = find(&nodes, 0x80, 0x15, 0).unwrap();
    assert!(i2c.bars.unwrap()[0].is_some_and(|b| !b.assigned()));
    assert!(i2c.function().bar(0).is_none());
}

#[test]
fn the_never_touch_list_is_untouched_unsized_and_unwritten() {
    let w = gaming_pc();
    let nodes = build(&w, &CUT, gp_keys);
    let untouched: Vec<(u8, u8, u8)> = nodes.iter().filter(|n| n.untouched.is_some_and(gp_platform::never_touch::is_key)).map(|n| (n.loc.bus, n.loc.dev, n.loc.func)).collect();
    // The WLAN's root port has nothing but the WLAN below it, so it is untouched with it (the Mini-PC's rule).
    let mut want = NEVER.to_vec();
    want.push(gpworld::WLAN_PORT);
    want.sort_unstable();
    let mut got = untouched.clone();
    got.sort_unstable();
    assert_eq!(got, want);
    for at in NEVER {
        let n = find(&nodes, at.0, at.1, at.2).unwrap();
        assert!(n.bars.is_none(), "{} was sized", n.loc);
        assert_eq!(w.writes_to(at.0, at.1, at.2), 0, "{} was written", n.loc);
        let f = n.function();
        assert!(f.bar(0).is_none() && f.untouched().is_some());
        assert!(!f.write32_ext(4, 0x6), "a raw write to {} went through", n.loc);
        assert_eq!(w.writes_to(at.0, at.1, at.2), 0);
    }
    assert_eq!(find(&nodes, 0x85, 0, 0).unwrap().untouched, Some("gp_wlan"), "the WLAN card is on the list");
    assert_eq!(find(&nodes, 0x80, 0x1f, 5).unwrap().untouched, Some("gp_flash"));
    assert_eq!(find(&nodes, 0x00, 0x0e, 0).unwrap().untouched, Some("gp_vmd"));
    // What the first round drives is touched.
    for at in [gpworld::XHCI_PCH, gpworld::XHCI_CPU, gpworld::GPU_A, gpworld::NIC] {
        assert!(find(&nodes, at.0, at.1, at.2).unwrap().untouched.is_none());
    }
}

fn probed(_: &PciFunction) -> Result<(), String> {
    panic!("a driver probed a function of the never-touch list")
}

#[test]
fn no_driver_binds_a_listed_function_by_class_or_by_id() {
    // hda's own match list (crates/drivers/hda/src/lib.rs:62-69) ends in the class entry 04/03; an xHCI class
    // driver, a network class driver and exact-id drivers for the flash controller and the VMD function join it.
    static HDA: [Match; 1] = [Match::Class { class: 0x04_03_00, mask: 0xFF_FF_00 }];
    static USB: [Match; 1] = [Match::Class { class: 0x0C_03_00, mask: 0xFF_FF_00 }];
    static NET: [Match; 1] = [Match::Class { class: 0x02_00_00, mask: 0xFF_00_00 }];
    static IDS: [Match; 3] = [Match::Id { vendor: 0x8086, device: 0x7f24 }, Match::Id { vendor: 0x8086, device: 0xad0b }, Match::Id { vendor: 0x8086, device: 0x7f68 }];
    let cands: Vec<Candidate> =
        [("hda", &HDA[..]), ("usb", &USB[..]), ("net", &NET[..]), ("ids", &IDS[..])].into_iter().map(|(name, matches)| Candidate { name, matches, probe: probed }).collect();
    let w = gaming_pc();
    let nodes = build(&w, &CUT, gp_keys);
    for at in [gpworld::PCH_AUDIO, gpworld::GPU_AUDIO, gpworld::NHI, gpworld::WLAN, gpworld::FLASH_PCH, gpworld::VMD, gpworld::MEI] {
        let n = find(&nodes, at.0, at.1, at.2).unwrap();
        assert!(matches!(bind::bind_node(n, &cands), Err(BindError::Untouched(k)) if gp_platform::never_touch::is_key(k)), "{}", n.loc);
    }
    // The flash controller of the CPU side has no candidate at all and is untouched all the same.
    let n = find(&nodes, 0x00, 0x1f, 5).unwrap();
    assert!(matches!(bind::bind_node(n, &cands), Err(BindError::NoDriver)));
    assert_eq!(n.untouched, Some("gp_flash"));
}

#[test]
fn without_the_profile_the_list_does_nothing() {
    // The same machine booted without `profile=gaming-pc`: no gp key, nothing held, the WLAN card like any card.
    for x in facts::FUNCTIONS {
        assert_eq!(gp::never_touch_key(false, x.vendor, x.device, x.class), None);
        assert_eq!(gp::network_key(false, false, x.class), None);
    }
    let w = gaming_pc();
    let nodes = build(&w, &CUT, |n| gp::network_key(false, false, n.class));
    assert!(nodes.iter().all(|n| n.untouched.is_none()));
}

#[test]
fn the_mini_pc_gets_the_same_tree_with_or_without_the_profile() {
    let base = alloc::format!("{:?}", build(&minipc(), &ALL, keys_off));
    for mode in [true, false] {
        let w = minipc();
        let (roots, planned) = plan(&w, &ALL, mode);
        assert_eq!(roots, ALL, "one root: the plan is the input");
        assert!(planned[0].plan.peers.is_empty() && planned[0].plan.refused.is_none() && !planned[0].from_facts);
        assert!(w.writes.lock().unwrap().is_empty());
        assert_eq!(alloc::format!("{:?}", build(&w, &roots, keys_off)), base);
        // The never-touch list has none of its ids there.
        assert!(build(&w, &roots, keys_off).iter().all(|n| gp::never_touch_key(true, n.vendor, n.device, n.class).is_none()));
    }
}

/// QEMU's shape in `host/tests/pc/60-peer-root.sh`: q35's bus 0 and a `pxb-pcie` root at bus 0x80 whose only
/// device is a root port at 80:14.0 with an xHCI behind it.
fn qemu_pxb() -> Fake {
    let w = Fake::new();
    let mut hb = FakeFn::new(0x8086, 0x29C0, 0x06_00_00, 0);
    hb.cmd(0x6);
    w.add(0, 0, 0, hb);
    w.add(0, 1, 0, FakeFn::new(0x1234, 0x1111, 0x03_00_00, 0));
    w.add(0, 2, 0, FakeFn::new(0x1B36, 0x000B, 0x06_00_00, 0)); // the expander's function on bus 0
    w.add(0, 3, 0, root_port(0, 0x01, 0x01));
    w.add(0x80, 0x14, 0, root_port(0x80, 0x81, 0x81));
    let mut x = FakeFn::new(0x1B36, 0x000D, 0x0C_03_30, 0);
    x.cmd(0x6).bar64(0, 0x8_0000_0000, 0x4000, false);
    w.add(0x81, 0, 0, x);
    w
}

#[test]
fn the_plan_finds_qemus_second_root() {
    let w = qemu_pxb();
    assert_eq!(build(&w, &ALL, keys_off).len(), 4, "without the plan: bus 0 only");
    let (roots, planned) = plan(&w, &ALL, false);
    assert_eq!(roots, [Root { segment: 0, bus_start: 0, bus_end: 0x7f }, Root { segment: 0, bus_start: 0x80, bus_end: 0xff }]);
    assert!(!planned[0].from_facts);
    let nodes = build(&w, &roots, keys_off);
    let x = find(&nodes, 0x81, 0, 0).unwrap();
    assert_eq!(x.bars.unwrap()[0].map(|b| (b.start(), b.size())), Some((0x8_0000_0000, 0x4000)));
    assert_eq!(x.parent.map(|p| nodes[p].loc), Some(Loc::new(0, 0x80, 0x14, 0)));
}

/// A space where buses that hold nothing answer like bus 0 (aliased reads).
struct Aliased<'a>(&'a Fake);

impl ConfigAccess for Aliased<'_> {
    fn read32(&self, at: Loc, off: u16) -> u32 {
        let empty = !self.0.fns.lock().unwrap().keys().any(|k| k.bus == at.bus);
        self.0.read32(if empty { Loc::new(at.segment, 0, at.dev, at.func) } else { at }, off)
    }
    fn write32(&self, _: Loc, _: u16, _: u32) -> bool {
        panic!("planning wrote")
    }
    fn space_size(&self, segment: u16, bus: u8) -> u16 {
        self.0.space_size(segment, bus)
    }
    fn name(&self) -> &'static str {
        "aliased"
    }
}

#[test]
fn aliased_buses_are_refused_and_the_gaming_pc_takes_the_boards_known_roots() {
    let w = gaming_pc();
    let a = Aliased(&w);
    let (roots, planned) = plan(&a, &[WINDOW], true);
    assert_eq!(planned[0].plan.refused, Some(Refusal::TooManyPeers));
    assert!(planned[0].from_facts);
    assert_eq!(roots, CUT);
    // Without the profile there is no table to fall back on: the range stays whole.
    let (roots, planned) = plan(&a, &[WINDOW], false);
    assert_eq!(roots, [WINDOW]);
    assert!(!planned[0].from_facts && planned[0].plan.refused == Some(Refusal::TooManyPeers));
}

#[test]
fn the_known_roots_are_taken_only_when_the_machine_agrees() {
    let answers = |bus: u8, dev: u8| if (bus, dev) == (0x80, 0x14) { 0x8086 } else { 0xffff };
    let low = [Range::new(0, 0), Range::new(1, 4), Range::new(5, 0x58)];
    assert_eq!(peer::roots_from_facts(WINDOW, &low, &answers), Some(CUT.to_vec()));
    // The ECAM end is the range's, not the table's.
    let wide = Root { segment: 0, bus_start: 0, bus_end: 0xff };
    assert_eq!(peer::roots_from_facts(wide, &low, &answers).map(|r| r[1].bus_end), Some(0xff));
    // Refused: bus 80 silent at device 14, a bridge of bus 0 reaching bus 80, another segment, a range that
    // does not start at bus 0 or ends before the second root does.
    assert_eq!(peer::roots_from_facts(WINDOW, &low, &|_, _| 0xffff), None);
    assert_eq!(peer::roots_from_facts(WINDOW, &low, &|_, _| 0x0000), None);
    assert_eq!(peer::roots_from_facts(WINDOW, &[Range::new(0, 0), Range::new(0x70, 0x80)], &answers), None);
    assert_eq!(peer::roots_from_facts(Root { segment: 1, ..WINDOW }, &low, &answers), None);
    assert_eq!(peer::roots_from_facts(Root { bus_start: 1, ..WINDOW }, &low, &answers), None);
    assert_eq!(peer::roots_from_facts(Root { bus_end: 0xbf, ..WINDOW }, &low, &answers), None);
}

#[test]
fn a_bus_outside_the_ecam_range_is_never_read() {
    // The board's MCFG ends at df (e0-ff belong to VMD); a planner asked about the whole 00-ff must not read there.
    struct Fenced<'a>(&'a Fake);
    impl ConfigAccess for Fenced<'_> {
        fn read32(&self, at: Loc, off: u16) -> u32 {
            assert!(at.bus <= facts::ECAM_LAST_BUS, "read of bus {:02x}", at.bus);
            self.0.read32(at, off)
        }
        fn write32(&self, _: Loc, _: u16, _: u32) -> bool {
            panic!("planning wrote")
        }
        fn space_size(&self, segment: u16, bus: u8) -> u16 {
            self.0.space_size(segment, bus)
        }
        fn name(&self) -> &'static str {
            "fenced"
        }
    }
    let w = gaming_pc();
    let (roots, _) = plan(&Fenced(&w), &[Root { segment: 0, bus_start: 0, bus_end: 0xff }], true);
    assert_eq!(roots, [Root { segment: 0, bus_start: 0, bus_end: 0x7f }, Root { segment: 0, bus_start: 0x80, bus_end: 0xff }]);
}

/// The header read `GuardView` hands to `storage_ranges_with` (GP-GUARD's reader for untouched storage functions).
fn header(w: &super::fake::Fake) -> impl Fn(&Node, u8) -> u32 + '_ {
    move |n, off| w.read32(n.loc, off as u16)
}

#[test]
fn the_guard_sees_the_disks_behind_the_second_root() {
    let w = gaming_pc();
    let nodes = build(&w, &CUT, gp_keys);
    let ranges = guardview::storage_ranges_with(&nodes, Some(&header(&w))).expect("every disk controller's BARs are known");
    for bus in [0x01u64, 0x81, 0x86] {
        let start = 0x8d20_0000 + (bus << 16);
        assert!(ranges.iter().any(|r| r.start == start && r.end == start + 0x4000), "the SSD on bus {bus:02x}");
    }
    // Without the cut the two disks of the second root would be unknown to the guard.
    let uncut = guardview::storage_ranges_with(&build(&w, &[WINDOW], gp_keys), Some(&header(&w))).unwrap();
    assert!(!uncut.iter().any(|r| r.start == 0x8d20_0000 + (0x81 << 16)));
}

#[test]
fn the_untouched_vmd_function_does_not_blind_the_guard() {
    // VMD is class 01 and on the never-touch list, so it is never sized. Without an answer for it the guard would
    // refuse every firmware register (power off and reboot go through it). GP-GUARD's reader for untouched storage
    // functions (crates/pci/src/guardview.rs, written for `nvme_bind=off`) covers it from the BAR registers alone.
    let w = gaming_pc();
    let nodes = build(&w, &CUT, gp_keys);
    assert!(guardview::storage_ranges(&nodes).is_err(), "no reader: unknown, as before");
    let writes = w.writes.lock().unwrap().len();
    let ranges = guardview::storage_ranges_with(&nodes, Some(&header(&w))).unwrap();
    assert_eq!(w.writes.lock().unwrap().len(), writes, "reads only");
    let covers = |start: u64, size: u64| ranges.iter().any(|r| r.start <= start && r.end >= start + size);
    assert!(covers(0x8c00_0000, 16 << 20) && covers(0xb4_3200_0000, 16 << 20), "the two 16 MiB windows, whole");
    // The 32 MiB configuration window is covered up to the guess's 16 MiB cap (`acpi::guard::BAR_GUESS_MAX`); the
    // rest is not built.
    assert!(covers(0xb4_3000_0000, 16 << 20));
    // The I/O APIC and HPET at 0xfec00000/0xfed00000 stay reachable.
    assert!(!ranges.iter().any(|r| r.hits(acpi::sdt::Space::Memory, 0xfec0_0000, 0xfed0_1000)));
    // An unsized function that is not untouched is still unknown.
    let mut asleep = nodes.clone();
    asleep.iter_mut().filter(|n| n.loc.bus == 0x81).for_each(|n| n.bars = None);
    assert!(guardview::storage_ranges_with(&asleep, Some(&header(&w))).is_err());
}

#[test]
fn the_device_line_says_for_good() {
    assert_eq!(alloc::format!("{}", Until("gp_flash")), "for good on this PC (gp_flash)");
    assert_eq!(alloc::format!("{}", Until("gp_wlan")), "for good on this PC (gp_wlan)");
    assert_eq!(alloc::format!("{}", Until("gp_net")), "until gp_net=on");
    let w = gaming_pc();
    let nodes = build(&w, &CUT, gp_keys);
    let text = crate::tree::lines(&nodes, "fake").join("\n");
    assert!(text.contains("80:1f.5 [8086:7f24] 0c8000") && text.contains("untouched for good on this PC (gp_flash)"), "{text}");
}

#[test]
fn the_report_lists_both_strands_in_plain_german() {
    let w = gaming_pc();
    let (roots, planned) = plan(&w, &[WINDOW], true);
    let nodes = build(&w, &roots, gp_keys);
    let driver = |at: Loc| match (at.bus, at.dev, at.func) {
        gpworld::XHCI_PCH | gpworld::XHCI_CPU => Some(String::from("xhci")),
        _ => None,
    };
    let lines = gp::report_lines(&nodes, &roots, &planned, &driver, &|_| None);
    let text = lines.join("\n");
    assert_eq!(lines[0], "Anschluss-Stränge: 2 (Bus 00 bis 7f und 80 bis df), 41 Geräte und Abzweige gefunden");
    assert!(text.contains("Strang 1 (ab Bus 00): 13 Geräte, dazu 6 Abzweige und 1 Hauptknoten ohne eigene Zeile"), "{text}");
    assert!(text.contains("Strang 2 (ab Bus 80): 15 Geräte, dazu 6 Abzweige ohne eigene Zeile"), "{text}");
    assert!(text.contains("  80:14.0 USB-Anschlüsse [8086:7f6e]: läuft mit xhci"), "{text}");
    assert!(text.contains("  83:00.0 Netzwerkkarte (Kabel) [10ec:8126]: erkannt, ohne Treiber"), "{text}");
    assert!(text.contains("  80:1f.5 Baustein mit der Firmware des Mainboards [8086:7f24]: wird nie angefasst"), "{text}");
    assert!(text.contains("  85:00.0 WLAN-Karte [8086:272b]: wird nie angefasst"), "{text}");
    assert!(text.contains("Nie angefasst: 8 Geräte (00:0d.2, 00:0e.0, 00:1f.5, 04:00.1, 80:16.0, 80:1f.3, 80:1f.5, 85:00.0), dazu 1 Abzweig ohne eigene Zeile (80:1c.3)"), "{text}");
    assert!(text.contains("Intel VMD ist im BIOS eingeschaltet (00:0e.0). BitsOS fasst es nicht an. SSDs hinter VMD wären unsichtbar; gefunden wurden 3 SSDs."), "{text}");
    assert!(!text.contains("Achtung"));
    // No code word reaches the page.
    for word in ["untouched", "gp_", "bridge", "root", "probe", "BAR"] {
        assert!(!text.contains(word), "{word} in the report: {text}");
    }
}

/// GP-R2: the round's page (plan/hw/gaming-pc-round1-2026-11-15/GP000001.TXT, its device list as the fixture: no serial numbers in it)
/// said "15 Geräte" over 13 rows and named 80:1c.3 without a row. The same machine now gives the same rows, a count
/// equal to the rows below it, the rest counted beside it so the strands add up to the first line's 41.
#[test]
fn the_round_page_counts_what_it_lists() {
    let round = include_str!("fixtures/gp000001-pci.txt");
    let w = gaming_pc();
    let (roots, planned) = plan(&w, &[WINDOW], true);
    // As on the round's stick: nvme_bind=off holds the SSDs and the SATA functions, the four drivers bound.
    let keys = |n: &Node| gp::never_touch_key(true, n.vendor, n.device, n.class).or(matches!(n.class >> 8, 0x0108 | 0x0106).then_some("nvme_bind"));
    let nodes = build(&w, &roots, keys);
    let driver = |at: Loc| match (at.bus, at.dev, at.func) {
        gpworld::XHCI_PCH | gpworld::XHCI_CPU => Some(String::from("xhci")),
        gpworld::GPU_A => Some(String::from("amdgpu-rdna4")),
        gpworld::NIC => Some(String::from("rtl8126")),
        _ => None,
    };
    let lines = gp::report_lines(&nodes, &roots, &planned, &driver, &|_| None);
    let rows = |text: &[&str]| text.iter().filter(|l| l.starts_with("  ")).map(|l| String::from(*l)).collect::<Vec<String>>();
    let round_lines: Vec<&str> = round.lines().collect();
    let ours: Vec<&str> = lines.iter().map(String::as_str).collect();
    assert_eq!(rows(&ours), rows(&round_lines), "the rows are the round's");
    assert_eq!(lines[0], round_lines[0], "the first line is the round's");
    // Each strand's number is the rows below its line; with what it counts beside them the strands make 41.
    let mut total = 0;
    for (i, l) in lines.iter().enumerate().filter(|(_, l)| l.starts_with("Strang ")) {
        let below = lines[i + 1..].iter().take_while(|r| r.starts_with("  ")).count();
        let nums: Vec<usize> = l.split(|c: char| !c.is_ascii_digit()).filter_map(|x| x.parse().ok()).collect();
        // "Strang <n> (ab Bus <bb>): <rows> Geräte, dazu <bridges> Abzweige [und <hosts> Hauptknoten]"
        assert_eq!(nums[2], below, "{l}");
        total += nums[2..].iter().sum::<usize>();
    }
    assert_eq!(total, 41, "{lines:#?}");
    assert_eq!(rows(&round_lines).len(), 28, "the round listed 13 + 15 rows");
    // "Nie angefasst" names only functions with a row, and the bridge beside them.
    let never = lines.iter().find(|l| l.starts_with("Nie angefasst: ")).expect("the line");
    assert_eq!(never, "Nie angefasst: 8 Geräte (00:0d.2, 00:0e.0, 00:1f.5, 04:00.1, 80:16.0, 80:1f.3, 80:1f.5, 85:00.0), dazu 1 Abzweig ohne eigene Zeile (80:1c.3)");
    let listed = never.split('(').nth(1).and_then(|x| x.split(')').next()).unwrap();
    for at in listed.split(", ") {
        assert!(lines.iter().any(|r| r.starts_with(&alloc::format!("  {at} ")) && r.ends_with("wird nie angefasst")), "{at} has its row");
    }
    // The round's line named nine, the 8 + 1 here are the same nine.
    assert!(round.contains("Nie angefasst: 9 Geräte (00:0d.2, 00:0e.0, 00:1f.5, 04:00.1, 80:16.0, 80:1c.3, 80:1f.3, 80:1f.5, 85:00.0)"));
}

#[test]
fn the_report_says_when_the_search_was_not_clear() {
    let w = gaming_pc();
    let a = Aliased(&w);
    let (roots, planned) = plan(&a, &[WINDOW], true);
    let nodes = build(&w, &roots, gp_keys);
    let lines = gp::report_lines(&nodes, &roots, &planned, &|_| None, &|_| None);
    assert!(lines.contains(&String::from("Achtung: Die Suche nach dem zweiten Strang war nicht eindeutig, die bekannte Aufteilung dieses Mainboards wurde genommen.")), "{lines:?}");
}

#[test]
fn the_picture_line_names_the_card_whose_memory_holds_the_picture() {
    let w = gaming_pc();
    let nodes = build(&w, &CUT, gp_keys);
    let picture = 2560 * 1440 * 4;
    assert_eq!(gp::bar_owner(&nodes, 0xa0_0000_0000, picture).map(|(n, i)| (n.loc, i)), Some((Loc::new(0, 4, 0, 0), 0)));
    assert_eq!(gp::picture_line(&nodes, Some((0xa0_0000_0000, picture, 2560, 1440))), "Bild: 2560 x 1440, liegt im Speicher von Grafik 04:00.0 (Adresse 0xa000000000, oberhalb von 512 GiB)");
    // The last byte must lie inside the BAR too; an address in nobody's BAR says so; no picture says so.
    assert!(gp::bar_owner(&nodes, 0xa0_0000_0000 + (16 << 30) - 4096, 8192).is_none());
    assert_eq!(gp::picture_line(&nodes, Some((0x9_0000_0000, picture, 2560, 1440))), "Bild: 2560 x 1440 an Adresse 0x900000000, gehört zu keinem gefundenen Gerät");
    assert_eq!(gp::picture_line(&nodes, None), "Bild: keins übergeben, BitsOS läuft ohne Bild");
    assert!(gp::bar_owner(&nodes, u64::MAX - 10, 100).is_none(), "a wrapping range has no owner");
    // An untouched function's BARs are unknown and own nothing: the never-touch list cannot be named here.
    assert!(gp::bar_owner(&nodes, 0xfe01_0000, 0x1000).is_none());
}
