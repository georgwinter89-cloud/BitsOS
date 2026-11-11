//! Host tests of the walk on a frame arena (no real page tables needed).

extern crate std;
use super::*;
use std::collections::BTreeMap;
use std::vec::Vec;

/// Table frames as heap arrays, numbered from 0x1000.
#[derive(Default)]
struct Arena {
    tables: BTreeMap<u64, [u64; 512]>,
    next: u64,
    freed: Vec<u64>,
    fail_after: Option<usize>,
}

impl Tables for Arena {
    fn read(&self, table: u64, i: usize) -> u64 {
        self.tables[&table][i]
    }
    fn write(&mut self, table: u64, i: usize, v: u64) {
        self.tables.get_mut(&table).unwrap()[i] = v;
    }
    fn alloc_table(&mut self) -> Option<u64> {
        if let Some(n) = self.fail_after.as_mut() {
            if *n == 0 {
                return None;
            }
            *n -= 1;
        }
        self.next += 0x1000;
        self.tables.insert(self.next, [0; 512]);
        Some(self.next)
    }
    fn free_table(&mut self, table: u64) {
        assert!(self.tables.remove(&table).is_some(), "double free of {table:#x}");
        self.freed.push(table);
    }
}

fn arena() -> (Arena, u64) {
    let mut a = Arena { next: 0x10_0000, ..Default::default() };
    let root = a.alloc_table().unwrap();
    (a, root)
}

const K: u64 = 0xFFFF_C900_0000_0000;
const RW: u64 = PageFlags::WRITABLE | PageFlags::NX | PageFlags::GLOBAL;

#[test]
fn map_and_translate_every_size() {
    let (mut a, root) = arena();
    map(&mut a, root, K, 0x7C_0000_0000, PageSize::Size1G, RW).unwrap();
    map(&mut a, root, K + (1 << 30), 0x20_0000, PageSize::Size2M, RW | PageFlags::PWT).unwrap();
    map(&mut a, root, K + (2 << 30), 0x5000, PageSize::Size4K, RW | PageFlags::PAT_4K).unwrap();
    let l = translate(&a, root, K + 0x1234_5678).unwrap();
    assert_eq!((l.phys, l.page_virt, l.size), (0x7C_1234_5678, K, PageSize::Size1G));
    assert!(l.flags.contains(PageFlags::GLOBAL | PageFlags::NX | PageFlags::WRITABLE));
    assert!(!l.flags.contains(PageFlags::HUGE), "HUGE is not a leaf flag for callers");
    let l = translate(&a, root, K + (1 << 30) + 0x1F_FFFF).unwrap();
    assert_eq!((l.phys, l.size), (0x3F_FFFF, PageSize::Size2M));
    assert!(l.flags.contains(PageFlags::PWT));
    let l = translate(&a, root, K + (2 << 30) + 8).unwrap();
    assert_eq!((l.phys, l.size), (0x5008, PageSize::Size4K));
    assert!(l.flags.contains(PageFlags::PAT_4K));
    assert_eq!(translate(&a, root, K + (2 << 30) + 0x1000), None);
}

#[test]
fn pat_bit_moves_to_bit_12_in_large_leaves() {
    let (mut a, root) = arena();
    map(&mut a, root, K, 0x40_0000, PageSize::Size2M, RW | PageFlags::PAT_4K).unwrap();
    let pdpt = a.read(root, index(K, 4)) & ADDR_MASK;
    let pd = a.read(pdpt, index(K, 3)) & ADDR_MASK;
    let e = a.read(pd, 0);
    assert!(e & PAT_LARGE != 0 && e & PageFlags::HUGE != 0);
    assert_eq!(e & ADDR_MASK & !PAT_LARGE, 0x40_0000, "the frame address keeps its bits");
    assert!(translate(&a, root, K).unwrap().flags.contains(PageFlags::PAT_4K));
}

#[test]
fn refusals() {
    let (mut a, root) = arena();
    assert_eq!(map(&mut a, root, K + 0x1000, 0, PageSize::Size2M, RW), Err(MapError::NotAligned));
    assert_eq!(map(&mut a, root, K, 0x1000, PageSize::Size2M, RW), Err(MapError::NotAligned));
    map(&mut a, root, K, 0, PageSize::Size2M, RW).unwrap();
    assert_eq!(map(&mut a, root, K, 0, PageSize::Size2M, RW), Err(MapError::AlreadyMapped));
    assert_eq!(map(&mut a, root, K + 0x3000, 0, PageSize::Size4K, RW), Err(MapError::HugePageInTheWay));
    a.fail_after = Some(0);
    assert_eq!(map(&mut a, root, K + (5 << 30), 0, PageSize::Size4K, RW), Err(MapError::NoMemory));
}

#[test]
fn user_leaf_widens_intermediates() {
    let (mut a, root) = arena();
    map(&mut a, root, 0x40_0000, 0x9000, PageSize::Size4K, PageFlags::USER).unwrap();
    let l = translate(&a, root, 0x40_0000).unwrap();
    assert!(l.flags.contains(PageFlags::USER) && !l.flags.contains(PageFlags::WRITABLE));
}

#[test]
fn unmap_frees_empty_tables_but_keeps_shared_pdpts() {
    let (mut a, root) = arena();
    let base = a.tables.len();
    for p in 0..600u64 {
        map(&mut a, root, K + p * 0x1000, 0x100_0000 + p * 0x1000, PageSize::Size4K, RW).unwrap();
    }
    map(&mut a, root, K + (1 << 30), 0x20_0000, PageSize::Size2M, RW).unwrap();
    // PDPT + PD(0) + 2 PTs + PD(1).
    assert_eq!(a.tables.len(), base + 5);
    let mut seen = Vec::new();
    let st = unmap(&mut a, root, K, (1 << 30) + (2 << 20), 3, &mut |u| seen.push(u)).unwrap();
    assert_eq!(st.leaves, 601);
    assert_eq!(st.tables_freed, 4, "2 PTs and 2 PDs; the PDPT stays");
    assert_eq!(a.tables.len(), base + 1);
    assert_eq!(seen[0], Unmapped { virt: K, phys: 0x100_0000, size: PageSize::Size4K, flags: PageFlags(RW | PageFlags::PRESENT) });
    assert_eq!(seen[600].size, PageSize::Size2M);
    assert_eq!(translate(&a, root, K), None);
    // The PDPT is still linked: the next mapping reuses it.
    let pdpt = a.read(root, index(K, 4)) & ADDR_MASK;
    assert!(a.tables.contains_key(&pdpt));
}

#[test]
fn unmap_keeps_tables_with_other_mappings() {
    let (mut a, root) = arena();
    map(&mut a, root, K, 0x1000, PageSize::Size4K, RW).unwrap();
    map(&mut a, root, K + 0x1000, 0x2000, PageSize::Size4K, RW).unwrap();
    let st = unmap(&mut a, root, K, 0x1000, 3, &mut |_| {}).unwrap();
    assert_eq!(st, UnmapStats { leaves: 1, tables_freed: 0 });
    assert_eq!(translate(&a, root, K + 0x1000).unwrap().phys, 0x2000);
}

#[test]
fn unmap_refuses_part_of_a_large_page_before_changing_anything() {
    let (mut a, root) = arena();
    map(&mut a, root, K, 0x1000, PageSize::Size4K, RW).unwrap();
    map(&mut a, root, K + (2 << 20), 0x20_0000, PageSize::Size2M, RW).unwrap();
    assert_eq!(unmap(&mut a, root, K, 3 << 20, 3, &mut |_| {}), Err(MapError::PartialHugePage));
    assert!(translate(&a, root, K).is_some(), "nothing removed");
    assert_eq!(unmap(&mut a, root, K + 1, 0x1000, 3, &mut |_| {}), Err(MapError::NotAligned));
}

#[test]
fn user_teardown_frees_pdpts_too() {
    let (mut a, root) = arena();
    let base = a.tables.len();
    map(&mut a, root, 0x7000_0000, 0x1000, PageSize::Size4K, PageFlags::USER).unwrap();
    let st = unmap(&mut a, root, 0x7000_0000, 0x1000, 4, &mut |_| {}).unwrap();
    assert_eq!(st.tables_freed, 3);
    assert_eq!(a.tables.len(), base);
}

#[test]
fn leaves_are_listed_with_canonical_addresses() {
    let (mut a, root) = arena();
    map(&mut a, root, K + 0x5000, 0xA000, PageSize::Size4K, RW).unwrap();
    map(&mut a, root, K + (4 << 20), 0x40_0000, PageSize::Size2M, RW).unwrap();
    map(&mut a, root, 0x1000, 0xB000, PageSize::Size4K, RW).unwrap();
    let mut v = Vec::new();
    for_each_leaf(&a, root, K, K + (1 << 30), &mut |u| v.push((u.virt, u.phys, u.size)));
    assert_eq!(v, [(K + 0x5000, 0xA000, PageSize::Size4K), (K + (4 << 20), 0x40_0000, PageSize::Size2M)]);
}

#[test]
fn update_leaves_rewrites_flags_and_keeps_frames() {
    let (mut a, root) = arena();
    let u = PageFlags::USER | PageFlags::WRITABLE | PageFlags::NX;
    for i in 0..4u64 {
        map(&mut a, root, 0x40_0000 + i * 0x1000, 0x9000 + i * 0x1000, PageSize::Size4K, u).unwrap();
    }
    map(&mut a, root, 0x80_0000, 0x20_0000, PageSize::Size4K, u).unwrap();
    let ro = PageFlags::USER | PageFlags::NX;
    let n = update_leaves(&mut a, root, 0x40_1000, 0x40_3000, &mut |_| Some(ro)).unwrap();
    assert_eq!(n, 2);
    let l = translate(&a, root, 0x40_1000).unwrap();
    assert_eq!(l.phys, 0xA000);
    assert!(!l.flags.contains(PageFlags::WRITABLE) && l.flags.contains(PageFlags::USER));
    assert!(translate(&a, root, 0x40_0000).unwrap().flags.contains(PageFlags::WRITABLE), "outside the range");
    assert!(translate(&a, root, 0x40_3000).unwrap().flags.contains(PageFlags::WRITABLE), "end is exclusive");
    assert_eq!(update_leaves(&mut a, root, 0x40_1000, 0x40_3000, &mut |_| Some(ro)).unwrap(), 0, "unchanged entries are not counted");
    assert_eq!(update_leaves(&mut a, root, 0x40_0000, 0x90_0000, &mut |u| (u.virt == 0x80_0000).then_some(ro)).unwrap(), 1);
    assert!(!translate(&a, root, 0x80_0000).unwrap().flags.contains(PageFlags::WRITABLE));
    assert_eq!(update_leaves(&mut a, root, 0x90_0000, 0x40_0000, &mut |_| Some(ro)).unwrap(), 0, "empty range");
}

#[test]
fn user_tables_carry_user_even_under_a_kernel_only_leaf() {
    let (mut a, root) = arena();
    map_in(&mut a, root, 0x40_0000, 0x9000, PageSize::Size4K, PageFlags::NX, true).unwrap();
    let l = translate(&a, root, 0x40_0000).unwrap();
    assert!(!l.flags.contains(PageFlags::USER), "the leaf stays kernel-only");
    let ru = PageFlags::USER | PageFlags::NX;
    assert_eq!(update_leaves(&mut a, root, 0x40_0000, 0x40_1000, &mut |_| Some(ru)).unwrap(), 1);
    assert!(translate(&a, root, 0x40_0000).unwrap().flags.contains(PageFlags::USER), "made user later: reachable");
    map(&mut a, root, 0x80_0000, 0xA000, PageSize::Size4K, PageFlags::NX).unwrap();
    assert_eq!(update_leaves(&mut a, root, 0x80_0000, 0x80_1000, &mut |_| Some(ru)).unwrap(), 1);
    assert!(!translate(&a, root, 0x80_0000).unwrap().flags.contains(PageFlags::USER), "plain map keeps kernel tables");
}

#[test]
fn update_leaves_refuses_part_of_a_large_leaf() {
    let (mut a, root) = arena();
    let u = PageFlags::USER | PageFlags::WRITABLE | PageFlags::NX;
    map(&mut a, root, 0x40_0000, 0x20_0000, PageSize::Size2M, u).unwrap();
    map(&mut a, root, 0x80_0000, 0x9000, PageSize::Size4K, u).unwrap();
    let ro = PageFlags::USER | PageFlags::NX;
    let r = update_leaves(&mut a, root, 0x40_1000, 0x40_2000, &mut |_| Some(ro));
    assert!(matches!(r, Err(MapError::PartialHugePage)));
    assert!(translate(&a, root, 0x40_0000).unwrap().flags.contains(PageFlags::WRITABLE), "nothing changed");
    let r = update_leaves(&mut a, root, 0x40_1000, 0x80_1000, &mut |_| Some(ro));
    assert!(matches!(r, Err(MapError::PartialHugePage)));
    assert!(translate(&a, root, 0x80_0000).unwrap().flags.contains(PageFlags::WRITABLE), "checked before any change");
    assert_eq!(update_leaves(&mut a, root, 0x40_0000, 0x80_1000, &mut |_| Some(ro)).unwrap(), 2, "whole leaf inside");
}

#[test]
fn small_entries_map_what_the_huge_leaf_maps() {
    // MM-13: a 2 MiB user leaf (write-combining through PAT, dirty, accessed, NX) split into 4 KiB entries.
    let flags = PageFlags::USER | PageFlags::WRITABLE | PageFlags::ACCESSED | PageFlags::DIRTY | PageFlags::NX | PageFlags::PAT_4K | PageFlags::PWT;
    let pde = 0x4000_0000 | leaf_bits(flags, 2);
    assert!(pde & PAT_LARGE != 0 && pde & PageFlags::HUGE != 0, "the large layout moves PAT to bit 12");
    for i in [0usize, 1, 255, 511] {
        let e = small_entry(pde, i);
        assert_eq!(e & ADDR_MASK, 0x4000_0000 + i as u64 * 4096);
        assert_eq!(leaf_flags(e, 1), flags | PageFlags::PRESENT, "same rights, type and A/D bits in the 4 KiB layout");
    }
}

/// An `UnmapSlices` run to its end; the number of steps and what each step saw (a leaf list per step).
fn slices(t: &mut impl Tables, root: u64, virt: u64, len: u64, free_below: u32) -> (UnmapStats, Vec<Vec<Unmapped>>) {
    let mut c = UnmapSlices::new(virt, len).unwrap();
    c.check(t, root).unwrap();
    let (mut st, mut steps) = (UnmapStats::default(), Vec::new());
    while !c.done() {
        let mut seen = Vec::new();
        c.step(t, root, free_below, &mut |u| seen.push(u), &mut st);
        steps.push(seen);
    }
    c.finish(t, root, free_below, &mut st);
    (st, steps)
}

/// 4 KiB leaves in two regions, a 2 MiB leaf, a 1 GiB leaf after an empty stretch, 4 KiB leaves behind it.
fn mixed_layout() -> (Arena, u64, u64) {
    let (mut a, root) = arena();
    for i in 0..600 {
        map(&mut a, root, K + i * 0x1000, 0x1000, PageSize::Size4K, RW).unwrap();
    }
    map(&mut a, root, K + (4 << 20), 0x20_0000, PageSize::Size2M, RW).unwrap();
    map(&mut a, root, K + (2 << 30), 0x4000_0000, PageSize::Size1G, RW).unwrap();
    for i in 0..3 {
        map(&mut a, root, K + (3 << 30) + i * 0x1000, 0x1000, PageSize::Size4K, RW).unwrap();
    }
    (a, root, (4 << 30) + (1 << 21))
}

#[test]
fn slices_end_exactly_like_one_unmap() {
    let (mut whole, root, len) = mixed_layout();
    let (mut sliced, root2, _) = mixed_layout();
    assert_eq!(root, root2);
    let mut one = Vec::new();
    let st = unmap(&mut whole, root, K, len, 3, &mut |u| one.push(u)).unwrap();
    let (st2, steps) = slices(&mut sliced, root, K, len, 3);
    assert_eq!(st, st2);
    assert_eq!(one, steps.concat(), "the same leaves in the same order");
    assert_eq!(whole.freed.iter().collect::<std::collections::BTreeSet<_>>(), sliced.freed.iter().collect(), "the same tables freed");
    assert_eq!(whole.tables.keys().collect::<Vec<_>>(), sliced.tables.keys().collect::<Vec<_>>(), "the same tables left");
}

#[test]
fn a_slice_runs_to_the_end_of_a_gib_leaf_and_over_empty_stretches() {
    let (mut a, root) = arena();
    map(&mut a, root, K, 0x4000_0000, PageSize::Size1G, RW).unwrap();
    map(&mut a, root, K + (1 << 30), 0x1000, PageSize::Size4K, RW).unwrap();
    // The leaf; a 2 MiB step with the page, which takes table and PD along; the rest of that GiB; the empty GiB after it.
    let (st, steps) = slices(&mut a, root, K, 3 << 30, 3);
    assert_eq!(st, UnmapStats { leaves: 2, tables_freed: 2 });
    assert_eq!(steps[0].len(), 1);
    assert_eq!(steps[0][0].size, PageSize::Size1G);
    assert_eq!(steps[1].len(), 1);
    assert_eq!(steps.len(), 4);
}

#[test]
fn a_slice_clears_at_most_one_page_table_and_frees_a_pd_with_its_last_entry() {
    let (mut a, root) = arena();
    for r in 0..3u64 {
        for i in 0..512 {
            map(&mut a, root, K + (r << 21) + i * 0x1000, 0x1000, PageSize::Size4K, RW).unwrap();
        }
    }
    // From the middle of a table: the first step ends at the next 2 MiB boundary.
    let mut c = UnmapSlices::new(K + 0x10_0000, (3 << 21) - 0x10_0000).unwrap();
    c.check(&a, root).unwrap();
    let mut st = UnmapStats::default();
    let mut left = 3;
    while !c.done() {
        let mut seen = Vec::new();
        c.step(&mut a, root, 3, &mut |u| seen.push(u), &mut st);
        assert!(seen.len() <= 512);
        assert!(seen.windows(2).all(|w| w[0].virt >> 21 == w[1].virt >> 21), "one table per step");
        left -= 1;
        // The first table keeps its lower half, so it stays and the PD with it; the two after it go in their steps.
        assert_eq!(st.tables_freed, 2 - left);
    }
    assert_eq!(st.leaves, 3 * 512 - 256);
    assert!(translate(&a, root, K).is_some(), "below the range nothing changed");
    // The rest of the first table: now the PD empties in the step that clears its last entry.
    let (st, steps) = slices(&mut a, root, K, 0x10_0000, 3);
    assert_eq!(st, UnmapStats { leaves: 256, tables_freed: 2 });
    assert_eq!(steps.len(), 1);
}

#[test]
fn the_check_says_what_the_full_walk_says() {
    let (mut a, root, len) = mixed_layout();
    map(&mut a, root, K + (1 << 30), 0x60_0000, PageSize::Size2M, RW).unwrap();
    let mut x = 0x2545_F491_4F6C_DD1Du64;
    let mut next = move |m: u64| {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x % m
    };
    let (mut ok, mut refused) = (0, 0);
    for round in 0..4000 {
        // Page-granular near the leaves' edges, otherwise anywhere.
        let step = [0x1000, 1 << 21, 1 << 30][round % 3];
        let lo = next((len + (1 << 30)) / step) * step;
        let n = (next((len + (1 << 30)) / step) + 1) * step;
        let c = UnmapSlices::new(K + lo, n).unwrap();
        let full = check_partial(&a, root, 4, 0, (K + lo) & 0xFFFF_FFFF_FFFF, (K + lo + n) & 0xFFFF_FFFF_FFFF);
        assert_eq!(c.check(&a, root), full, "[{lo:#x}, +{n:#x})");
        if full.is_ok() { ok += 1 } else { refused += 1 }
    }
    assert!(ok > 100 && refused > 100, "{ok} accepted, {refused} refused");
}

#[test]
fn a_refused_range_changes_nothing_and_needs_no_step() {
    let (mut a, root) = arena();
    map(&mut a, root, K + (1 << 30), 0x4000_0000, PageSize::Size1G, RW).unwrap();
    let c = UnmapSlices::new(K, (1 << 30) + (1 << 21)).unwrap();
    assert_eq!(c.check(&a, root), Err(MapError::PartialHugePage));
    let c = UnmapSlices::new(K + (1 << 30) + (1 << 21), 1 << 30).unwrap();
    assert_eq!(c.check(&a, root), Err(MapError::PartialHugePage));
    assert!(translate(&a, root, K + (1 << 30)).is_some());
    assert!(matches!(UnmapSlices::new(K + 1, 0x1000), Err(MapError::NotAligned)));
    let mut none = UnmapSlices::new(K, 0).unwrap();
    assert!(none.done());
    assert_eq!(none.check(&a, root), Ok(()));
    let mut st = UnmapStats::default();
    none.step(&mut a, root, 3, &mut |_| panic!("an empty range has no leaf"), &mut st);
    none.finish(&mut a, root, 3, &mut st);
    assert_eq!(st, UnmapStats::default());
}

/// `kernel::unmap_kernel`'s walk on the arena, with the room `freed` has; returns the tables that came free per piece.
fn deferred_walk(a: &mut Arena, root: u64, virt: u64, len: u64, freed: &mut Vec<u64>, lost: &mut u64) -> (UnmapStats, usize) {
    let mut t = crate::paging::kernel::Deferred { d: a, freed, lost, range: (virt, virt + len), ends: false };
    let mut c = UnmapSlices::new(virt, len).unwrap();
    c.check(&t, root).unwrap();
    let mut st = UnmapStats::default();
    while !c.done() {
        c.step(&mut t, root, 3, &mut |_| {}, &mut st);
    }
    let before_ends = t.freed.len() + *t.lost as usize;
    t.ends = true;
    c.finish(&mut t, root, 3, &mut st);
    (st, before_ends)
}

#[test]
fn a_table_that_reaches_past_the_range_goes_only_at_the_very_end() {
    let (mut a, root) = arena();
    // [K + 4 KiB, K + 8 MiB - 4 KiB): the first and last PT and the PD reach past it, two PTs lie inside.
    let (virt, len) = (K + 0x1000, (8 << 20) - 0x2000);
    for r in [virt, K + (2 << 20), K + (4 << 20), K + (8 << 20) - 0x2000] {
        map(&mut a, root, r, 0x1000, PageSize::Size4K, RW).unwrap();
    }
    let mut freed = Vec::new();
    freed.reserve_exact(16);
    let mut lost = 0;
    let (st, before_ends) = deferred_walk(&mut a, root, virt, len, &mut freed, &mut lost);
    assert_eq!(before_ends, 2, "only the two tables inside before the last piece");
    assert_eq!(st, UnmapStats { leaves: 4, tables_freed: 5 });
    assert_eq!(freed.len(), 5);
    assert_eq!(lost, 0);
    assert!(a.freed.is_empty(), "collected, not freed: the shootdown comes first");
    let pdpt = a.read(root, index(K, 4)) & ADDR_MASK;
    assert_eq!(a.read(pdpt, index(K, 3)), 0, "the PD's entry is gone");
    // A neighbour with a page in the last PT: the table stays through all of it.
    let (mut a, root) = arena();
    map(&mut a, root, K + 0x1000, 0x1000, PageSize::Size4K, RW).unwrap();
    map(&mut a, root, K, 0x1000, PageSize::Size4K, RW).unwrap();
    let (mut freed, mut lost) = (Vec::new(), 0);
    freed.reserve_exact(16);
    let (st, _) = deferred_walk(&mut a, root, K + 0x1000, 0x1000, &mut freed, &mut lost);
    assert_eq!(st, UnmapStats { leaves: 1, tables_freed: 0 });
    assert!(translate(&a, root, K).is_some());
}

#[test]
fn what_does_not_fit_in_the_room_is_lost_one_at_a_time() {
    let build = || {
        let (mut a, root) = arena();
        for r in 0..3u64 {
            map(&mut a, root, K + (r << 21), 0x1000, PageSize::Size4K, RW).unwrap();
        }
        (a, root)
    };
    // Three PTs and a PD.
    let (mut a, root) = build();
    let mut freed = Vec::new();
    freed.reserve_exact(2);
    let room = freed.capacity() as u64;
    let mut lost = 0;
    let (st, _) = deferred_walk(&mut a, root, K, 3 << 21, &mut freed, &mut lost);
    assert_eq!(st.tables_freed, 4);
    assert_eq!((freed.len() as u64, lost), (room, 4 - room), "accumulated over the slices");
    // No room at all: counted, every one, and no allocation.
    let (mut a, root) = build();
    let (mut freed, mut lost) = (Vec::new(), 0);
    deferred_walk(&mut a, root, K, 3 << 21, &mut freed, &mut lost);
    assert_eq!((freed.capacity(), lost), (0, 4));
}

#[test]
fn room_for_emptied_tables_is_retried_smaller() {
    let mut v = Vec::new();
    crate::paging::kernel::reserve_room(&mut v, 2052);
    assert!(v.capacity() >= 2052);
    // More than any heap has: the first attempts fail for sure (past isize::MAX bytes), a smaller one gets through.
    let want = usize::MAX / 8;
    let mut v = Vec::new();
    crate::paging::kernel::reserve_room(&mut v, want);
    assert!(v.capacity() > 0 && v.capacity() < want, "capacity {}", v.capacity());
    let mut v = Vec::new();
    crate::paging::kernel::reserve_room(&mut v, 0);
    assert_eq!(v.capacity(), 0);
}

#[test]
fn an_empty_stretch_in_a_present_pd_is_one_step() {
    let (mut a, root) = arena();
    // PDEs 0 and 300 present, everything between and after them empty.
    map(&mut a, root, K, 0x1000, PageSize::Size4K, RW).unwrap();
    map(&mut a, root, K + (300 << 21), 0x1000, PageSize::Size4K, RW).unwrap();
    let (st, per_step) = slices(&mut a, root, K, 1 << 30, 3);
    assert_eq!(st, UnmapStats { leaves: 2, tables_freed: 3 });
    assert_eq!(per_step.len(), 4, "PDE 0, the stretch to 300, PDE 300, the stretch to the end");
    // From the middle of an empty stretch, and stopped by the end of the range.
    let (mut a, root) = arena();
    map(&mut a, root, K, 0x1000, PageSize::Size4K, RW).unwrap();
    assert_eq!(step_end(&a, root, K + (5 << 21) + 0x1000), K + (1 << 30));
    assert_eq!(step_end(&a, root, K + 0x1000), K + (1 << 21));
}

#[test]
fn a_range_inside_one_2m_is_one_step() {
    for (virt, len, one) in [
        (K, 0x1000, true),
        (K, 2 << 20, true),
        (K + (2 << 20) - 0x1000, 0x1000, true),
        (K + (2 << 20) - 0x1000, 0x2000, false),
        (K, (2 << 20) + 0x1000, false),
        (K, 0, true),
    ] {
        assert_eq!(UnmapSlices::new(virt, len).unwrap().one_step(), one, "{virt:#x} + {len:#x}");
    }
    // What `one_step` promises `unmap_kernel`: the first step takes the lot.
    let (mut a, root) = arena();
    map(&mut a, root, K, 0x1000, PageSize::Size4K, RW).unwrap();
    map(&mut a, root, K + (2 << 20) - 0x1000, 0x1000, PageSize::Size4K, RW).unwrap();
    let mut c = UnmapSlices::new(K, 2 << 20).unwrap();
    let mut st = UnmapStats::default();
    c.step(&mut a, root, 3, &mut |_| {}, &mut st);
    assert!(c.done());
    assert_eq!(st.leaves, 2);
}

/// A walk that takes its table frames from what `kernel::map_with_retries` hands it.
struct Fed<'a> {
    a: &'a core::cell::RefCell<Arena>,
    take: &'a mut dyn FnMut() -> Option<u64>,
}

impl Tables for Fed<'_> {
    fn read(&self, t: u64, i: usize) -> u64 {
        self.a.borrow().read(t, i)
    }
    fn write(&mut self, t: u64, i: usize, v: u64) {
        self.a.borrow_mut().write(t, i, v);
    }
    fn alloc_table(&mut self) -> Option<u64> {
        (self.take)()
    }
    fn free_table(&mut self, t: u64) {
        self.a.borrow_mut().free_table(t);
    }
}

#[test]
fn a_frame_another_walker_made_unneeded_comes_back_to_the_caller() {
    use crate::paging::kernel::map_with_retries;
    let (a, root) = arena();
    let a = core::cell::RefCell::new(a);
    let mut given = Vec::new();
    // The third table is the PT. While its frame is being fetched, outside the lock, a neighbour installs that PT.
    let mut source = || {
        let mut a = a.borrow_mut();
        if given.len() == 2 {
            map(&mut *a, root, K + 0x1000, 0x1000, PageSize::Size4K, RW).unwrap();
        }
        let f = a.alloc_table();
        given.extend(f);
        f
    };
    let (out, spare) = map_with_retries(&mut source, &mut |take| map(&mut Fed { a: &a, take }, root, K, 0x2000, PageSize::Size4K, RW));
    assert_eq!(out, Ok(()));
    assert_eq!(spare, given.last().copied(), "the unneeded frame is the last one given, and the caller gets it back");
    let a = a.into_inner();
    assert!(translate(&a, root, K).is_some() && translate(&a, root, K + 0x1000).is_some());
    let pdpt = a.read(root, index(K, 4)) & ADDR_MASK;
    let pd = a.read(pdpt, index(K, 3)) & ADDR_MASK;
    assert_ne!(a.read(pd, index(K, 2)) & ADDR_MASK, spare.unwrap(), "the spare is in no table");
    // Every pass found a frame: nothing left over. And a source that runs dry ends it with its answer.
    let (a, root) = arena();
    let a = core::cell::RefCell::new(a);
    let mut source = || a.borrow_mut().alloc_table();
    let (out, spare) = map_with_retries(&mut source, &mut |take| map(&mut Fed { a: &a, take }, root, K, 0x2000, PageSize::Size4K, RW));
    assert_eq!((out, spare), (Ok(()), None));
    let (a, root) = arena();
    let a = core::cell::RefCell::new(a);
    let (out, spare) = map_with_retries(&mut || None, &mut |take| map(&mut Fed { a: &a, take }, root, K, 0x2000, PageSize::Size4K, RW));
    assert_eq!((out, spare), (Err(MapError::NoMemory), None));
}

#[test]
fn a_waiter_gets_the_lock_between_the_slices_of_a_walk() {
    use crate::paging::kernel::{let_waiters_in, with_tables_locked};
    use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    static SLICES: AtomicU64 = AtomicU64::new(0);
    static STOP: AtomicBool = AtomicBool::new(false);
    // Counted in slices passed, not in time. Without `let_waiters_in` a wait here runs to tens of thousands of slices. The
    // best of a few runs, since the host may take the waiter's CPU away for a moment now and then.
    let mut best = u64::MAX;
    for _ in 0..9 {
        STOP.store(false, Ordering::Relaxed);
        SLICES.store(0, Ordering::Relaxed);
        let walker = std::thread::spawn(|| {
            while !STOP.load(Ordering::Relaxed) {
                with_tables_locked(|| {
                    SLICES.fetch_add(1, Ordering::Relaxed);
                    for _ in 0..50 {
                        core::hint::spin_loop();
                    }
                });
                let_waiters_in();
            }
        });
        while SLICES.load(Ordering::Relaxed) == 0 {
            core::hint::spin_loop();
        }
        let mut longest = 0;
        for _ in 0..2000 {
            let before = SLICES.load(Ordering::Relaxed);
            with_tables_locked(|| {});
            longest = longest.max(SLICES.load(Ordering::Relaxed) - before);
        }
        STOP.store(true, Ordering::Relaxed);
        walker.join().unwrap();
        best = best.min(longest);
    }
    assert!(best <= 64, "a waiter waited out {best} slices");
}
