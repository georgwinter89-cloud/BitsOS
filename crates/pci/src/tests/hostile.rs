//! Hostile configuration spaces: capability loops and bad lengths, bridges with impossible bus numbers,
//! random garbage. Nothing may panic, loop forever or list a function twice.

use super::fake::{bridge, root_port, Fake, FakeFn};
use super::{find, no_keys, ALL};
use crate::caps::{ext, id, CapInfo, ExtInfo};
use crate::node::Problem;
use crate::{build, Loc};
use alloc::vec::Vec;

fn one(f: FakeFn) -> crate::Node {
    let w = Fake::new();
    w.add(0, 1, 0, f);
    build(&w, &ALL, no_keys).remove(0)
}

#[test]
fn std_capability_loop_and_bad_pointers() {
    let mut f = FakeFn::new(0x1234, 0x2, 0x02_00_00, 0);
    f.cap(0x40, id::PM, 0x50).cap(0x50, id::MSI, 0x40);
    let n = one(f);
    assert_eq!(n.caps.len(), 2);
    assert!(n.problems.contains(&Problem::CapLoop { at: 0x40 }));

    let mut f = FakeFn::new(0x1234, 0x2, 0x02_00_00, 0);
    f.cap(0x40, id::PM, 0x40);
    let n = one(f);
    assert_eq!(n.caps.len(), 1, "self-pointer");
    assert!(n.problems.contains(&Problem::CapLoop { at: 0x40 }));

    let mut f = FakeFn::new(0x1234, 0x2, 0x02_00_00, 0);
    f.cap(0x40, id::PM, 0x10);
    let n = one(f);
    assert!(n.problems.contains(&Problem::CapBadPointer { at: 0x40, ptr: 0x10 }));

    // Low two bits of a pointer are masked (0x53 -> 0x50), not followed literally.
    let mut f = FakeFn::new(0x1234, 0x2, 0x02_00_00, 0);
    f.cap(0x40, id::PM, 0x53).cap(0x50, id::VENDOR, 0).w8(0x52, 4);
    let n = one(f);
    assert_eq!(n.caps.iter().map(|c| c.offset).collect::<Vec<_>>(), [0x40, 0x50]);

    // The longest legal list: 48 capabilities, every dword from 0x40 to 0xFC.
    let mut f = FakeFn::new(0x1234, 0x2, 0x02_00_00, 0);
    for p in (0x40..=0xFCu16).step_by(4) {
        let next = if p == 0xFC { 0 } else { p as u8 + 4 };
        f.cap(p as u8, 0x0D, next);
    }
    let n = one(f);
    assert_eq!(n.caps.len(), 48);
    assert!(n.problems.is_empty());

    // Status bit 4 clear: the pointer is not read even if it is set.
    let mut f = FakeFn::new(0x1234, 0x2, 0x02_00_00, 0);
    f.w8(0x34, 0x40).w8(0x40, id::PM);
    assert!(one(f).caps.is_empty());
}

#[test]
fn std_capability_lengths() {
    // Vendor capability with length 0, and one running past 0x100.
    let mut f = FakeFn::new(0x1234, 0x2, 0x02_00_00, 0);
    f.cap(0x40, id::VENDOR, 0xF8).w8(0x42, 0).cap(0xF8, id::VENDOR, 0).w8(0xFA, 0x20);
    let n = one(f);
    assert_eq!(n.caps.len(), 2);
    assert!(n.caps.iter().all(|c| c.info == CapInfo::Plain));
    assert!(n.problems.contains(&Problem::CapBadLength { id: id::VENDOR as u16, at: 0x40, len: 0 }));
    assert!(n.problems.contains(&Problem::CapBadLength { id: id::VENDOR as u16, at: 0xF8, len: 0x20 }));
    // MSI with 64-bit addresses and masks (0x18 bytes) at 0xF0: does not fit.
    let mut f = FakeFn::new(0x1234, 0x2, 0x02_00_00, 0);
    f.cap(0xF0, id::MSI, 0).w16(0xF2, 0x180);
    let n = one(f);
    assert_eq!(n.caps[0].info, CapInfo::Plain);
    assert!(n.problems.contains(&Problem::CapBadLength { id: id::MSI as u16, at: 0xF0, len: 0x18 }));
    // PCIe v2 at 0xE0: the v1 part (link status) fits, the tail does not: decoded and noted.
    let mut f = FakeFn::new(0x1234, 0x2, 0x02_00_00, 0);
    f.pcie(0xE0, 0, 0);
    let n = one(f);
    assert!(n.pcie.is_some());
    assert!(n.problems.iter().any(|p| matches!(p, Problem::CapBadLength { id: 0x10, .. })));
}

/// An endpoint with a PCIe capability: only such a function has an extended space to walk.
fn pcie_fn() -> FakeFn {
    let mut f = FakeFn::new(0x1234, 0x2, 0x02_00_00, 0);
    f.pcie(0x40, 0, 0);
    f
}

#[test]
fn ext_capability_loops_and_lengths() {
    let mut f = pcie_fn();
    f.ext(0x100, ext::AER, 1, 0x140).ext(0x140, ext::L1SS, 1, 0x100);
    let n = one(f);
    assert_eq!(n.ext_caps.len(), 2);
    assert!(n.problems.contains(&Problem::CapLoop { at: 0x100 }));

    let mut f = pcie_fn();
    f.ext(0x100, ext::AER, 1, 0x100);
    assert_eq!(one(f).ext_caps.len(), 1);

    let mut f = pcie_fn();
    f.ext(0x100, ext::AER, 1, 0x40);
    let n = one(f);
    assert!(n.problems.contains(&Problem::CapBadPointer { at: 0x100, ptr: 0x40 }));

    // VSEC whose length runs past 4 KiB; DVSEC shorter than its header; AER at the very end of the space.
    let mut f = pcie_fn();
    f.ext(0x100, ext::VSEC, 1, 0x200).w32(0x104, 0xFFF0_0001);
    f.ext(0x200, ext::DVSEC, 1, 0xFFC).w32(0x204, 0x0040_8086);
    f.ext(0xFFC, ext::AER, 1, 0);
    let n = one(f);
    assert_eq!(n.ext_caps.len(), 3);
    assert!(n.ext_caps.iter().all(|c| c.info == ExtInfo::Plain));
    assert!(n.problems.contains(&Problem::CapBadLength { id: ext::VSEC, at: 0x100, len: 0xFFF }));
    assert!(n.problems.contains(&Problem::CapBadLength { id: ext::DVSEC, at: 0x200, len: 4 }));
    assert!(n.problems.contains(&Problem::CapBadLength { id: ext::AER, at: 0xFFC, len: 0x14 }));

    // A good VSEC.
    let mut f = pcie_fn();
    f.ext(0x100, ext::VSEC, 1, 0).w32(0x104, 0x0180_0002);
    assert_eq!(one(f).ext_caps[0].info, ExtInfo::Vsec { vsec_id: 2, rev: 0, len: 0x18 });

    // A chain through every dword of the extended space ends without a problem.
    let mut f = pcie_fn();
    for p in (0x100..0x1000u16).step_by(4) {
        let next = if p == 0xFFC { 0 } else { p + 4 };
        f.ext(p, 0x0019, 1, next);
    }
    let n = one(f);
    assert_eq!(n.ext_caps.len(), 960);
    assert!(n.problems.is_empty());
}

#[test]
fn msix_table_outside_its_bar() {
    let mut f = FakeFn::new(0x1234, 0x2, 0x02_00_00, 0);
    f.bar32(0, 0xF000_0000, 0x1000, false);
    // 2048 entries (32 KiB) in a 4 KiB BAR, PBA in BIR 7 (does not exist).
    f.cap(0x40, id::MSIX, 0).w16(0x42, 0x7FF).w32(0x44, 0).w32(0x48, 7);
    let n = one(f);
    assert!(n.problems.contains(&Problem::MsixOutside { bir: 0, offset: 0 }));
    assert!(n.problems.contains(&Problem::MsixOutside { bir: 7, offset: 0 }));
}

#[test]
fn bridges_with_impossible_bus_numbers() {
    let w = Fake::new();
    w.add(0, 0, 0, FakeFn::new(0x8086, 0x29C0, 0x06_00_00, 0));
    w.add(0, 1, 0, bridge(0x1234, 1, 0, 0, 0)); // unconfigured
    w.add(0, 2, 0, bridge(0x1234, 2, 0, 5, 3)); // subordinate below secondary
    w.add(0, 3, 0, bridge(0x1234, 3, 0, 1, 2)); // good: 1..2
    w.add(0, 4, 0, bridge(0x1234, 4, 0, 1, 1)); // same secondary again
    w.add(0, 5, 0, bridge(0x1234, 5, 7, 3, 3)); // primary mismatch (followed)
    w.add(1, 0, 0, bridge(0x1234, 6, 1, 0, 0)); // points back to bus 0
    w.add(1, 1, 0, bridge(0x1234, 7, 1, 1, 2)); // points at its own bus
    w.add(1, 2, 0, bridge(0x1234, 8, 1, 2, 9)); // subordinate outside the parent's range
    w.add(3, 0, 0, FakeFn::new(0x8086, 0x10D3, 0x02_00_00, 0));
    let nodes = build(&w, &ALL, no_keys);
    let p = |b, d| find(&nodes, b, d, 0).unwrap().problems.clone();
    assert_eq!(p(0, 1), [Problem::BridgeUnconfigured { secondary: 0, subordinate: 0 }]);
    assert_eq!(p(0, 2), [Problem::BridgeUnconfigured { secondary: 5, subordinate: 3 }]);
    assert!(p(0, 3).is_empty());
    assert_eq!(p(0, 4), [Problem::BridgeBadBus { secondary: 1, subordinate: 1 }]);
    assert_eq!(p(0, 5), [Problem::BridgePrimary { primary: 7 }]);
    assert!(find(&nodes, 3, 0, 0).is_some(), "the mismatched bridge's bus is still walked");
    assert_eq!(p(1, 0), [Problem::BridgeUnconfigured { secondary: 0, subordinate: 0 }]);
    assert_eq!(p(1, 1), [Problem::BridgeBadBus { secondary: 1, subordinate: 2 }]);
    assert_eq!(p(1, 2), [Problem::BridgeBadBus { secondary: 2, subordinate: 9 }]);
    // Nothing listed twice.
    let mut locs: Vec<Loc> = nodes.iter().map(|n| n.loc).collect();
    let all = locs.len();
    locs.sort();
    locs.dedup();
    assert_eq!(locs.len(), all);
}

#[test]
fn bus_outside_the_ecam_window() {
    let mut w = Fake::new();
    w.reach = (0, 3);
    w.add(0, 1, 0, root_port(0, 4, 4));
    w.add(4, 0, 0, FakeFn::new(0x8086, 0x10D3, 0x02_00_00, 0));
    let nodes = build(&w, &ALL, no_keys);
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0].problems, [Problem::BusUnreachable { bus: 4 }]);
}

#[test]
fn a_deep_chain_does_not_need_recursion() {
    // 200 nested bridges, each on its own bus, a device at the bottom.
    let w = Fake::new();
    for b in 0..200u8 {
        w.add(b, 0, 0, bridge(0x1B36, 1, b, b + 1, 200));
    }
    w.add(200, 0, 0, FakeFn::new(0x8086, 0x10D3, 0x02_00_00, 0));
    let nodes = build(&w, &ALL, no_keys);
    assert_eq!(nodes.len(), 201);
    assert_eq!(crate::node::ancestors(&nodes, 200).len(), 200);
}

/// xorshift64*: deterministic garbage.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

#[test]
fn random_spaces_never_panic() {
    let mut r = Rng(0x9E37_79B9_7F4A_7C15);
    for round in 0..300 {
        let w = Fake::new();
        let count = 1 + (r.next() % 24) as usize;
        for _ in 0..count {
            let bus = (r.next() % 6) as u8;
            let dev = (r.next() % 32) as u8;
            let func = if r.next().is_multiple_of(3) { (r.next() % 8) as u8 } else { 0 };
            let mut f = FakeFn::new(1 + (r.next() % 0xFFFE) as u16, r.next() as u16, (r.next() as u32) & 0xFF_FFFF, (r.next() % 4) as u8 | if r.next().is_multiple_of(2) { 0x80 } else { 0 });
            // Garbage everywhere past the IDs, then the IDs and header type back in place.
            let keep: Vec<u8> = f.cfg[..0x10].to_vec();
            for b in f.cfg.iter_mut() {
                *b = r.next() as u8;
            }
            f.cfg[..0x10].copy_from_slice(&keep);
            // Bridges get small bus numbers sometimes so the walk follows them.
            if f.cfg[0x0E] & 0x7F == 1 && r.next().is_multiple_of(2) {
                f.buses(bus, (r.next() % 8) as u8, (r.next() % 8) as u8);
            }
            for m in f.bar_mask.iter_mut() {
                *m = r.next() as u32;
            }
            w.add(bus, dev, func, f);
        }
        if round % 2 == 0 {
            w.fns.lock().unwrap().values_mut().for_each(|f| f.cfg[6] |= 0x10);
        }
        let nodes = build(&w, &ALL, no_keys);
        let mut locs: Vec<Loc> = nodes.iter().map(|n| n.loc).collect();
        let n = locs.len();
        locs.sort();
        locs.dedup();
        assert_eq!(locs.len(), n, "round {round}: a function listed twice");
        assert!(n <= count, "round {round}");
        let _ = crate::tree::lines(&nodes, "fake");
    }
}
