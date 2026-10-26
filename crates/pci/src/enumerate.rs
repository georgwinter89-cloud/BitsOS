//! The walk (reads only): from each root bus through the bridges as the firmware numbered them
//! (primary/secondary/subordinate are never rewritten), every device, every function of a multi-function
//! device. Vendor 0xFFFF and 0x0000 are both "absent" (spec/core.md §11 trap 3).
//!
//! Hostile bus numbers cannot loop the walk: a bus is scanned at most once, a secondary bus must lie above
//! the bridge's own bus and inside its parent's range, and the walk is a work list, not recursion, so a deep
//! chain cannot exhaust the kernel stack.
//!
//! A function still initialising answers vendor 0x0001 (Configuration Request Retry Status with software
//! visibility, PCIe r5 §2.3.1): it is never added with that vendor; the walk reports it as not ready and
//! `scan` walks again after a short wait, up to a deadline (Linux `pci_bus_wait_crs`).
//!
//! Each MCFG range is walked from its first bus (the root bus BitsOS assumes per range; root buses only AML
//! names are not probed, see the PC-1 notes). A range starting inside one already walked is skipped with an
//! event, so overlapping ranges cannot list a function twice.

use crate::access::{read16, read8, vendor_present, ConfigAccess, Loc};
use crate::caps::{self, CapInfo};
use crate::node::{BridgeInfo, Node, PcieInfo, Problem, Window};
use alloc::vec::Vec;

/// A root bus range to start from (an MCFG entry: segment, first and last bus).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Root {
    pub segment: u16,
    pub bus_start: u8,
    pub bus_end: u8,
}

struct Pending {
    segment: u16,
    bus: u8,
    parent: Option<usize>,
    /// Allowed bus range for bridges found on this bus (the parent's secondary..=subordinate).
    lo: u8,
    hi: u8,
}

/// Vendor ID a function answers while it is not ready yet (CRS software visibility); never a real vendor.
pub const CRS_VENDOR: u16 = 0x0001;

/// What one walk found: the tree, and the functions that answered "not ready yet" (not in the tree).
#[derive(Debug, Default)]
pub struct Walk {
    pub nodes: Vec<Node>,
    pub not_ready: Vec<Loc>,
}

/// Reads one function's header. `None` if nothing is there (or it is not ready: vendor 0x0001).
pub fn probe(c: &dyn ConfigAccess, at: Loc, parent: Option<usize>, space: u16) -> Option<Node> {
    let id = c.read32(at, 0);
    let vendor = id as u16;
    if !vendor_present(vendor) || vendor == CRS_VENDOR {
        return None;
    }
    let cr = c.read32(at, 8);
    let header_type = read8(c, at, 0x0E);
    let cmd_sta = c.read32(at, 4);
    let mut problems = Vec::new();
    let status = (cmd_sta >> 16) as u16;
    let (caps, ext_caps, space) = caps::read_all(c, at, status, space, &mut problems);
    let pcie: Option<PcieInfo> = caps.iter().find_map(|cap| if let CapInfo::Pcie(p) = cap.info { Some(p) } else { None });
    let subsystem = if header_type & 0x7F == 0 {
        let s = c.read32(at, 0x2C);
        (s as u16, (s >> 16) as u16)
    } else {
        (0, 0)
    };
    let bridge = (header_type & 0x7F == 1).then(|| bridge_info(c, at));
    Some(Node {
        loc: at,
        vendor,
        device: (id >> 16) as u16,
        class: cr >> 8,
        revision: cr as u8,
        header_type,
        subsystem,
        command_at_scan: cmd_sta as u16,
        status_at_scan: status,
        interrupt_pin: read8(c, at, 0x3D),
        parent,
        space,
        bridge,
        caps,
        ext_caps,
        pcie,
        bars: None,
        untouched: None,
        problems,
    })
}

/// Bus numbers and forwarding windows of a type-1 header (PCI-to-PCI Bridge spec 1.2 §3.2).
pub fn bridge_info(c: &dyn ConfigAccess, at: Loc) -> BridgeInfo {
    let buses = c.read32(at, 0x18);
    let io_reg = read16(c, at, 0x1C);
    let io = {
        let (b, l) = (io_reg & 0xFF, io_reg >> 8);
        if b & 0xF == 1 {
            // 32-bit I/O addressing: upper halves at 0x30.
            let up = c.read32(at, 0x30);
            let base = ((up & 0xFFFF) as u64) << 16 | ((b & 0xF0) as u64) << 8;
            let limit = ((up >> 16) as u64) << 16 | ((l & 0xF0) as u64) << 8 | 0xFFF;
            (base <= limit).then_some(Window { start: base, end: limit })
        } else {
            let base = ((b & 0xF0) as u64) << 8;
            let limit = ((l & 0xF0) as u64) << 8 | 0xFFF;
            (io_reg != 0 && base <= limit).then_some(Window { start: base, end: limit })
        }
    };
    let m = c.read32(at, 0x20);
    let mem = {
        let base = ((m & 0xFFF0) as u64) << 16;
        let limit = ((m >> 16) & 0xFFF0) as u64;
        let limit = limit << 16 | 0xF_FFFF;
        // All zero: no memory window implemented (firmware closes a window with base above limit).
        (m != 0 && base <= limit).then_some(Window { start: base, end: limit })
    };
    let p = c.read32(at, 0x24);
    let prefetch = {
        let is64 = p & 0xF == 1;
        let (ub, ul) = if is64 { (c.read32(at, 0x28) as u64, c.read32(at, 0x2C) as u64) } else { (0, 0) };
        let base = ub << 32 | ((p & 0xFFF0) as u64) << 16;
        let limit = ul << 32 | (((p >> 16) & 0xFFF0) as u64) << 16 | 0xF_FFFF;
        (p != 0 && base <= limit).then_some(Window { start: base, end: limit })
    };
    BridgeInfo {
        primary: buses as u8,
        secondary: (buses >> 8) as u8,
        subordinate: (buses >> 16) as u8,
        io,
        mem,
        prefetch,
        control: read16(c, at, 0x3E),
    }
}

/// Walks every root range. Nodes come out in walk order: a bridge always before what sits below it.
pub fn enumerate(c: &dyn ConfigAccess, roots: &[Root]) -> Vec<Node> {
    walk(c, roots).nodes
}

/// `enumerate` plus the functions that were not ready.
pub fn walk(c: &dyn ConfigAccess, roots: &[Root]) -> Walk {
    let mut w = Walk::default();
    // Buses already claimed (by a root or a bridge), per segment: the first claim wins, a hostile bridge
    // pointing at a claimed bus is not followed.
    let mut claimed: Vec<(u16, u8)> = Vec::new();
    let mut walked: Vec<Root> = Vec::new();
    for r in roots {
        let inside = walked.iter().any(|o| o.segment == r.segment && (o.bus_start..=o.bus_end).contains(&r.bus_start));
        if r.bus_end < r.bus_start || inside || claimed.contains(&(r.segment, r.bus_start)) {
            klog::event!(Warn, "pci", "root.skipped", segment = r.segment, bus_start = r.bus_start, bus_end = r.bus_end;
                "bus range starts inside one already walked (or is backwards): not walked again");
            continue;
        }
        walked.push(*r);
        claimed.push((r.segment, r.bus_start));
        let mut work = alloc::vec![Pending { segment: r.segment, bus: r.bus_start, parent: None, lo: r.bus_start, hi: r.bus_end }];
        while let Some(p) = work.pop() {
            let found = scan_bus(c, &p, &mut w);
            let mut follow = Vec::new();
            for &i in &found {
                let Some(b) = w.nodes[i].bridge else { continue };
                if let Some(problem) = check_bridge(&b, &p, c, &claimed) {
                    w.nodes[i].problems.push(problem);
                    if !matches!(problem, Problem::BridgePrimary { .. }) {
                        continue;
                    }
                }
                claimed.push((p.segment, b.secondary));
                follow.push(Pending { segment: p.segment, bus: b.secondary, parent: Some(i), lo: b.secondary, hi: b.subordinate });
            }
            // Reversed onto the stack so the first bridge's subtree is walked first (lspci order).
            work.extend(follow.into_iter().rev());
        }
    }
    w
}

/// Why a bridge's secondary bus is not followed (or `BridgePrimary`, which is only noted).
fn check_bridge(b: &BridgeInfo, on: &Pending, c: &dyn ConfigAccess, claimed: &[(u16, u8)]) -> Option<Problem> {
    if b.secondary == 0 || b.subordinate < b.secondary {
        return Some(Problem::BridgeUnconfigured { secondary: b.secondary, subordinate: b.subordinate });
    }
    let below = b.secondary > on.bus && b.secondary >= on.lo && b.subordinate <= on.hi;
    if !below || claimed.contains(&(on.segment, b.secondary)) {
        return Some(Problem::BridgeBadBus { secondary: b.secondary, subordinate: b.subordinate });
    }
    if c.space_size(on.segment, b.secondary) == 0 {
        return Some(Problem::BusUnreachable { bus: b.secondary });
    }
    if b.primary != on.bus {
        return Some(Problem::BridgePrimary { primary: b.primary });
    }
    None
}

/// Every device of one bus; returns the indices of the nodes it added.
fn scan_bus(c: &dyn ConfigAccess, p: &Pending, w: &mut Walk) -> Vec<usize> {
    let mut added = Vec::new();
    let space = c.space_size(p.segment, p.bus);
    if space == 0 {
        return added;
    }
    // Below a PCIe root or downstream port only device 0 exists (Linux `only_one_child`): devices that answer
    // on every device number would otherwise appear 32 times.
    let port = p.parent.filter(|&i| w.nodes[i].pcie.is_some_and(|x| x.is_root_port() || x.is_downstream_port()));
    let devs = if port.is_some() { 1 } else { 32 };
    for dev in 0..devs {
        let at = Loc::new(p.segment, p.bus, dev, 0);
        if read16(c, at, 0) == CRS_VENDOR {
            w.not_ready.push(at);
            continue;
        }
        let Some(f0) = probe(c, at, p.parent, space) else { continue };
        let funcs = if f0.multifunction() { 8 } else { 1 };
        let first = w.nodes.len();
        w.nodes.push(f0);
        added.push(w.nodes.len() - 1);
        for func in 1..funcs {
            let at = Loc::new(p.segment, p.bus, dev, func);
            if read16(c, at, 0) == CRS_VENDOR {
                w.not_ready.push(at);
                continue;
            }
            if let Some(f) = probe(c, at, p.parent, space) {
                w.nodes.push(f);
                added.push(w.nodes.len() - 1);
            }
        }
        if let Some(pi) = port
            && ari_forwarding(c, &w.nodes[pi])
        {
            for n in &mut w.nodes[first..] {
                if ari_next_function(c, n).is_some_and(|next| next >= 8) {
                    n.problems.push(Problem::AriNotEnumerated);
                }
            }
        }
    }
    added
}

/// The ARI capability's Next Function Number (0 = end of the chain); `None` without ARI.
fn ari_next_function(c: &dyn ConfigAccess, n: &Node) -> Option<u8> {
    let cap = n.ext_cap(caps::ext::ARI)?;
    (cap.offset + 8 <= n.space).then(|| (read16(c, n.loc, cap.offset + 4) >> 8) as u8)
}

/// The port forwards ARI (Device Control 2 bit 5; PCIe capability version 2 and later). Only then can a
/// chain reach functions 8-255, which this walk does not enumerate (it notes them).
fn ari_forwarding(c: &dyn ConfigAccess, port: &Node) -> bool {
    let Some(cap) = port.cap(caps::id::PCIE) else { return false };
    let off = cap.offset as u16 + 0x28;
    port.pcie.is_some_and(|x| x.version >= 2) && off + 4 <= port.space && read16(c, port.loc, off) & (1 << 5) != 0
}
