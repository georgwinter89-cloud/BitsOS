//! PC-4: walking the buses again after boot (hot plug, a function that was not ready, a driver restart),
//! and the "not ready" (CRS) wait shared with `scan`.
//!
//! A rescan keeps every function it finds again exactly as it was (same node, same BARs, same untouched
//! key): **BARs are never sized again for a function already in the tree**, bound or not, because sizing
//! switches decoding off under a running driver. Only functions that are new get the untouched rule and
//! BAR sizing; functions that are gone lose their driver (its remove hook) and their record. An untouched
//! function stays untouched, and a touched one stays touched, whatever appears next to it (R8d).
//!
//! A function that still answers "not ready" (CRS) at the deadline is not gone: it is held in the tree
//! unchanged (`held`), with its other functions when it is function 0 and with everything below it when it
//! is a bridge, so a device in reset never loses its driver to one slow rescan (Linux likewise never
//! removes a device on a CRS timeout; it only skips adding a new one).
//!
//! Between walks the caller sleeps with no lock held (B4); a deadline, not a retry count, ends the wait (R1).

use crate::enumerate::Walk;
use crate::node::Node;
use crate::{bars, caps, optin, ConfigAccess, Loc, Problem};
use alloc::vec::Vec;

/// How long a walk waits for functions that answer "not ready" (CRS) before leaving them out, and how
/// often it looks again.
pub const CRS_WAIT_NS: u64 = 1_000_000_000;
pub const CRS_POLL_NS: u64 = 10_000_000;

/// What the CRS loop ended with.
#[derive(Debug, Default)]
pub struct Crs {
    pub nodes: Vec<Node>,
    /// Still "not ready" at the deadline (left out of the tree).
    pub timed_out: Vec<Loc>,
    pub waited_ns: u64,
    pub waits: usize,
}

/// Walks until no function answers "not ready" or the deadline passed. `walk` takes its own lock; `wait`
/// sleeps (it is called with no lock held and gets the functions still not ready). Without a running clock
/// (`clock` false) there is no deadline to wait for, so there is no retry at all.
pub fn crs_loop(walk: &mut dyn FnMut() -> Walk, now: &dyn Fn() -> u64, clock: bool, wait: &mut dyn FnMut(u64, &[Loc])) -> Crs {
    let start = now();
    let mut waits = 0;
    loop {
        let w = walk();
        let elapsed = now().saturating_sub(start);
        if w.not_ready.is_empty() {
            return Crs { nodes: w.nodes, timed_out: Vec::new(), waited_ns: elapsed, waits };
        }
        if !clock || elapsed >= CRS_WAIT_NS {
            return Crs { nodes: w.nodes, timed_out: w.not_ready, waited_ns: elapsed, waits };
        }
        wait(CRS_POLL_NS, &w.not_ready);
        waits += 1;
    }
}

/// The same function as before: same place, IDs, class and header type. A different card in the slot is
/// "gone" plus "new".
pub fn same(a: &Node, b: &Node) -> bool {
    a.loc == b.loc && a.vendor == b.vendor && a.device == b.device && a.class == b.class && a.header_type & 0x7F == b.header_type & 0x7F
}

/// Sizes one function's BARs unless it is untouched, has no BARs, or sits in D1-D3hot (PC-2's
/// `enable_device` sizes it after the wake). True when it was sized.
pub fn size_node(c: &dyn ConfigAccess, n: &mut Node, phys_limit: u64) -> bool {
    if n.untouched.is_some() || bars::slots(n.header_type) == 0 {
        return false;
    }
    if let Some(caps::CapInfo::Pm { state, .. }) = n.cap(caps::id::PM).map(|c| c.info)
        && state != 0
    {
        n.problems.push(Problem::BarPowerState { state });
        return false;
    }
    let keep = n.class >> 8 == 0x0600;
    let b = bars::size_bars(c, n.loc, n.header_type, keep, phys_limit, &mut n.problems);
    caps::check_msix(&n.caps, &b, &mut n.problems);
    n.bars = Some(b);
    true
}

/// The tree after a rescan.
#[derive(Debug)]
pub struct Merged {
    /// In walk order (parents before children; `parent` indices into this list).
    pub nodes: Vec<Node>,
    /// Indices (into `nodes`) of the functions that are new.
    pub added: Vec<usize>,
    /// Functions of the old tree the walk no longer found (and not held).
    pub gone: Vec<Node>,
    /// Functions of the old tree the walk could not judge (still "not ready", or behind one that was):
    /// kept unchanged at the end of `nodes`, never counted as gone.
    pub held: Vec<Loc>,
    /// Functions the walk found again.
    pub kept: usize,
    pub sized: usize,
}

/// True when the walk could not see `o` because of a "not ready" answer: `o` itself, function 0 of its
/// device (the other functions are only probed after it), or a parent already held.
fn unseen(o: &Node, not_ready: &[Loc], parent_held: bool) -> bool {
    let f0 = Loc::new(o.loc.segment, o.loc.bus, o.loc.dev, 0);
    parent_held || not_ready.contains(&o.loc) || not_ready.contains(&f0)
}

/// Merges a new walk into the old tree: found-again functions keep their old node (BARs, untouched key,
/// problems, command at scan; only the parent index is renumbered), new ones get the untouched rule and BAR
/// sizing. Old functions the walk could not judge (`not_ready`: still CRS at the deadline) are held
/// unchanged, not gone. Writes only to new functions (the sizing protocol), never to a kept or held one.
pub fn merge(
    c: &dyn ConfigAccess,
    old: &[Node],
    walked: Vec<Node>,
    not_ready: &[Loc],
    key_for: impl Fn(&Node) -> Option<&'static str>,
    phys_limit: u64,
) -> Merged {
    // What a fresh boot would mark untouched; applied to the new functions only.
    let mut fresh = walked.clone();
    optin::mark_untouched(&mut fresh, &key_for);
    let mut nodes = Vec::with_capacity(walked.len());
    let mut added = Vec::new();
    for (i, w) in walked.into_iter().enumerate() {
        match old.iter().find(|o| same(o, &w)) {
            Some(o) => {
                let mut k = o.clone();
                k.parent = w.parent;
                nodes.push(k);
            }
            None => {
                let mut n = w;
                n.untouched = fresh[i].untouched;
                added.push(i);
                nodes.push(n);
            }
        }
    }
    let kept = nodes.len() - added.len();
    // Old nodes are in walk order (parents first), so a held parent is known before its children.
    let mut held_old: Vec<usize> = Vec::new();
    for (i, o) in old.iter().enumerate() {
        let found = nodes.iter().any(|n| same(n, o));
        // A new function now sits at that place: the old one is gone, whatever the walk missed.
        let taken = nodes.iter().any(|n| n.loc == o.loc);
        let parent_held = o.parent.is_some_and(|p| held_old.contains(&p));
        if !found && !taken && unseen(o, not_ready, parent_held) {
            held_old.push(i);
        }
    }
    let mut held = Vec::with_capacity(held_old.len());
    for &i in &held_old {
        let mut k = old[i].clone();
        k.parent = old[i].parent.and_then(|p| nodes.iter().position(|n| same(n, &old[p])));
        held.push(k.loc);
        nodes.push(k);
    }
    let gone: Vec<Node> = old.iter().filter(|o| !nodes.iter().any(|n| same(n, o))).cloned().collect();
    let mut sized = 0;
    for &i in &added {
        if size_node(c, &mut nodes[i], phys_limit) {
            sized += 1;
        }
    }
    Merged { nodes, added, gone, held, kept, sized }
}

/// The order gone functions lose their drivers: children before the bridge that carries them (Linux
/// `pci_stop_and_remove_bus_device`). `gone` is in walk order (parents first), so reversed.
pub fn removal_order(gone: &[Node]) -> impl Iterator<Item = &Node> {
    gone.iter().rev()
}

/// What `pci::rescan` did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Rescan {
    pub added: Vec<Loc>,
    pub gone: Vec<Loc>,
    /// Kept unchanged because they still answered "not ready" (or sit behind one that did): not judged.
    pub held: Vec<Loc>,
    pub kept: usize,
    /// Functions whose BARs were sized (new ones only).
    pub sized: usize,
    /// New functions a driver took.
    pub bound: usize,
}
