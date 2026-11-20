//! Root buses the firmware's bridges do not lead to (plan/spec/gaming-pc.md §5). BitsOS has no AML interpreter, so it
//! cannot ask ACPI for the root buses the way Linux does on this board (`PC00` 00-7f, `PC02` 80-df); it walks each
//! MCFG range from its first bus. The Gaming-PC's one range 00-df holds a second root at bus 80 that no bridge
//! forwards to.
//!
//! The model is Linux's fallback for machines without ACPI roots (linux-7.3.1 arch/x86/pci/legacy.c:15-25
//! `pcibios_fixup_peer_bridges`, :37-55 `pcibios_scan_specific_bus`): for every bus number not known yet, read the
//! vendor id of function 0 of each of the 32 device numbers; the first answer that is not 0x0000 or 0xffff makes the
//! bus a root, and what its bridges forward to is known from then on.
//!
//! The answer is a **cut** of the range (00-7f and 80-df), because the walk keeps a bridge inside its root's range
//! and drops a root that starts inside a range already walked. Reads only. Where the tree is not one this rule
//! understands, the plan is refused and the input comes back unchanged.

use alloc::vec::Vec;

/// Bus numbers `start..=end` of one segment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Range {
    pub start: u8,
    pub end: u8,
}

impl Range {
    pub const fn new(start: u8, end: u8) -> Range {
        Range { start, end }
    }
    pub fn contains(&self, bus: u8) -> bool {
        (self.start..=self.end).contains(&bus)
    }
}

/// What the planner may ask the machine (configuration reads only).
pub trait Answers {
    /// The vendor id function 0 of device `dev` on `bus` answers with (0xffff or 0x0000: nothing there).
    fn vendor(&self, bus: u8, dev: u8) -> u16;
    /// The highest bus the bridges below root bus `root` forward to, looking no further than `limit`; `root` itself
    /// when it has no bridge. This is the walk's own answer for a root range `root..=limit`.
    fn reach(&self, root: u8, limit: u8) -> u8;
}

/// More peer roots than any board this was written for has (this one has one): then nothing is believed.
pub const MAX_PEERS: usize = 4;

/// A root bus found by probing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Peer {
    pub bus: u8,
    /// The first device number that answered (0x14 on this machine's bus 80: there is no device 0).
    pub first_device: u8,
    pub vendor: u16,
    /// Highest bus below it.
    pub reach: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The range ends before it starts.
    Backwards,
    /// More than `MAX_PEERS` buses answered as roots (a device answering on every bus, or aliased reads).
    TooManyPeers,
    /// A bridge of the first root forwards to buses above a peer root: a cut would take them away from it.
    ClaimAbovePeer { peer: u8, claim_start: u8 },
}

impl Refusal {
    pub fn name(&self) -> &'static str {
        match self {
            Refusal::Backwards => "backwards",
            Refusal::TooManyPeers => "too_many_peers",
            Refusal::ClaimAbovePeer { .. } => "claim_above_peer",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    /// The ranges to walk, in bus order. The input alone when nothing was found or the plan was refused.
    pub roots: Vec<Range>,
    pub peers: Vec<Peer>,
    pub refused: Option<Refusal>,
    /// Vendor-id reads made.
    pub probes: u32,
}

impl Plan {
    fn unchanged(window: Range, refused: Option<Refusal>, probes: u32) -> Plan {
        Plan { roots: alloc::vec![window], peers: Vec::new(), refused, probes }
    }
    /// True when the plan is the input: the walk that follows is the walk without this module.
    pub fn is_input(&self, window: Range) -> bool {
        self.roots.len() == 1 && self.roots[0] == window
    }
}

fn present(vendor: u16) -> bool {
    vendor != 0xffff && vendor != 0x0000
}

/// Cuts `window` at every root bus that answers but that the walk from `window.start` did not reach. `claimed`: the
/// buses that walk did reach (the root bus itself and every followed bridge's secondary..=subordinate).
pub fn plan_roots(window: Range, claimed: &[Range], answers: &dyn Answers) -> Plan {
    if window.end < window.start {
        return Plan::unchanged(window, Some(Refusal::Backwards), 0);
    }
    let mut peers: Vec<Peer> = Vec::new();
    let mut probes = 0u32;
    // u16: `window.end` may be 0xff.
    let mut bus = window.start as u16 + 1;
    while bus <= window.end as u16 {
        let b = bus as u8;
        if let Some(c) = claimed.iter().filter(|c| c.contains(b)).map(|c| c.end).max() {
            bus = c as u16 + 1;
            continue;
        }
        let mut found = None;
        for dev in 0..32u8 {
            probes += 1;
            let vendor = answers.vendor(b, dev);
            if present(vendor) {
                found = Some((dev, vendor));
                break;
            }
        }
        let Some((first_device, vendor)) = found else {
            bus += 1;
            continue;
        };
        if peers.len() == MAX_PEERS {
            return Plan::unchanged(window, Some(Refusal::TooManyPeers), probes);
        }
        // A hostile answer cannot move the plan backwards or past the range.
        let reach = answers.reach(b, window.end).clamp(b, window.end);
        peers.push(Peer { bus: b, first_device, vendor, reach });
        bus = reach as u16 + 1;
    }
    let Some(first) = peers.first() else {
        return Plan::unchanged(window, None, probes);
    };
    if let Some(c) = claimed.iter().find(|c| c.end > first.bus) {
        return Plan::unchanged(window, Some(Refusal::ClaimAbovePeer { peer: first.bus, claim_start: c.start }), probes);
    }
    let mut roots = Vec::with_capacity(peers.len() + 1);
    let mut start = window.start;
    for p in &peers {
        roots.push(Range::new(start, p.bus - 1));
        start = p.bus;
    }
    roots.push(Range::new(start, window.end));
    Plan { roots, peers, refused: None, probes }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facts::{self, Function};
    extern crate std;
    use core::cell::Cell;
    use std::vec::Vec;

    /// A machine built from a function table; counts what is asked and refuses to be asked outside `ecam`.
    struct Tree<'a> {
        functions: Vec<&'a Function>,
        ecam: Range,
        asked: Cell<u32>,
    }

    impl<'a> Tree<'a> {
        fn new(functions: impl Iterator<Item = &'a Function>, ecam: Range) -> Tree<'a> {
            Tree { functions: functions.collect(), ecam, asked: Cell::new(0) }
        }
        /// What a walk from `root` claims: the root bus and every bridge range reached from it.
        fn claimed(&self, root: u8, limit: u8) -> Vec<Range> {
            let mut out = std::vec![Range::new(root, root)];
            let mut todo = std::vec![root];
            while let Some(bus) = todo.pop() {
                for x in self.functions.iter().filter(|x| x.bus == bus && x.below.0 > bus && x.below.1 <= limit) {
                    out.push(Range::new(x.below.0, x.below.1));
                    todo.push(x.below.0);
                }
            }
            out
        }
    }

    impl Answers for Tree<'_> {
        fn vendor(&self, bus: u8, dev: u8) -> u16 {
            assert!(self.ecam.contains(bus), "read outside the ECAM range: bus {bus:02x}");
            self.asked.set(self.asked.get() + 1);
            self.functions.iter().find(|x| x.bus == bus && x.dev == dev && x.func == 0).map_or(0xffff, |x| x.vendor)
        }
        fn reach(&self, root: u8, limit: u8) -> u8 {
            self.claimed(root, limit).iter().map(|r| r.end).max().unwrap_or(root)
        }
    }

    const WINDOW: Range = Range::new(facts::ECAM_FIRST_BUS, facts::ECAM_LAST_BUS);

    fn gaming_pc() -> Tree<'static> {
        Tree::new(facts::FUNCTIONS.iter(), WINDOW)
    }

    #[test]
    fn this_machine_is_cut_into_00_7f_and_80_df() {
        let t = gaming_pc();
        let p = plan_roots(WINDOW, &t.claimed(0, WINDOW.end), &t);
        assert_eq!(p.roots, [Range::new(0x00, 0x7f), Range::new(0x80, 0xdf)]);
        assert_eq!(p.refused, None);
        let roots: Vec<(u8, u8)> = facts::ROOTS.iter().map(|r| (r.first, r.last)).collect();
        assert_eq!(p.roots.iter().map(|r| (r.start, r.end)).collect::<Vec<_>>(), roots, "the firmware's own root list");
    }

    #[test]
    fn the_same_cut_with_the_second_graphics_card_in() {
        // 5.11.: card B on buses 05-07 and the Thunderbolt ports on 08 and 09 (they sit on 05-58 in today's table).
        let moved = [(0x07u8, 0u8, (0x08u8, 0x08u8)), (0x07, 1, (0x09, 0x09))];
        let mut owned: Vec<Function> = facts::FUNCTIONS.iter().chain(facts::CARD_B_2026_11_05).copied().collect();
        for x in owned.iter_mut().filter(|x| x.bus == 0) {
            if let Some(m) = moved.iter().find(|m| (m.0, m.1) == (x.dev, x.func)) {
                x.below = m.2;
            }
        }
        let t = Tree::new(owned.iter(), WINDOW);
        let p = plan_roots(WINDOW, &t.claimed(0, WINDOW.end), &t);
        assert_eq!(p.roots, [Range::new(0x00, 0x7f), Range::new(0x80, 0xdf)]);
    }

    #[test]
    fn bus_80_is_found_at_device_14_because_it_has_no_device_0() {
        let t = gaming_pc();
        let p = plan_roots(WINDOW, &t.claimed(0, WINDOW.end), &t);
        assert_eq!(p.peers, [Peer { bus: 0x80, first_device: 0x14, vendor: 0x8086, reach: 0x86 }]);
    }

    #[test]
    fn a_tree_with_one_root_comes_back_unchanged() {
        // The Mini-PC's bridges (plan/hw/gx-780m-minipc-2026-10-30/c1/hw_info.txt): everything hangs below bus 0,
        // one MCFG range 00-ff, two wide hot-plug ranges.
        let port = |dev, func, below| Function { bus: 0, dev, func, vendor: 0x1022, device: 0x14ee, class: 0x06_04_00, below, what: "port" };
        let end = |bus, vendor, device, class| Function { bus, dev: 0, func: 0, vendor, device, class, below: (0, 0), what: "device" };
        let table = [
            Function { bus: 0, dev: 0, func: 0, vendor: 0x1022, device: 0x14e8, class: 0x06_00_00, below: (0, 0), what: "host bridge" },
            port(1, 1, (0x01, 0x01)),
            port(1, 3, (0x02, 0x02)),
            port(2, 1, (0x03, 0x03)),
            port(2, 2, (0x04, 0x04)),
            port(2, 4, (0x05, 0x05)),
            port(2, 5, (0x06, 0x06)),
            port(3, 1, (0x07, 0x66)),
            port(4, 1, (0x67, 0xc6)),
            port(8, 1, (0xc7, 0xc7)),
            port(8, 2, (0xc8, 0xc8)),
            port(8, 3, (0xc9, 0xc9)),
            end(0x01, 0x126f, 0x2268, 0x01_08_02),
            end(0x02, 0x8086, 0x125c, 0x02_00_00),
            end(0x04, 0x10ec, 0xb852, 0x02_80_00),
            end(0xc7, 0x1002, 0x1900, 0x03_00_00),
            end(0xc9, 0x1022, 0x14ec, 0x13_00_00),
        ];
        let window = Range::new(0, 0xff);
        let t = Tree::new(table.iter(), window);
        let p = plan_roots(window, &t.claimed(0, 0xff), &t);
        assert!(p.is_input(window));
        assert_eq!((p.peers.len(), p.refused), (0, None));
        // Only buses ca-ff are outside every bridge range; they were looked at and nothing answered.
        assert_eq!(p.probes, (0xff - 0xca + 1) * 32);
    }

    #[test]
    fn a_bus_inside_a_bridge_range_is_never_a_root() {
        let t = gaming_pc();
        let claimed = t.claimed(0, WINDOW.end);
        let before = t.asked.get();
        let p = plan_roots(WINDOW, &claimed, &t);
        // Buses 01-58 hang below bus 0's ports and 81-86 below bus 80's: none of them is a root although each answers.
        assert!(p.peers.iter().all(|x| x.bus == 0x80));
        assert!(p.roots.iter().all(|r| r.start == 0x00 || r.start == 0x80));
        // None of bus 0's claimed buses was even read: 0x59..=0x7f, then bus 80 up to device 0x14, then 0x87..=0xdf.
        let unclaimed = (0x7f - 0x59 + 1) + (0xdf - 0x87 + 1);
        assert_eq!(t.asked.get() - before, unclaimed * 32 + 0x15);
        assert_eq!(p.probes, unclaimed * 32 + 0x15);
    }

    #[test]
    fn all_ones_and_zero_are_nobody() {
        struct Fixed(u16);
        impl Answers for Fixed {
            fn vendor(&self, _: u8, _: u8) -> u16 {
                self.0
            }
            fn reach(&self, _: u8, _: u8) -> u8 {
                panic!("nothing answered, so nothing has a reach")
            }
        }
        for v in [0xffff, 0x0000] {
            let p = plan_roots(WINDOW, &[Range::new(0, 0)], &Fixed(v));
            assert!(p.is_input(WINDOW) && p.refused.is_none(), "{v:#x}");
        }
    }

    /// Answers with a vendor on every bus and device, and claims whatever reach it is built with.
    struct Everywhere {
        reach: fn(u8, u8) -> u8,
        asked: Cell<u32>,
    }
    impl Answers for Everywhere {
        fn vendor(&self, _: u8, _: u8) -> u16 {
            self.asked.set(self.asked.get() + 1);
            0x1af4
        }
        fn reach(&self, root: u8, limit: u8) -> u8 {
            (self.reach)(root, limit)
        }
    }

    #[test]
    fn a_device_answering_everywhere_is_refused_after_a_few_reads() {
        let h = Everywhere { reach: |root, _| root, asked: Cell::new(0) };
        let p = plan_roots(WINDOW, &[Range::new(0, 0)], &h);
        assert_eq!(p.refused, Some(Refusal::TooManyPeers));
        assert!(p.is_input(WINDOW) && p.peers.is_empty());
        assert_eq!(h.asked.get(), MAX_PEERS as u32 + 1, "one read per believed bus, then the one too many");
    }

    #[test]
    fn a_hostile_reach_cannot_leave_the_range_or_go_backwards() {
        let past = Everywhere { reach: |_, _| 0xff, asked: Cell::new(0) };
        let p = plan_roots(WINDOW, &[Range::new(0, 0)], &past);
        assert_eq!(p.roots, [Range::new(0, 0), Range::new(1, 0xdf)]);
        assert_eq!(p.peers[0].reach, 0xdf);
        let back = Everywhere { reach: |_, _| 0, asked: Cell::new(0) };
        let p = plan_roots(WINDOW, &[Range::new(0, 0)], &back);
        assert_eq!(p.refused, Some(Refusal::TooManyPeers), "reach below the bus counts as the bus itself: it ends");
    }

    #[test]
    fn the_end_of_the_ecam_range_is_kept_and_never_passed() {
        // `Tree::vendor` panics on a read outside the range; a range ending at 0xff must not wrap either.
        let t = gaming_pc();
        let p = plan_roots(WINDOW, &t.claimed(0, WINDOW.end), &t);
        assert_eq!(p.roots.last().map(|r| r.end), Some(0xdf));
        let full = Range::new(0, 0xff);
        let t = Tree::new(facts::FUNCTIONS.iter(), full);
        let p = plan_roots(full, &t.claimed(0, 0xff), &t);
        assert_eq!(p.roots, [Range::new(0x00, 0x7f), Range::new(0x80, 0xff)]);
        // A range that does not start at 0 keeps its start.
        let high = Range::new(0x40, 0xdf);
        let t = Tree::new(facts::FUNCTIONS.iter().filter(|x| x.bus >= 0x80), high);
        let p = plan_roots(high, &[Range::new(0x40, 0x40)], &t);
        assert_eq!(p.roots, [Range::new(0x40, 0x7f), Range::new(0x80, 0xdf)]);
    }

    #[test]
    fn two_peers_give_three_ranges() {
        let extra = Function { bus: 0xc0, dev: 3, func: 0, vendor: 0x8086, device: 1, class: 0x06_04_00, below: (0xc1, 0xc4), what: "port" };
        let t = Tree::new(facts::FUNCTIONS.iter().chain(core::iter::once(&extra)), WINDOW);
        let p = plan_roots(WINDOW, &t.claimed(0, WINDOW.end), &t);
        assert_eq!(p.roots, [Range::new(0x00, 0x7f), Range::new(0x80, 0xbf), Range::new(0xc0, 0xdf)]);
        assert_eq!(p.peers[1], Peer { bus: 0xc0, first_device: 3, vendor: 0x8086, reach: 0xc4 });
    }

    #[test]
    fn a_first_root_bridge_reaching_past_a_peer_refuses_the_cut() {
        // A port on bus 0 forwarding to 90-9f while bus 80 answers as a root: cutting at 80 would orphan 90-9f.
        let t = gaming_pc();
        let mut claimed = t.claimed(0, WINDOW.end);
        claimed.push(Range::new(0x90, 0x9f));
        let p = plan_roots(WINDOW, &claimed, &t);
        assert_eq!(p.refused, Some(Refusal::ClaimAbovePeer { peer: 0x80, claim_start: 0x90 }));
        assert!(p.is_input(WINDOW));
    }

    #[test]
    fn a_backwards_range_is_refused_without_a_read() {
        let t = gaming_pc();
        let w = Range::new(0x10, 0x0f);
        let p = plan_roots(w, &[], &t);
        assert_eq!((p.refused, p.probes, t.asked.get()), (Some(Refusal::Backwards), 0, 0));
        assert!(p.is_input(w));
    }

    #[test]
    fn a_range_of_one_bus_has_nothing_to_probe() {
        let t = gaming_pc();
        let w = Range::new(0, 0);
        let p = plan_roots(w, &[w], &t);
        assert!(p.is_input(w));
        assert_eq!(p.probes, 0);
    }
}
