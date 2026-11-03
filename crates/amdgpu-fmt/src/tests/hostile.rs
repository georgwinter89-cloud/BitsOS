//! Hostile inputs are refused with Linux's errno and change nothing; corrupt tables never panic the walker.

use super::{MemStore, Rng};
use crate::errno;
use crate::pte::{MapFlags, Target};
use crate::store::{TableAddr, TableStore};
use crate::vm::{Backing, Vm, VmError};
use crate::walk::{self, WalkError};

fn local(p: u64) -> Backing<'static> {
    Backing::Local { phys: p }
}

#[test]
fn bad_ranges_and_addresses_are_refused() {
    let mut s = MemStore::new();
    let mut vm = Vm::new(&mut s, super::whole()).unwrap();
    let rw = MapFlags::RW;
    let cases: [(u64, u64, Backing, VmError); 9] = [
        (0x1001, 4096, local(0), VmError::BadVa(0x1001)),
        (0x0000_8000_0000_0000, 4096, local(0), VmError::BadVa(0x0000_8000_0000_0000)),
        (0xffff_7fff_ffff_f000, 4096, local(0), VmError::BadVa(0xffff_7fff_ffff_f000)),
        (0x1000, 0, local(0), VmError::BadLength(0)),
        (0x1000, 100, local(0), VmError::BadLength(100)),
        (0xffff_ffff_ffff_f000, 0x2000, local(0), VmError::BadVa(0xffff_ffff_ffff_f000)),
        (0x1000, 4096, local(0x1234), VmError::BadPhys(0x1234)),
        (0x1000, 4096, local(1 << 48), VmError::BadPhys(1 << 48)),
        (0x1000, 0x2000, local((1 << 48) - 0x1000), VmError::BadPhys((1 << 48) - 0x1000)),
    ];
    for (va, len, b, want) in cases {
        assert_eq!(vm.map(&mut s, va, len, b, rw), Err(want), "va {va:#x} len {len:#x}");
        assert_eq!(want.errno(), errno::EINVAL);
    }
    let pages = [0x1000u64, 0x2001];
    assert_eq!(
        vm.map(&mut s, 0, 0x1000, Backing::System { pages: &pages, snooped: true }, rw),
        Err(VmError::PageCount { expected: 1, got: 2 })
    );
    assert_eq!(
        vm.map(&mut s, 0, 0x2000, Backing::System { pages: &pages, snooped: true }, rw),
        Err(VmError::BadPhys(0x2001))
    );
    assert_eq!(vm.table_count(), 1);
    assert_eq!(vm.tlb_seq(), 0);
    assert_eq!(s.tables.len(), 1);
}

#[test]
fn overlap_and_holes() {
    let mut s = MemStore::new();
    let mut vm = Vm::new(&mut s, super::whole()).unwrap();
    vm.map(&mut s, 0x10_0000, 0x4000, local(0x8000_0000), MapFlags::RW).unwrap();
    let e = vm.map(&mut s, 0x10_3000, 0x1000, local(0), MapFlags::RW).unwrap_err();
    assert_eq!(e, VmError::Overlap(0x10_0000));
    assert_eq!(vm.map(&mut s, 0xF_F000, 0x2000, local(0), MapFlags::RW), Err(VmError::Overlap(0x10_0000)));
    let e = vm.unmap(&mut s, 0x10_1000).unwrap_err();
    assert_eq!((e, e.errno()), (VmError::NotMapped(0x10_1000), errno::ENOENT));
    assert_eq!(vm.unmap(&mut s, 0x20_0000), Err(VmError::NotMapped(0x20_0000)));
    // Clearing a hole is fine and changes nothing.
    let seq = vm.tlb_seq();
    assert_eq!(vm.clear(&mut s, 0x40_0000, 0x1000), Ok(seq));
}

#[test]
fn out_of_memory_changes_nothing() {
    let mut s = MemStore::new();
    let mut vm = Vm::new(&mut s, super::whole()).unwrap();
    s.fail_after = Some(2);
    let e = vm.map(&mut s, 0x1000, 0x1000, local(0), MapFlags::RW).unwrap_err();
    assert_eq!((e, e.errno()), (VmError::NoMemory, errno::ENOMEM));
    assert_eq!(s.tables.len(), 1);
    assert_eq!(vm.mappings().count(), 0);
    s.fail_after = Some(0);
    assert!(matches!(Vm::new(&mut s, super::whole()), Err(VmError::NoMemory)));
}

#[test]
fn hostile_store_addresses_are_refused() {
    for bad in [0x1234_5040u64, 0x1234_5008, 1 << 50] {
        let mut s = MemStore::new();
        let mut vm = Vm::new(&mut s, super::whole()).unwrap();
        s.bad_next = Some(bad);
        let e = vm.map(&mut s, 0x1000, 0x1000, local(0), MapFlags::RW).unwrap_err();
        assert_eq!(e, VmError::BadTable(bad));
        assert_eq!(e.errno(), errno::EIO);
        assert_eq!(s.tables.len(), 1, "bad table given back, nothing linked");
        s.bad_next = Some(bad);
        assert!(matches!(Vm::new(&mut s, super::whole()), Err(VmError::BadTable(_))));
    }
}

/// A store whose tables are random words: the walker returns errors or answers, never panics.
struct Garbage(u64);
impl crate::store::TableRead for Garbage {
    fn read(&self, table: u64, index: usize) -> Option<u64> {
        if table % 7 == 3 {
            return None;
        }
        let mut r = Rng::new(self.0 ^ table ^ index as u64);
        Some(r.next())
    }
}

#[test]
fn walker_survives_garbage() {
    let mut rng = Rng::new(99);
    let mut outcomes = [0usize; 3];
    for i in 0..100_000 {
        let g = Garbage(i);
        let va = if rng.chance(90) { rng.next() & 0x7FFF_FFFF_FFFF } else { rng.next() };
        match walk::walk(&g, rng.next() & 0xFFFF_FFFF_F000, va) {
            Ok(_) => outcomes[0] += 1,
            Err(WalkError::NonCanonical(_)) => outcomes[1] += 1,
            Err(_) => outcomes[2] += 1,
        }
        let _ = walk::check_fragment(&g, 0x1000, va & 0x7FFF_FFFF_F000);
    }
    assert!(outcomes.iter().all(|&n| n > 0), "{outcomes:?}");
}

#[test]
fn walker_reports_misplaced_and_unreadable() {
    let mut s = MemStore::new();
    let root = s.alloc().unwrap();
    // A 2 MiB leaf on PDB1? No: an unaligned huge leaf on the root and a PDE to nowhere.
    s.write(root.phys, 0, 0x20_1000 | 1 | crate::pte::PDE_PTE);
    assert!(matches!(walk::walk(&s, root.phys, 0), Err(WalkError::Misplaced { .. })));
    s.write(root.phys, 1, crate::pte::pde(TableAddr { phys: 0x7_0000_0000, target: Target::Local }).unwrap());
    let va = 1u64 << 39;
    assert!(matches!(walk::walk(&s, root.phys, va), Err(WalkError::Unreadable { table: 0x7_0000_0000, .. })));
}

#[test]
fn huge_unaligned_request_is_refused_by_the_table_budget() {
    // VA 0 against phys 0x1000 drops every block to frag 0: 2^35 PTEs in 2^26 PTBs. The budget refuses it
    // after visiting a few thousand entries instead of looping over all of them.
    let mut s = MemStore::new();
    s.capacity = Some(64);
    let mut vm = Vm::new(&mut s, super::whole()).unwrap();
    let e = vm.map(&mut s, 0, 1 << 47, local(0x1000), MapFlags::RW).unwrap_err();
    assert_eq!((e, e.errno()), (VmError::NoMemory, errno::ENOMEM));
    let e = vm.map(&mut s, 0, 1 << 39, Backing::Prt, MapFlags::RW);
    assert!(e.is_ok(), "aligned 512 GiB PRT needs one PDB1 table: {e:?}");
    assert_eq!((vm.mappings().count(), s.tables.len()), (1, 2));
    // What fits the budget still maps; one 2 MiB of frag-0 pages more than the budget does not.
    let mut vm2 = Vm::new(&mut s, super::whole()).unwrap();
    vm2.map(&mut s, 1 << 30, 4 << 20, local(0x1000), MapFlags::RW).unwrap();
    let left = s.available();
    assert_eq!(vm2.map(&mut s, 2 << 30, (left as u64) << 21, local(0x1000), MapFlags::RW), Err(VmError::NoMemory));
}

#[test]
fn local_memory_outside_the_carveout_is_refused() {
    let c = crate::addr::Carveout::new(0x80_0000_0000, 0x840 << 24, 512 << 20).unwrap();
    let mut s = MemStore::new();
    let mut vm = Vm::new(&mut s, c).unwrap();
    let base = c.phys_base();
    let end = base + c.size();
    vm.map(&mut s, 0x20_0000, 2 << 20, local(base), MapFlags::RW).unwrap();
    vm.map(&mut s, 0x40_0000, 4096, local(end - 4096), MapFlags::RW).unwrap();
    for (p, len) in [(base - 4096, 4096), (end, 4096), (end - 4096, 8192), (0, 4096), (0xFED0_0000, 4096)] {
        let e = vm.map(&mut s, 0x100_0000, len, local(p), MapFlags::RW).unwrap_err();
        assert_eq!((e, e.errno()), (VmError::BadPhys(p), errno::EINVAL), "{p:#x}+{len:#x}");
    }
    // System pages are the caller's DMA addresses, not bound to the carve-out.
    vm.map(&mut s, 0x100_0000, 4096, Backing::System { pages: &[0x1000], snooped: true }, MapFlags::RW).unwrap();
    use crate::gart::{Gart, GartError, GartImage, GartPage};
    let g = Gart::new(0x7FFF_0000_0000, 64 << 10, 0x1000, c).unwrap();
    let mut t = GartImage::new(&g);
    let lp = GartPage { target: Target::Local, writeable: true };
    g.map(&mut t, 0, &[base, end - 4096], lp).unwrap();
    assert_eq!(g.map(&mut t, 0x2000, &[base, end], lp), Err(GartError::BadPhys(end)));
    assert_eq!(t.0[2], 0x1000, "nothing written");
    let sys = GartPage { target: Target::System { snooped: true }, writeable: true };
    g.map(&mut t, 0x2000, &[end], sys).unwrap();
}

/// Tables whose addresses encode the VA prefix, every PTE an honest identity page with frag 31: a 2^31-page
/// fragment block that is really contiguous. Counts reads.
struct Frag31(core::cell::Cell<u64>);
impl crate::store::TableRead for Frag31 {
    fn read(&self, table: u64, index: usize) -> Option<u64> {
        self.0.set(self.0.get() + 1);
        let (depth, prefix) = (table >> 40, (table & ((1 << 40) - 1)) >> 12);
        let child = (prefix << 9) | index as u64;
        let t = |d: u64| TableAddr { phys: (d << 40) | (child << 12), target: Target::Local };
        match depth {
            0..=2 => crate::pte::pde(t(depth + 1)).ok(),
            _ => crate::pte::leaf(child << 12, 4096, Target::Local, MapFlags::RW, 31, false).ok(),
        }
    }
}

#[test]
fn fragment_check_is_bounded_for_any_frag() {
    let mem = Frag31(core::cell::Cell::new(0));
    assert_eq!(walk::check_fragment(&mem, 0, 0x1234_5000), Ok(()));
    assert!(mem.0.get() <= 4 * 8194, "{} reads", mem.0.get());
}

#[test]
fn user_va_range_like_the_va_ioctl() {
    use crate::addr::{VA_RESERVED_SIZE as R, VA_SIZE, user_range};
    assert_eq!(user_range(R, 4096, VA_SIZE), Some(R));
    assert_eq!(user_range(R - 4096, 4096, VA_SIZE), None, "bottom reserved");
    assert_eq!(user_range(0, 4096, VA_SIZE), None);
    assert_eq!(user_range(0x0000_8000_0000_0000, 4096, VA_SIZE), None, "hole");
    assert_eq!(user_range(0xffff_8000_0000_0000, 4096, VA_SIZE), Some(0x8000_0000_0000));
    assert_eq!(user_range(0xffff_ffff_ffdf_f000, 4096, VA_SIZE), Some(0xffff_ffdf_f000), "last user page");
    assert_eq!(user_range(0xffff_ffff_ffe0_0000, 4096, VA_SIZE), None, "top reserved");
    assert_eq!(user_range(0xffff_ffff_ffff_f000, u64::MAX, VA_SIZE), None, "overflow");
    assert_eq!(user_range(R, 4096, R), None, "VM smaller than the reserve");
}
