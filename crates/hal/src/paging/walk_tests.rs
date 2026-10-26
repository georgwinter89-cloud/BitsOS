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
