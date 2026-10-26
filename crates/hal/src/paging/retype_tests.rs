//! Host tests of the direct-map retype on a frame arena.

extern crate std;
use super::super::walk::{map, translate, PageSize};
use super::*;
use std::collections::BTreeMap;

#[derive(Default)]
struct Arena {
    tables: BTreeMap<u64, [u64; 512]>,
    next: u64,
    budget: Option<usize>,
}

impl Tables for Arena {
    fn read(&self, table: u64, i: usize) -> u64 {
        self.tables[&table][i]
    }
    fn write(&mut self, table: u64, i: usize, v: u64) {
        self.tables.get_mut(&table).unwrap()[i] = v;
    }
    fn alloc_table(&mut self) -> Option<u64> {
        if let Some(n) = self.budget.as_mut() {
            *n = n.checked_sub(1)?;
        }
        self.next += 0x1000;
        self.tables.insert(self.next, [0; 512]);
        Some(self.next)
    }
    fn free_table(&mut self, table: u64) {
        self.tables.remove(&table);
    }
}

const D: u64 = 0xFFFF_8880_0000_0000;
const RW: u64 = PageFlags::WRITABLE | PageFlags::NX | PageFlags::GLOBAL;

/// A direct map of 0..2 GiB with one 1 GiB leaf and 512 2 MiB leaves.
fn direct() -> (Arena, u64) {
    let mut a = Arena { next: 0x1_0000_0000, ..Default::default() };
    let root = a.alloc_table().unwrap();
    map(&mut a, root, D, 0, PageSize::Size1G, RW).unwrap();
    for i in 0..512u64 {
        map(&mut a, root, D + (1 << 30) + i * (2 << 20), (1 << 30) + i * (2 << 20), PageSize::Size2M, RW).unwrap();
    }
    (a, root)
}

#[test]
fn split_keeps_every_translation_and_right() {
    let (mut a, root) = direct();
    let mut st = RetypeStats::default();
    // 3 pages inside the 1 GiB leaf, crossing a 2 MiB line.
    let va = D + (6 << 20) - 0x1000;
    split_range(&mut a, root, va, 0x3000, &mut st).unwrap();
    assert_eq!((st.split_1g, st.split_2m), (1, 2), "one 1G split, then the two 2M leaves the range touches");
    for probe in [0u64, 0x1234_5000, (6 << 20) - 0x1000, (6 << 20) + 0x1000, (1 << 30) - 0x1000] {
        let l = translate(&a, root, D + probe).unwrap();
        assert_eq!(l.phys, probe, "same frame at {probe:#x}");
        assert!(l.flags.contains(RW), "same rights at {probe:#x}");
        assert_eq!(l.flags.cache(), Some(CacheType::WriteBack));
    }
    assert_eq!(translate(&a, root, va).unwrap().size, PageSize::Size4K);
    assert_eq!(translate(&a, root, D + (8 << 20)).unwrap().size, PageSize::Size2M, "untouched 2M leaves stay");
    // Splitting again changes nothing.
    let mut again = RetypeStats::default();
    split_range(&mut a, root, va, 0x3000, &mut again).unwrap();
    assert_eq!(again, RetypeStats::default());
}

#[test]
fn retype_changes_only_the_range_and_goes_back() {
    let (mut a, root) = direct();
    let va = D + (1 << 30) + (4 << 20);
    let mut st = RetypeStats::default();
    split_range(&mut a, root, va, 0x2000, &mut st).unwrap();
    assert_eq!((st.split_1g, st.split_2m), (0, 1));
    for (cache, want) in [(CacheType::WriteCombining, PageFlags::PWT), (CacheType::Uncached, PageFlags::PCD | PageFlags::PWT)] {
        assert_eq!(set_cache_range(&mut a, root, va, 0x2000, cache.bits()), Ok(2));
        for p in [va, va + 0x1000] {
            let l = translate(&a, root, p).unwrap();
            assert_eq!(l.flags.cache(), Some(cache));
            assert_eq!(l.flags.0 & (PageFlags::PWT | PageFlags::PCD | PageFlags::PAT_4K), want);
            assert!(l.flags.contains(RW));
        }
        assert_eq!(translate(&a, root, va + 0x2000).unwrap().flags.cache(), Some(CacheType::WriteBack), "next page untouched");
        assert_eq!(translate(&a, root, va - 0x1000).unwrap().flags.cache(), Some(CacheType::WriteBack), "page before untouched");
    }
    set_cache_range(&mut a, root, va, 0x2000, CacheType::WriteBack.bits()).unwrap();
    assert_eq!(translate(&a, root, va).unwrap().flags.cache(), Some(CacheType::WriteBack));
}

#[test]
fn refusals_change_nothing() {
    let (mut a, root) = direct();
    let mut st = RetypeStats::default();
    // A range that runs past the mapped 2 GiB: refused, and the cache bits stay.
    let va = D + (2 << 30) - 0x1000;
    assert_eq!(split_range(&mut a, root, va, 0x2000, &mut st), Err(MapError::NotMapped));
    assert_eq!(set_cache_range(&mut a, root, va, 0x2000, CacheType::Uncached.bits()), Err(MapError::NotMapped));
    assert_eq!(translate(&a, root, va).unwrap().flags.cache(), Some(CacheType::WriteBack));
    // A large leaf is never retyped as a whole.
    assert_eq!(set_cache_range(&mut a, root, D, 0x1000, CacheType::Uncached.bits()), Err(MapError::NotMapped));
    assert_eq!(translate(&a, root, D + 0x5000).unwrap().flags.cache(), Some(CacheType::WriteBack));
    // No frame for the new table: refused, the mapping is unchanged.
    a.budget = Some(0);
    assert_eq!(split_range(&mut a, root, D + 0x1000, 0x1000, &mut st), Err(MapError::NoMemory));
    let l = translate(&a, root, D + 0x1000).unwrap();
    assert_eq!((l.phys, l.size), (0x1000, PageSize::Size1G));
}

#[test]
fn pat_bit_of_a_large_leaf_survives_the_split() {
    let mut a = Arena { next: 0x1_0000_0000, ..Default::default() };
    let root = a.alloc_table().unwrap();
    map(&mut a, root, D, 0x4000_0000, PageSize::Size1G, RW | PageFlags::PAT_4K | PageFlags::PWT).unwrap();
    let mut st = RetypeStats::default();
    split_range(&mut a, root, D + 0x20_3000, 0x1000, &mut st).unwrap();
    for p in [D + 0x20_3000, D + 0x40_0000, D + 0x20_0000] {
        let l = translate(&a, root, p).unwrap();
        assert_eq!(l.flags.0 & (PageFlags::PAT_4K | PageFlags::PWT), PageFlags::PAT_4K | PageFlags::PWT, "at {p:#x}");
        assert_eq!(l.phys, 0x4000_0000 + (p - D));
    }
}
