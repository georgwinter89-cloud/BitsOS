//! The free scanner's skip hints (`buddy::compact`, `Targets`): whole-zone runs neither read nor set them, and a
//! pageblock that is free whole gets none. 12.11.2026, ktest series: `compact_memory` right after a burst of runs
//! for small orders found 903 of about 1000 pageblocks hinted and moved next to nothing (`mm.migrate.compact`).
use super::*;
use crate::buddy::How;
use crate::compact::{Migrate, Moved};
use crate::Buddy;
use std::boxed::Box;

const BASE: u64 = 2 * MIB;

/// A migrator that copies: the target from the free scanner, the old frame freed.
pub(super) struct Mover(pub(super) &'static Buddy);

impl Migrate for Mover {
    fn migrate<'b>(&self, phys: u64, kind: Kind, owner: u64, dest: &mut dyn FnMut() -> Option<crate::Target<'b>>) -> Moved {
        let Some(t) = dest() else { return Moved::Stays };
        let d = t.into_phys();
        self.0.set_owner(d, owner);
        self.0.set_owner(phys, 0);
        self.0.free(phys, 0, kind);
        Moved::Moved
    }
}

/// Every other frame of pageblocks [from, to) of `cached` memory goes: holes, no two of them buddies.
pub(super) fn holes(b: &Buddy, from: u64, to: u64) {
    for i in (from * 512..to * 512).step_by(2) {
        let p = BASE + i * PAGE;
        b.set_owner(p, 0);
        b.free(p, 0, Kind::PageCache);
    }
    b.drain_all();
}

#[test]
fn a_whole_zone_run_takes_targets_from_pageblocks_with_stale_hints() {
    // Eight pageblocks of clean cache, holes only in the lowest: a run for an order finds no target above the
    // chunk it wants to clear and leaves a hint on every pageblock it looked at.
    let (b, _c) = super::compact::cached(16);
    assert!(b.set_migrator(Box::leak(Box::new(Mover(b)))));
    holes(b, 0, 1);
    b.compact_direct(3);
    let hinted = b.skip_hints();
    assert!(hinted >= 6, "the free scanner hinted {hinted} pageblocks that had no free frame");
    // Now the upper six pageblocks have holes: the hints are stale.
    holes(b, 2, 8);
    let before = b.compact_stats();
    let s = b.compact_all(How::Manual, &mut || {});
    assert!(s.cleared >= 2 && s.moved >= 512, "the pageblocks at the bottom move into the holes above: {s:?}");
    assert!(b.has_free_block(9));
    assert!(b.compact_stats().isolated - before.isolated >= 512, "targets came from the hinted pageblocks");
    assert!(b.skip_hints() >= hinted, "a whole-zone run clears no hint either");
    b.check().unwrap();
}

/// A run for a 4 MiB block over 32 MiB of cache with holes in pageblocks 3..15, the top pageblock full of cache
/// or free whole: the hints it leaves.
fn hints_with_the_top_pageblock(free_whole: bool) -> u64 {
    let (b, _c) = super::compact::cached(32);
    assert!(b.set_migrator(Box::leak(Box::new(Mover(b)))));
    if free_whole {
        for i in 15 * 512..16 * 512 {
            let p = BASE + i * PAGE;
            b.set_owner(p, 0);
            b.free(p, 0, Kind::PageCache);
        }
    }
    holes(b, 3, 15);
    assert!(b.has_free_block(9) == free_whole && !b.has_free_block(10));
    assert!(b.compact_direct(10), "{:?}", b.compact_stats());
    assert_eq!(b.alloc(10, Zone::Normal, Kind::Other), Some(4 * MIB), "the aligned chunk at 4 MiB moved into the holes");
    b.check().unwrap();
    b.skip_hints()
}

#[test]
fn a_pageblock_that_is_free_whole_gets_no_hint() {
    // Full of cache it has no free frame for the scanner: a hint. Free whole it is no target by rule: passed, and
    // no hint that would keep the scanner off it once it is in use and has holes.
    assert_eq!(hints_with_the_top_pageblock(true) + 1, hints_with_the_top_pageblock(false));
}
