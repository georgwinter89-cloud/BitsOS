//! The second PCI root bus (plan/spec/gaming-pc.md §5). The Gaming-PC's one MCFG range 00-df holds two root buses: 00
//! (CPU side) and 80 (chipset side: the keyboard's xHCI, the network card, two of the three SSDs). No bridge on bus 0
//! leads to bus 80, and without AML BitsOS cannot ask ACPI for it, so the walk from the range's first bus never
//! saw it. Here the range is **cut** before the walk: `gp_platform::peer_bus::plan_roots` finds the root buses no
//! bridge leads to (reads only, Linux's peer-bridge fallback) and the walk gets 00-7f and 80-df.
//!
//! On every machine, as Linux scans every host bridge: on one without a second root (the Mini-PC, QEMU) the plan
//! finds none and the walk is the walk it was. Nothing here writes: no bus number, BAR or bridge window is ever
//! assigned on this path.

use crate::access::{read16, ConfigAccess, Loc};
use crate::enumerate::Root;
use crate::node::{Node, Problem};
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::RefCell;
use gp_platform::facts;
use gp_platform::peer_bus::{self, Answers, Plan, Range, Refusal};

/// What became of one MCFG range.
#[derive(Clone, Debug)]
pub struct Planned {
    pub window: Root,
    pub roots: Vec<Root>,
    pub plan: Plan,
    /// The probe was refused and the firmware's own root list (`facts::ROOTS`) was taken instead.
    pub from_facts: bool,
}

impl Planned {
    pub fn cut(&self) -> bool {
        self.roots.len() > 1
    }
    /// "00-7f 80-df"
    pub fn roots_text(&self) -> String {
        let parts: Vec<String> = self.roots.iter().map(|r| alloc::format!("{:02x}-{:02x}", r.bus_start, r.bus_end)).collect();
        parts.join(" ")
    }
}

/// The buses a walk reached: the root bus and the range of every bridge it followed.
pub fn claimed(nodes: &[Node], root: u8) -> Vec<Range> {
    let dropped = |p: &Problem| matches!(p, Problem::BridgeUnconfigured { .. } | Problem::BridgeBadBus { .. } | Problem::BusUnreachable { .. });
    let mut out = alloc::vec![Range::new(root, root)];
    out.extend(nodes.iter().filter(|n| !n.problems.iter().any(dropped)).filter_map(|n| n.bridge).map(|b| Range::new(b.secondary, b.subordinate)));
    out
}

type WalkFn<'a> = dyn FnMut(&[Root]) -> Vec<Node> + 'a;

struct Probe<'a, 'w> {
    c: &'a dyn ConfigAccess,
    segment: u16,
    walk: RefCell<&'a mut WalkFn<'w>>,
}

impl Answers for Probe<'_, '_> {
    fn vendor(&self, bus: u8, dev: u8) -> u16 {
        // A bus outside every ECAM window is never read.
        if self.c.space_size(self.segment, bus) == 0 {
            return 0xffff;
        }
        read16(self.c, Loc::new(self.segment, bus, dev, 0), 0)
    }
    fn reach(&self, root: u8, limit: u8) -> u8 {
        let nodes = (self.walk.borrow_mut())(&[Root { segment: self.segment, bus_start: root, bus_end: limit }]);
        claimed(&nodes, root).iter().map(|r| r.end).max().unwrap_or(root)
    }
}

/// The firmware's own root list for this board (ACPI `PC00`/`PC02`, read from Linux's journal), for the case that
/// probing was refused because too many buses answered. Taken only when it fits what the machine shows: the range
/// starts at bus 0 and holds both roots, no bridge of bus 0 forwards into the second root's buses, and the second
/// root bus answers at the device number the table expects.
pub fn roots_from_facts(window: Root, claimed: &[Range], vendor: &dyn Fn(u8, u8) -> u16) -> Option<Vec<Root>> {
    let (first, second) = (facts::ROOTS[0], facts::ROOTS[1]);
    let fits = window.segment == 0 && window.bus_start == first.first && window.bus_end >= second.last;
    let clear = claimed.iter().all(|c| c.end < second.first);
    let answers = !matches!(vendor(second.first, second.first_device), 0xffff | 0x0000);
    (fits && clear && answers).then(|| {
        alloc::vec![
            Root { segment: 0, bus_start: first.first, bus_end: second.first - 1 },
            Root { segment: 0, bus_start: second.first, bus_end: window.bus_end },
        ]
    })
}

/// Plans one MCFG range. `walk` is the walk the scan itself uses (it waits for functions that are not ready).
/// `gaming_pc`: the profile is on, so a refused probe may fall back on the board's own root list (`roots_from_facts`,
/// taken only where it fits what the machine shows); elsewhere a refused probe keeps the range whole.
pub fn plan_window(c: &dyn ConfigAccess, window: Root, gaming_pc: bool, walk: &mut WalkFn<'_>) -> Planned {
    let first = walk(&[window]);
    let claimed = claimed(&first, window.bus_start);
    let probe = Probe { c, segment: window.segment, walk: RefCell::new(walk) };
    let w = Range::new(window.bus_start, window.bus_end);
    let plan = peer_bus::plan_roots(w, &claimed, &probe);
    let mut roots: Vec<Root> = plan.roots.iter().map(|r| Root { segment: window.segment, bus_start: r.start, bus_end: r.end }).collect();
    let mut from_facts = false;
    if gaming_pc
        && plan.refused == Some(Refusal::TooManyPeers)
        && let Some(known) = roots_from_facts(window, &claimed, &|bus, dev| probe.vendor(bus, dev))
    {
        roots = known;
        from_facts = true;
    }
    Planned { window, roots, plan, from_facts }
}

/// The root ranges to walk. `gaming_pc`: the profile is on (the board's known roots back and check the plan).
pub fn plan_all(c: &dyn ConfigAccess, roots: Vec<Root>, gaming_pc: bool, walk: &mut WalkFn<'_>) -> (Vec<Root>, Vec<Planned>) {
    let planned: Vec<Planned> = roots.iter().map(|&r| plan_window(c, r, gaming_pc, walk)).collect();
    for p in &planned {
        log(p, gaming_pc);
    }
    (planned.iter().flat_map(|p| p.roots.iter().copied()).collect(), planned)
}

fn log(p: &Planned, gaming_pc: bool) {
    let window = alloc::format!("{:02x}-{:02x}", p.window.bus_start, p.window.bus_end);
    for peer in &p.plan.peers {
        klog::event!(Info, "pci", "peer.root", bus = klog::Hex(peer.bus as u64), first_device = klog::Hex(peer.first_device as u64),
            vendor = klog::Hex(peer.vendor as u64), reach = klog::Hex(peer.reach as u64); "a root bus no bridge leads to");
    }
    if let Some(r) = p.plan.refused {
        klog::event!(Warn, "pci", "peer.refused", window = window.as_str(), reason = r.name(), detail = alloc::format!("{r:?}"), from_facts = p.from_facts;
            "the probe for further root buses was not believed");
    }
    klog::event!(Info, "pci", "peer.plan", segment = p.window.segment, window = window.as_str(), roots = p.roots_text().as_str(),
        peers = p.plan.peers.len(), probes = p.plan.probes, from_facts = p.from_facts);
    // On the Gaming-PC the answer is known: say so when the machine shows something else. Only for the board's
    // own MCFG range 00-df: `profile=gaming-pc` can be set by hand on any machine (QEMU's range is 00-ff).
    let boards = p.window.segment == 0 && (p.window.bus_start, p.window.bus_end) == (facts::ECAM_FIRST_BUS, facts::ECAM_LAST_BUS);
    if gaming_pc && boards {
        let known: Vec<(u8, u8)> = facts::ROOTS.iter().map(|r| (r.first, r.last.min(p.window.bus_end))).collect();
        let got: Vec<(u8, u8)> = p.roots.iter().map(|r| (r.bus_start, r.bus_end)).collect();
        if known != got {
            klog::event!(Warn, "pci", "peer.unexpected", roots = p.roots_text().as_str(), expected = "00-7f 80-df";
                "the root buses are not the two this board had under Linux");
        }
    }
}
