//! `pci.cfg` around the walk, a rescan's merge and BAR sizing: held for each access (the walk) or one
//! function's sizing sequence, never while the tree's lists are made. Under a spin lock the heap cannot
//! reclaim, and the tree alone is several pages (Mini-PC, 12.11.2026).

use super::fake::{Fake, FakeFn};
use super::world::minipc;
use super::{keys_off, ALL};
use crate::access::{ConfigAccess, Loc};
use crate::node::Problem;
use crate::{enumerate, rescan};
use alloc::format;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};

/// A back end that notes how many tracked locks its caller holds at each call.
struct Watch<'a> {
    inner: &'a Fake,
    /// Accesses with exactly one lock held / with any other number.
    locked: AtomicUsize,
    other: AtomicUsize,
    /// `space_size` calls (the walk asks once per bus, between that bus's accesses) with no lock / some lock.
    between_free: AtomicUsize,
    between_held: AtomicUsize,
}

impl<'a> Watch<'a> {
    fn new(inner: &'a Fake) -> Self {
        Watch { inner, locked: AtomicUsize::new(0), other: AtomicUsize::new(0), between_free: AtomicUsize::new(0), between_held: AtomicUsize::new(0) }
    }
    fn access(&self) {
        let c = if ksync::lockdep::held_count() == 1 { &self.locked } else { &self.other };
        c.fetch_add(1, Ordering::Relaxed);
    }
}

impl ConfigAccess for Watch<'_> {
    fn read32(&self, at: Loc, off: u16) -> u32 {
        self.access();
        self.inner.read32(at, off)
    }
    fn write32(&self, at: Loc, off: u16, v: u32) -> bool {
        self.access();
        self.inner.write32(at, off, v)
    }
    fn space_size(&self, segment: u16, bus: u8) -> u16 {
        let c = if ksync::lockdep::held_count() == 0 { &self.between_free } else { &self.between_held };
        c.fetch_add(1, Ordering::Relaxed);
        self.inner.space_size(segment, bus)
    }
    fn name(&self) -> &'static str {
        "watch"
    }
}

#[test]
fn the_walk_holds_the_lock_per_access_only() {
    assert!(ksync::lockdep::enabled(), "lockdep off in a debug build: the counts below would mean nothing");
    let w = minipc();
    let watch = Watch::new(&w);
    let (nodes, not_ready) = crate::walk_until_ready(&watch, &ALL);
    assert!(not_ready.is_empty());
    assert_eq!(ksync::lockdep::held_count(), 0);
    // The same tree as the walk without any lock.
    let plain = enumerate::enumerate(&w, &ALL);
    assert_eq!(format!("{nodes:?}"), format!("{plain:?}"));
    // The list is larger than a page here, as on the machine: it grew between accesses, where no lock was held.
    assert!(nodes.capacity() * size_of::<crate::Node>() > 4096, "{} nodes", nodes.len());
    let locked = watch.locked.load(Ordering::Relaxed);
    assert!(locked > 4 * nodes.len(), "every access under pci.cfg: {locked}");
    assert_eq!(watch.other.load(Ordering::Relaxed), 0, "an access without pci.cfg (or under two locks)");
    assert!(watch.between_free.load(Ordering::Relaxed) >= 8, "one question per bus walked");
    assert_eq!(watch.between_held.load(Ordering::Relaxed), 0, "pci.cfg was held between two accesses");
}

/// A function that gives the sizing as many problems as it can: six BARs of the reserved type and an MSI-X
/// capability whose table and PBA lie in no BAR.
fn troubled() -> FakeFn {
    let mut f = FakeFn::new(0x1234, 0x0001, 0x02_00_00, 0);
    f.cmd(0x6).msix(0x70, 0, false, 5);
    for i in 0..6u16 {
        f.w32(0x10 + 4 * i, 0x6);
    }
    f
}

#[test]
fn sizing_under_the_lock_allocates_nothing() {
    let w = Fake::new();
    w.add(0, 0, 0, troubled());
    let mut d3 = FakeFn::new(0x1234, 0x0002, 0x02_00_00, 0);
    d3.cmd(0x6).bar32(0, 0xFE00_0000, 0x1000, false).pm(0x40, 0, 3, false);
    w.add(0, 1, 0, d3);
    let watch = Watch::new(&w);
    for mut n in enumerate::enumerate(&w, &ALL) {
        let before = n.problems.len();
        // What `size_locked` reserves; the capacity must not move while the lock is held.
        n.problems.reserve(rescan::sizing_problems(&n));
        let cap = n.problems.capacity();
        crate::size_locked(&watch, &mut n, 1 << 48);
        assert_eq!(n.problems.capacity(), cap, "{}: the problem list grew under pci.cfg: {:?}", n.loc, n.problems);
        let noted = n.problems.len() - before;
        assert!(noted <= rescan::sizing_problems(&n));
        match n.loc.dev {
            0 => {
                assert_eq!(noted, 8, "{:?}", n.problems);
                assert_eq!(noted, rescan::sizing_problems(&n), "the bound is exact for the worst function");
            }
            _ => assert_eq!(n.problems[before..], [Problem::BarPowerState { state: 3 }]),
        }
    }
    assert!(watch.locked.load(Ordering::Relaxed) > 0);
    assert_eq!(watch.other.load(Ordering::Relaxed), 0, "the whole sizing sequence runs under pci.cfg");
    assert_eq!(ksync::lockdep::held_count(), 0);
}

#[test]
fn a_rescan_merges_with_no_lock_and_sizes_new_functions_under_it() {
    let w = minipc();
    let old = crate::build(&w, &ALL, keys_off);
    // A card appears in the free slot below 00:02.5 (bus 6).
    let mut card = FakeFn::new(0x10EC, 0x8125, 0x02_00_00, 0);
    card.cmd(0).bar64(2, 0xFCD0_0000, 0x1_0000, false);
    w.add(6, 0, 0, card);
    let watch = Watch::new(&w);
    let (walked, not_ready) = crate::walk_until_ready(&watch, &ALL);
    let mut sized_at: Vec<Loc> = Vec::new();
    let m = rescan::merge_with(&old, walked.clone(), &not_ready, keys_off, |n| {
        // Called for the new function only, with nothing held: the lock is `size_locked`'s.
        assert_eq!(ksync::lockdep::held_count(), 0);
        sized_at.push(n.loc);
        crate::size_locked(&watch, n, 1 << 48)
    });
    assert_eq!(sized_at, [Loc::new(0, 6, 0, 0)]);
    assert_eq!((m.added.len(), m.gone.len(), m.kept, m.sized), (1, 0, old.len(), 1));
    assert_eq!(watch.other.load(Ordering::Relaxed), 0);
    assert_eq!(watch.between_held.load(Ordering::Relaxed), 0);
    // The same result as the merge that sizes without any lock (the host tests' `merge`).
    let plain = rescan::merge(&w, &old, walked, &not_ready, keys_off, 1 << 48);
    assert_eq!(format!("{:?}", m.nodes), format!("{:?}", plain.nodes));
}
