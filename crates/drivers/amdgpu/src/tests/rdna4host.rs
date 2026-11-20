//! Wave 42: the PCI side of `nbif` and `ih` on the card (`rdna4::hostprep`, the chain that runs before a stage
//! and is the gate there) on a fake PCI side: its steps come in the stage's order, each only after the one
//! before it passed, and what only asks comes before what changes; no bridge is ever written, held against the
//! PCI core's own enable; Linux's rule for AtomicOps to the root port. And the IH switched off for a Reload as
//! Linux switches it off (`rdna4::ihoff`).

use super::rdna4fix::*;
use super::rdna4nb::up;
use super::rdna4nbmodel::{after, n};
use crate::gpucore::psp::Hw;
use crate::rdna4::fakemodel::Model;
use crate::rdna4::hostprep::{self as hp, BridgeNow, CardNow, For, PciSide, PortNow, Prep};
use crate::rdna4::ihoff::{self, Off, OffIo, OffRegs};
use crate::rdna4::ihstage::HOST_BYTES;
use crate::rdna4::stage::{Session, Stage};
use rdna4_core::ih7::{self, f};
use std::string::String;
use std::vec::Vec;

const ALL: u16 = hp::CMD_IO | hp::CMD_MEM | hp::CMD_MASTER;

/// The Gaming-PC's path as `lspci` showed it: root port, the card's own switch (upstream, downstream), the card
/// with its command register at 0x0006 and an I/O BAR.
struct Side {
    survey: Result<(), String>,
    card: Option<CardNow>,
    bridges: Vec<BridgeNow>,
    pages: Option<u64>,
    arm: Result<(), String>,
    enable: Result<String, String>,
    msi: Result<String, String>,
    calls: Vec<&'static str>,
}

fn port(port_type: u8, devcap2: u32) -> Option<PortNow> {
    Some(PortNow { port_type, devcap2, devctl2: 0 })
}

fn bridge(loc: &str, pcie: Option<PortNow>) -> BridgeNow {
    BridgeNow { loc: String::from(loc), command: ALL, d_state: 0, mem_window: true, io_window: true, pcie }
}

impl Side {
    fn measured() -> Side {
        Side {
            survey: Ok(()),
            card: Some(CardNow { command: 0x0006, d_state: 0, bars_recorded: true, decode_needed: hp::CMD_MEM | hp::CMD_IO, pcie: port(hp::TYPE_LEG_END, 0) }),
            // Nearest first.
            bridges: std::vec![
                bridge("0000:03:00.0", port(hp::TYPE_DOWNSTREAM, hp::DEVCAP2_ATOMIC_ROUTE)),
                bridge("0000:02:00.0", port(hp::TYPE_UPSTREAM, hp::DEVCAP2_ATOMIC_ROUTE)),
                bridge("0000:00:01.1", port(hp::TYPE_ROOT_PORT, hp::DEVCAP2_ATOMIC_ROUTE | hp::DEVCAP2_ATOMIC_COMP32 | hp::DEVCAP2_ATOMIC_COMP64)),
            ],
            pages: Some(0x1_2340_0000),
            arm: Ok(()),
            enable: Ok(String::from("the card's command register 0x0006 -> 0x0407")),
            msi: Ok(String::from("MSI vector 0x41 on CPU 0")),
            calls: Vec::new(),
        }
    }
}

impl PciSide for Side {
    fn survey(&mut self) -> Result<(), String> {
        self.calls.push("survey");
        self.survey.clone()
    }
    fn card(&mut self) -> Option<CardNow> {
        self.calls.push("card");
        self.card
    }
    fn bridges(&mut self) -> Vec<BridgeNow> {
        self.calls.push("bridges");
        self.bridges.clone()
    }
    fn pages(&mut self, bytes: u64) -> Option<u64> {
        assert_eq!(bytes, HOST_BYTES);
        self.calls.push("pages");
        self.pages
    }
    fn arm_reload(&mut self) -> Result<(), String> {
        self.calls.push("arm_reload");
        self.arm.clone()
    }
    fn set_atomic_req(&mut self) -> Result<String, String> {
        self.calls.push("set_atomic_req");
        Ok(String::from("requester enable set (0x0000 -> 0x0040)"))
    }
    fn enable(&mut self) -> Result<String, String> {
        self.calls.push("enable");
        self.enable.clone()
    }
    fn msi(&mut self) -> Result<String, String> {
        self.calls.push("msi");
        self.msi.clone()
    }
}

const IH: For = For::Ih { bytes: HOST_BYTES };
/// The three calls that change something of the machine.
const CHANGES: [&str; 3] = ["set_atomic_req", "enable", "msi"];

fn run(change: impl FnOnce(&mut Side), stage: For, driven: bool, would_run: bool) -> (Prep, Vec<&'static str>) {
    let mut side = Side::measured();
    change(&mut side);
    let p = hp::prepare(&mut side, stage, driven, would_run);
    (p, side.calls)
}

#[test]
fn the_chain_for_ih_runs_in_the_stages_order_and_asks_before_it_changes() {
    let (p, calls) = run(|_| {}, IH, true, true);
    assert_eq!(calls, ["survey", "card", "bridges", "pages", "arm_reload", "enable", "msi"]);
    assert_eq!((p.path.clone(), p.bus_master, p.pages), (Some(Ok(())), Some(true), Some((HOST_BYTES, 0x1_2340_0000))));
    assert!(matches!((&p.enabled, &p.msi, &p.atomics), (Some(Ok(_)), Some(Ok(_)), None)));
    assert_eq!(p.changes, ["the card's command register 0x0006 -> 0x0407", "MSI enabled: MSI vector 0x41 on CPU 0"]);
    // The second card, a stage whose own order check says no, a stage with no PCI side: not one question.
    for (stage, driven, would_run) in [(IH, false, true), (IH, true, false), (For::Nbif, false, true), (For::Nbif, true, false), (For::Other, true, true)] {
        let (p, calls) = run(|_| {}, stage, driven, would_run);
        assert!(calls.is_empty() && p == Prep::default(), "{stage:?} {driven} {would_run}: {calls:?}");
    }
}

#[test]
fn a_no_ends_the_chain_before_anything_changes() {
    type Case = (&'static str, fn(&mut Side), &'static [&'static str]);
    let cases: [Case; 12] = [
        ("the survey says an IOMMU translates", |s| s.survey = Err(String::from("TranslationOn")), &["survey"]),
        ("the card's configuration space does not read", |s| s.card = None, &["survey", "card"]),
        ("a bridge with its I/O window and I/O decode off", |s| s.bridges[1].command = hp::CMD_MEM | hp::CMD_MASTER, &["survey", "card", "bridges"]),
        ("a bridge with memory decode off", |s| s.bridges[0].command = hp::CMD_IO | hp::CMD_MASTER, &["survey", "card", "bridges"]),
        ("the root port with bus master off", |s| s.bridges[2].command = hp::CMD_IO | hp::CMD_MEM, &["survey", "card", "bridges"]),
        ("a bridge in D3hot", |s| s.bridges[0].d_state = 3, &["survey", "card", "bridges"]),
        ("a bridge that does not answer", |s| s.bridges[1].command = 0xFFFF, &["survey", "card", "bridges"]),
        ("the card not in D0", |s| s.card.as_mut().unwrap().d_state = 3, &["survey", "card", "bridges"]),
        ("the card's BARs not on record", |s| s.card.as_mut().unwrap().bars_recorded = false, &["survey", "card", "bridges"]),
        ("no memory for the rings", |s| s.pages = None, &["survey", "card", "bridges", "pages"]),
        ("no Reload hook", |s| s.arm = Err(String::from("BAR5 does not map")), &["survey", "card", "bridges", "pages", "arm_reload"]),
        ("the PCI core refuses the enable", |s| s.enable = Err(String::from("Busy")), &["survey", "card", "bridges", "pages", "arm_reload", "enable"]),
    ];
    for (what, change, want) in cases {
        let (p, calls) = run(change, IH, true, true);
        assert_eq!(calls, want, "{what}");
        assert!(p.changes.is_empty() && p.msi.is_none() && !matches!(p.enabled, Some(Ok(_))), "{what}: {p:?}");
        // The enable is asked for only after every question was answered with yes.
        assert!(!calls.contains(&"enable") || calls[..calls.len() - 1] == ["survey", "card", "bridges", "pages", "arm_reload"], "{what}");
    }
    // The stage hears why (it asks in the same order and stops at the same place).
    let (p, _) = run(|s| s.bridges[1].command = hp::CMD_MEM | hp::CMD_MASTER, IH, true, true);
    assert_eq!(p.path, Some(Err(String::from("the bridge 0000:02:00.0 has I/O decode off (command register 0x0006), and BitsOS writes no bridge of this card"))));
    assert_eq!((p.pages, &p.enabled), (None, &None));
    let (p, _) = run(|s| s.bridges[2].command = 0, IH, true, true);
    assert!(p.path.unwrap().unwrap_err().contains("has bus master and memory decode and I/O decode off"));
    let (p, _) = run(|s| s.arm = Err(String::from("BAR5 does not map")), IH, true, true);
    assert_eq!(p.enabled, Some(Err(String::from("a Reload could not switch the card's rings off (BAR5 does not map)"))));
    // A vector the PCI core does not hand out: the enable happened and is named, the stage stops at the vector.
    let (p, calls) = run(|s| s.msi = Err(String::from("no vector")), IH, true, true);
    assert_eq!((calls.last(), p.changes.len(), &p.msi), (Some(&"msi"), 1, &Some(Err(String::from("no vector")))));
    // A window the bridge does not have needs no decode: nothing would be written there.
    let (p, calls) = run(|s| (s.bridges[1].command, s.bridges[1].io_window) = (hp::CMD_MEM | hp::CMD_MASTER, false), IH, true, true);
    assert!(p.path == Some(Ok(())) && calls.contains(&"enable"));
}

#[test]
fn atomic_ops_to_the_root_port_as_linux_asks_for_them() {
    let (p, calls) = run(|_| {}, For::Nbif, true, true);
    assert_eq!(calls, ["card", "bridges", "set_atomic_req"]);
    assert_eq!(p.changes, ["requester enable set (0x0000 -> 0x0040)"]);
    assert!(matches!(p.atomics, Some(Ok(_))) && p.enabled.is_none() && p.msi.is_none() && p.pages.is_none());
    // Set already (the stage runs again after a stop): asked, granted, nothing written.
    let (p, calls) = run(|s| s.card.as_mut().unwrap().pcie.as_mut().unwrap().devctl2 = hp::DEVCTL2_ATOMIC_REQ, For::Nbif, true, true);
    assert_eq!(calls, ["card", "bridges"]);
    assert!(p.changes.is_empty() && p.atomics.unwrap().unwrap().contains("was set already (the card's own Device Control 2 0x0040)"));
    // drivers/pci/pci.c:3747-3820, each way to a no: nothing is written.
    type Case = (fn(&mut Side), &'static str);
    let cases: [Case; 8] = [
        (|s| s.card = None, "the card's configuration space does not read"),
        (|s| s.card.as_mut().unwrap().pcie = None, "the card has no PCI Express capability"),
        (|s| s.card.as_mut().unwrap().pcie = port(hp::TYPE_UPSTREAM, 0), "the card is no endpoint (port type 5)"),
        (|s| s.bridges[2].pcie = port(hp::TYPE_ROOT_PORT, hp::DEVCAP2_ATOMIC_COMP32), "the root port 0000:00:01.1 does not complete 32- and 64-bit AtomicOps (Device Capabilities 2 0x80)"),
        (|s| s.bridges[2].pcie = None, "no root port above the card"),
        (|s| s.bridges[0].pcie = port(hp::TYPE_DOWNSTREAM, 0), "the switch port 0000:03:00.0 does not route AtomicOps"),
        (|s| s.bridges[1].pcie = port(hp::TYPE_UPSTREAM, 0), "the switch port 0000:02:00.0 does not route AtomicOps"),
        (|s| s.bridges[1].pcie.as_mut().unwrap().devctl2 = hp::DEVCTL2_ATOMIC_EGRESS_BLOCK, "the upstream port 0000:02:00.0 blocks AtomicOps at egress"),
    ];
    for (change, why) in cases {
        let (p, calls) = run(change, For::Nbif, true, true);
        assert_eq!(p.atomics, Some(Err(String::from(why))));
        assert!(p.changes.is_empty() && !calls.iter().any(|c| CHANGES.contains(c)), "{why}: {calls:?}");
    }
    // Linux asks nothing of the root port's routing bit, and an endpoint of either kind may request.
    let (p, _) = run(|s| (s.bridges[2].pcie, s.card.as_mut().unwrap().pcie) = (port(hp::TYPE_ROOT_PORT, hp::DEVCAP2_ATOMIC_COMP32 | hp::DEVCAP2_ATOMIC_COMP64), port(hp::TYPE_ENDPOINT, 0)), For::Nbif, true, true);
    assert!(matches!(p.atomics, Some(Ok(_))));
}

mod core_enable {
    //! The bridge rule against the PCI core itself: whenever `hostprep::bridges_forward` says yes, the PCI
    //! core's enable (`pci::enable::enable`, what `PciFunction::enable_device` runs) writes no bridge; whenever
    //! it says no for a command bit, the enable would have written that bridge.
    use super::{ALL, BridgeNow, hp};
    use pci::{Bar, BridgeInfo, ConfigAccess, Loc, Node};
    use std::string::String;
    use std::sync::Mutex;
    use std::vec::Vec;

    #[derive(Default)]
    struct Cfg {
        /// (function, offset) -> dword; anything else of a present function reads 0.
        regs: Mutex<Vec<((Loc, u16), u32)>>,
        writes: Mutex<Vec<(Loc, u16, u32)>>,
    }

    impl ConfigAccess for Cfg {
        fn read32(&self, at: Loc, off: u16) -> u32 {
            self.regs.lock().unwrap().iter().find(|(k, _)| *k == (at, off & !3)).map_or(0, |(_, v)| *v)
        }
        fn write32(&self, at: Loc, off: u16, v: u32) -> bool {
            self.writes.lock().unwrap().push((at, off, v));
            let mut regs = self.regs.lock().unwrap();
            match regs.iter_mut().find(|(k, _)| *k == (at, off & !3)) {
                Some((_, old)) => *old = v,
                None => regs.push(((at, off & !3), v)),
            }
            true
        }
        fn space_size(&self, _segment: u16, _bus: u8) -> u16 {
            4096
        }
        fn name(&self) -> &'static str {
            "fake"
        }
    }

    fn node(loc: Loc, ids: (u16, u16), class: u32, parent: Option<usize>, bridge: Option<BridgeInfo>, bars: Option<pci::Bars>) -> Node {
        Node {
            loc,
            vendor: ids.0,
            device: ids.1,
            class,
            revision: 0,
            header_type: u8::from(bridge.is_some()),
            subsystem: (0, 0),
            command_at_scan: 0,
            status_at_scan: 0,
            interrupt_pin: 0,
            parent,
            space: 4096,
            bridge,
            caps: Vec::new(),
            ext_caps: Vec::new(),
            pcie: None,
            bars,
            untouched: None,
            problems: Vec::new(),
        }
    }

    #[test]
    fn the_enable_writes_a_bridge_exactly_when_the_rule_says_no() {
        let window = |start| Some(pci::node::Window { start, end: start + 0xFFF });
        let locs = [Loc::new(0, 0, 1, 1), Loc::new(0, 2, 0, 0), Loc::new(0, 3, 0, 0)];
        let card = Loc::new(0, 4, 0, 0);
        let mut bars: pci::Bars = [None; 6];
        bars[0] = Some(Bar::Mem { phys: 0xA0_0000_0000, size: 16 << 30, prefetch: true, is64: true });
        bars[4] = Some(Bar::Io { port: 0x9000, size: 256 });
        bars[5] = Some(Bar::Mem { phys: 0x8D00_0000, size: 512 << 10, prefetch: false, is64: false });
        let need = pci::enable::decode_needed(Some(&bars));
        assert_eq!(need, hp::CMD_MEM | hp::CMD_IO);
        let (mut yes, mut no) = (0, 0);
        // The switch's upstream port in every state of its three command bits, with and without an I/O window.
        for (command, io_window) in (0..8u16).flat_map(|c| [(c, true), (c, false)]) {
            let info = |secondary: u8, io| Some(BridgeInfo { primary: secondary - 1, secondary, subordinate: 4, io, mem: window(0x8D00_0000), prefetch: window(0xA0_0000_0000), control: 0 });
            let nodes = [
                node(locs[0], (0x1022, 0x14DB), 0x06_04_00, None, info(2, window(0x9000)), None),
                node(locs[1], (0x1002, 0x1478), 0x06_04_00, Some(0), info(3, if io_window { window(0x9000) } else { None }), None),
                node(locs[2], (0x1002, 0x1479), 0x06_04_00, Some(1), info(4, window(0x9000)), None),
                node(card, (0x1002, 0x7550), 0x03_00_00, Some(2), None, Some(bars)),
            ];
            let cfg = Cfg::default();
            let commands = [ALL, command, ALL];
            for (n, c) in nodes.iter().zip(commands.iter().chain([&0x0006])) {
                let mut regs = cfg.regs.lock().unwrap();
                regs.push(((n.loc, 0), u32::from(n.device) << 16 | u32::from(n.vendor)));
                regs.push(((n.loc, 4), u32::from(*c)));
            }
            // What `livepci` builds from the same record and the same registers, nearest first.
            let now: Vec<BridgeNow> = (0..3)
                .rev()
                .map(|i| {
                    let b = nodes[i].bridge.unwrap();
                    BridgeNow { loc: String::new(), command: commands[i], d_state: 0, mem_window: b.mem.is_some() || b.prefetch.is_some(), io_window: b.io.is_some(), pcie: None }
                })
                .collect();
            let rule = hp::bridges_forward(need, &now);
            let o = pci::enable::enable(&cfg, &nodes, 3, true, false, true, u64::MAX, &mut |_| {});
            assert!(o.error.is_none(), "{:?}", o.error);
            let writes = cfg.writes.lock().unwrap();
            let bridge_written = writes.iter().any(|(at, ..)| *at != card);
            assert_eq!(rule.is_ok(), !bridge_written, "command {command:#x}, I/O window {io_window}: {rule:?}, the enable wrote {writes:?}");
            assert_eq!(bridge_written, o.changes.iter().any(|c| matches!(c, pci::enable::Change::Bridge { .. })));
            // The card's own command register is the one thing the enable changes otherwise: 0x0006 -> 0x0407.
            assert!(writes.iter().any(|w| *w == (card, 4, 0x0407)), "{writes:?}");
            if rule.is_ok() { yes += 1 } else { no += 1 }
        }
        // All three bits on, or all but I/O decode where the bridge has no I/O window.
        assert_eq!((yes, no), (3, 13));
    }
}

/// The model's registers as the Reload hook reaches them.
struct ModelOff<'a>(&'a mut Model);

impl OffIo for ModelOff<'_> {
    fn read(&mut self, byte: u32) -> Option<u32> {
        self.0.reg_read(byte)
    }
    fn write(&mut self, byte: u32, v: u32) -> bool {
        self.0.reg_write(byte, v)
    }
    fn delay_us(&mut self, us: u64) {
        self.0.delay_us(us)
    }
}

#[test]
fn a_reload_switches_the_ih_off_as_linux_does_and_writes_nothing_while_no_ring_is_on() {
    let rig = Rig::new();
    let n = n(&rig);
    let r = OffRegs::of(&n).unwrap();
    let b = |x| n.byte(x).unwrap();
    // Before the `ih` stage (and after a `gmc` alone): no ring is on, not one write.
    for stages in [&[][..], &[Stage::Nbif, Stage::Gmc][..]] {
        let (_, mut m) = after(&rig, stages);
        let (writes, t) = (m.writes.len(), m.card.now_us);
        assert_eq!(ihoff::irq_disable(&mut ModelOff(&mut m), &r), Off::NotOn);
        assert_eq!((m.writes.len(), m.card.now_us), (writes, t));
    }
    // After it: ih_v7_0_irq_disable's order (ih_v7_0.c:433-440), then its millisecond.
    let mut s = Session::default();
    let (_, mut m) = up(&rig, &mut s, model());
    let (cntl2, ring1) = (m.reg_read(b(ih7::CNTL2)).unwrap(), m.reg_read(b(ih7::RB_CNTL_RING1)).unwrap());
    assert!(f::SELF_IV_FORCE_WPTR_UPDATE_ENABLE.get(cntl2) == 1 && m.nb.ih_on == [true, true]);
    m.raise(ih7::Ring::Ring0, &ih7::Iv { client_id: 4, src_id: 9, ..ih7::Iv::default() }).unwrap();
    let (writes, t) = (m.writes.len(), m.card.now_us);
    assert_eq!(ihoff::irq_disable(&mut ModelOff(&mut m), &r), Off::Done);
    let (off2, off1) = ih7::self_int_off(cntl2, ring1);
    let ring0 = m.reg_read(b(ih7::RB_CNTL)).unwrap();
    assert_eq!(
        m.writes[writes..],
        [
            (b(ih7::RB_CNTL_RING1), off1),
            (b(ih7::CNTL2), off2),
            (b(ih7::RB_CNTL), ring0),
            (b(ih7::RB_RPTR), 0),
            (b(ih7::RB_WPTR), 0),
            (b(ih7::RB_CNTL_RING1), ih7::toggled(ih7::Ring::Ring1, off1, false)),
            (b(ih7::RB_RPTR_RING1), 0),
            (b(ih7::RB_WPTR_RING1), 0),
        ]
    );
    assert_eq!(m.card.now_us - t, ihoff::SETTLE_US);
    assert!(f::SELF_IV_FORCE_WPTR_UPDATE_ENABLE.get(off2) == 0 && f::RB_ENABLE.get(ring0) == 0 && f::ENABLE_INTR.get(ring0) == 0);
    assert_eq!((m.nb.ih_on, m.nb.wptr, m.nb.rptr), ([false, false], [0, 0], [0, 0]));
    assert!(m.refused.is_empty(), "{:?}", m.refused);
    // Off now: the card raises nothing more, and a second Reload writes nothing.
    assert!(m.raise(ih7::Ring::Ring0, &ih7::Iv::default()).is_err());
    let writes = m.writes.len();
    assert_eq!(ihoff::irq_disable(&mut ModelOff(&mut m), &r), Off::NotOn);
    assert_eq!(m.writes.len(), writes);
    // A card that is gone (all ones) or does not read: nothing is written.
    struct Gone(u32);
    impl OffIo for Gone {
        fn read(&mut self, _byte: u32) -> Option<u32> {
            Some(u32::MAX)
        }
        fn write(&mut self, _byte: u32, _v: u32) -> bool {
            self.0 += 1;
            true
        }
        fn delay_us(&mut self, _us: u64) {}
    }
    let mut gone = Gone(0);
    assert_eq!((ihoff::irq_disable(&mut gone, &r), gone.0), (Off::Gone, 0));
}
