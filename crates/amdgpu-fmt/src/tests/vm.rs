//! Builder cases with known layouts: levels, fragments, huge leaves, the invalid patterns, splits.

use std::vec::Vec;

use super::MemStore;
use crate::pte::{self, INVALID_PDE, INVALID_PTE, MapFlags, PDE_PTE, Target};
use crate::store::TableRead;
use crate::vm::{Backing, Level, Vm};
use crate::walk::{self, Walk};

const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;

fn leaf_at(store: &MemStore, vm: &Vm, va: u64) -> crate::walk::Leaf {
    match walk::walk(store, vm.root().phys, va).unwrap() {
        Walk::Page(l) => l,
        other => panic!("{va:#x}: {other:?}"),
    }
}

#[test]
fn new_vm_root_is_all_invalid_pdes() {
    let mut s = MemStore::new();
    let vm = Vm::new(&mut s, super::whole()).unwrap();
    assert!((0..512).all(|i| s.read(vm.root().phys, i) == Some(INVALID_PDE)));
    assert_eq!(vm.pd_addr(), vm.root().phys | 1);
    assert_eq!(walk::walk(&s, vm.root().phys, 0x1000).unwrap(), Walk::Hole { level: Level::Pdb2 });
}

#[test]
fn small_system_mapping_is_4k_ptes_frag0() {
    let mut s = MemStore::new();
    let mut vm = Vm::new(&mut s, super::whole()).unwrap();
    let pages = [0x10_0000, 0x7000, 0x3_0000_0000];
    vm.map(&mut s, 0x40_0000, 3 * 4096, Backing::System { pages: &pages, snooped: true }, MapFlags::RW).unwrap();
    assert_eq!(vm.table_count(), 4);
    for (k, &p) in pages.iter().enumerate() {
        let l = leaf_at(&s, &vm, 0x40_0000 + k as u64 * 4096 + 0x123);
        assert_eq!((l.phys, l.level, l.frag), (p + 0x123, Level::Ptb, 0));
        assert_eq!(l.target, Target::System { snooped: true });
    }
    // The rest of the PTB holds Linux's invalid PTE.
    let Walk::Hole { level } = walk::walk(&s, vm.root().phys, 0x40_3000).unwrap() else { panic!() };
    assert_eq!(level, Level::Ptb);
}

#[test]
fn aligned_2m_carveout_is_one_pdb0_leaf() {
    let mut s = MemStore::new();
    let mut vm = Vm::new(&mut s, super::whole()).unwrap();
    vm.map(&mut s, 0x20_0000, 2 * MIB, Backing::Local { phys: 0x6_6000_0000 }, MapFlags::RW).unwrap();
    assert_eq!(vm.table_count(), 3, "root, PDB1 table, PDB0 table; no PTB");
    let l = leaf_at(&s, &vm, 0x3F_FFFF);
    assert_eq!((l.phys, l.level, l.frag, l.target), (0x6_601F_FFFF, Level::Pdb0, 9, Target::Local));
}

#[test]
fn aligned_4m_gets_two_pdb0_leaves_with_frag_10() {
    let mut s = MemStore::new();
    let mut vm = Vm::new(&mut s, super::whole()).unwrap();
    vm.map(&mut s, 0x40_0000, 4 * MIB, Backing::Local { phys: 0x6_6040_0000 }, MapFlags::RW).unwrap();
    for va in [0x40_0000, 0x60_0000] {
        let l = leaf_at(&s, &vm, va);
        assert_eq!((l.level, l.frag), (Level::Pdb0, 10));
    }
    walk::check_fragment(&s, vm.root().phys, 0x7F_F000).unwrap();
}

#[test]
fn one_gib_is_a_pdb1_leaf() {
    let mut s = MemStore::new();
    let mut vm = Vm::new(&mut s, super::whole()).unwrap();
    vm.map(&mut s, 3 * GIB, GIB, Backing::Local { phys: 0x80_4000_0000 }, MapFlags::RW).unwrap();
    assert_eq!(vm.table_count(), 2);
    let l = leaf_at(&s, &vm, 3 * GIB + 0x1234_5678);
    assert_eq!((l.phys, l.level, l.frag), (0x80_4000_0000 + 0x1234_5678, Level::Pdb1, 18));
}

#[test]
fn physical_alignment_limits_the_fragment() {
    let mut s = MemStore::new();
    let mut vm = Vm::new(&mut s, super::whole()).unwrap();
    // VA 2 MiB aligned, PA only 64 KiB aligned: 16-page fragments in PTEs.
    vm.map(&mut s, 0x20_0000, 2 * MIB, Backing::Local { phys: 0x6_6001_0000 }, MapFlags::RW).unwrap();
    let l = leaf_at(&s, &vm, 0x20_0000);
    assert_eq!((l.level, l.frag), (Level::Ptb, 4));
    walk::check_fragment(&s, vm.root().phys, 0x2F_0000).unwrap();
}

#[test]
fn fragment_follows_linux_rule() {
    // Start page 0x10, 0x30 pages: blocks 0x10 (frag 4) then 0x20 (frag 5), as amdgpu_vm_pte_fragment.
    let mut s = MemStore::new();
    let mut vm = Vm::new(&mut s, super::whole()).unwrap();
    vm.map(&mut s, 0x10 * 4096, 0x30 * 4096, Backing::Local { phys: 0x10 * 4096 }, MapFlags::RW).unwrap();
    assert_eq!(leaf_at(&s, &vm, 0x1F * 4096).frag, 4);
    assert_eq!(leaf_at(&s, &vm, 0x20 * 4096).frag, 5);
    assert_eq!(leaf_at(&s, &vm, 0x3F * 4096).frag, 5);
}

#[test]
fn prt_range_walks_as_prt_on_its_level() {
    let mut s = MemStore::new();
    let mut vm = Vm::new(&mut s, super::whole()).unwrap();
    vm.map(&mut s, 0x20_0000, 2 * MIB, Backing::Prt, MapFlags::default()).unwrap();
    vm.map(&mut s, 0x40_0000, 4096, Backing::Prt, MapFlags::default()).unwrap();
    assert_eq!(walk::walk(&s, vm.root().phys, 0x30_0000).unwrap(), Walk::Prt { level: Level::Pdb0, frag: 9 });
    assert_eq!(walk::walk(&s, vm.root().phys, 0x40_0000).unwrap(), Walk::Prt { level: Level::Ptb, frag: 0 });
    assert_eq!(walk::walk(&s, vm.root().phys, 0x40_1000).unwrap(), Walk::Hole { level: Level::Ptb });
}

#[test]
fn upper_half_va_uses_the_top_of_the_index_space() {
    let mut s = MemStore::new();
    let mut vm = Vm::new(&mut s, super::whole()).unwrap();
    let va = 0xffff_8000_0000_0000;
    vm.map(&mut s, va, 4096, Backing::Local { phys: 0x5000 }, MapFlags::RW).unwrap();
    assert_eq!(vm.mappings().next().unwrap().start, 1 << 47);
    assert_eq!(leaf_at(&s, &vm, va).phys, 0x5000);
    assert!(vm.lookup(va).is_some());
    vm.unmap(&mut s, va).unwrap();
}

#[test]
fn unmap_writes_linux_invalid_values_and_defers_frees() {
    let mut s = MemStore::new();
    let mut vm = Vm::new(&mut s, super::whole()).unwrap();
    let seq0 = vm.tlb_seq();
    vm.map(&mut s, 0x1000, 4096, Backing::Local { phys: 0x9000 }, MapFlags::RW).unwrap();
    let pdb0 = vm.table_count();
    assert_eq!(pdb0, 4);
    // Find the PTB through the walker's view: the PDE chain.
    let mut t = vm.root().phys;
    for lvl in [Level::Pdb2, Level::Pdb1, Level::Pdb0] {
        let e = s.read(t, lvl.index(1)).unwrap();
        assert_eq!(e & 1, 1);
        assert_eq!(e & PDE_PTE, 0);
        t = e & pte::PDE_ADDR_MASK;
    }
    let ptb = t;
    let seq = vm.unmap(&mut s, 0x1000).unwrap();
    assert!(seq > seq0);
    assert_eq!(s.read(ptb, 1), Some(INVALID_PTE), "pending tables stay readable with the invalid PTE");
    assert_eq!(vm.table_count(), 1);
    assert_eq!(vm.pending_count(), 3);
    assert_eq!(s.read(vm.root().phys, 0), Some(INVALID_PDE));
    assert_eq!(vm.reclaim(&mut s, seq - 1), 0, "not flushed yet");
    assert_eq!(vm.reclaim(&mut s, seq), 3);
    assert_eq!(s.tables.len(), 1);
}

#[test]
fn clear_splits_without_touching_kept_blocks() {
    let mut s = MemStore::new();
    let mut vm = Vm::new(&mut s, super::whole()).unwrap();
    vm.map(&mut s, 0x40_0000, 4 * MIB, Backing::Local { phys: 0x6_6040_0000 }, MapFlags::RW).unwrap();
    s.logging = true;
    vm.clear(&mut s, 0x50_0000, 4096).unwrap();
    // The second 2 MiB block (VA 0x60_0000, PDB0 slot 3) went from frag 10 to frag 9 in one write, never
    // through an invalid entry.
    let slot3: Vec<u64> = s.log.iter().filter(|&&(_, i, v)| i == 3 && v & PDE_PTE != 0).map(|e| e.2).collect();
    assert_eq!(slot3.len(), 1, "{:x?}", s.log);
    assert_eq!(slot3[0], 0x6_6060_0000 | 0x61 | (9 << 7) | PDE_PTE);
    let kept: Vec<_> = vm.mappings().map(|m| (m.start, m.pages)).collect();
    assert_eq!(kept, [(0x40_0000, 256), (0x50_1000, 767)]);
    assert_eq!(walk::walk(&s, vm.root().phys, 0x50_0000).unwrap(), Walk::Hole { level: Level::Ptb });
    assert_eq!(leaf_at(&s, &vm, 0x50_1000).phys, 0x6_6050_1000);
    assert_eq!(leaf_at(&s, &vm, 0x60_0000).level, Level::Pdb0);
}

#[test]
fn remap_after_unmap_reuses_slots() {
    let mut s = MemStore::new();
    let mut vm = Vm::new(&mut s, super::whole()).unwrap();
    for round in 0..50u64 {
        let phys = 0x1_0000_0000 + round * 2 * MIB;
        vm.map(&mut s, 0x20_0000, 2 * MIB, Backing::Local { phys }, MapFlags::RW).unwrap();
        assert_eq!(leaf_at(&s, &vm, 0x20_0000).phys, phys);
        let seq = vm.unmap(&mut s, 0x20_0000).unwrap();
        vm.reclaim(&mut s, seq);
    }
    assert_eq!(s.tables.len(), 1);
}

#[test]
fn tables_in_system_memory_get_system_pdes() {
    let mut s = MemStore::at(0x2_0000_0000, Target::System { snooped: true });
    let mut vm = Vm::new(&mut s, super::whole()).unwrap();
    assert_eq!(vm.pd_addr() & 7, 7);
    vm.map(&mut s, 0, 4096, Backing::Local { phys: 0 }, MapFlags::RW).unwrap();
    let e = s.read(vm.root().phys, 0).unwrap();
    assert_eq!(e & 7, 7);
    assert_eq!(leaf_at(&s, &vm, 0).target, Target::Local);
}
