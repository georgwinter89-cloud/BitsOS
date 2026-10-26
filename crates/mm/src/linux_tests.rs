extern crate std;
use super::*;

const P: u64 = 4096;
const TOP: u64 = 0x7F00_0000_0000;
const AP: u64 = MAP_ANONYMOUS | MAP_PRIVATE;
const RW: u64 = PROT_READ | PROT_WRITE;

fn map(s: &mut Space, addr: u64, len: u64, prot: u64, flags: u64) -> Result<u64, Errno> {
    let p = s.plan_mmap(addr, len, prot, flags, 0)?;
    s.commit_mmap(&p, VmaKind::Anonymous)?;
    Ok(p.addr)
}

#[test]
fn mmap_argument_checks_in_linux_order() {
    let s = Space::new(TOP);
    assert_eq!(s.plan_mmap(0, P, RW, AP, 1), Err(Errno::Inval), "unaligned offset");
    assert_eq!(s.plan_mmap(0, P, RW, MAP_PRIVATE, 0), Err(Errno::NoDev), "file mapping");
    assert_eq!(s.plan_mmap(0, 0, RW, AP, 0), Err(Errno::Inval));
    assert_eq!(s.plan_mmap(0, u64::MAX - 10, RW, AP, 0), Err(Errno::NoMem), "PAGE_ALIGN overflows");
    assert_eq!(s.plan_mmap(0, TASK_SIZE, RW, AP, 0), Err(Errno::NoMem));
    assert_eq!(s.plan_mmap(0x1001, P, RW, AP | MAP_FIXED, 0), Err(Errno::Inval));
    assert_eq!(s.plan_mmap(TASK_SIZE - 0x800, P, RW, AP | MAP_FIXED, 0), Err(Errno::NoMem), "ENOMEM before EINVAL");
    assert_eq!(s.plan_mmap(0x1000, P, RW, AP | MAP_FIXED, 0), Err(Errno::Perm), "below mmap_min_addr");
    assert_eq!(s.plan_mmap(0x100_0000, P, RW, MAP_ANONYMOUS | MAP_FIXED, 0), Err(Errno::Inval), "no map type");
    assert_eq!(s.plan_mmap(0x100_0000, P, RW, MAP_ANONYMOUS | 3 | MAP_FIXED, 0), Err(Errno::Inval), "SHARED_VALIDATE on anon");
    assert_eq!(s.plan_mmap(0, P, RW, AP | MAP_HUGETLB, 0), Err(Errno::NoMem));
}

#[test]
fn hints_fixed_and_noreplace() {
    let mut s = Space::new(TOP);
    let a = map(&mut s, 0, 3 * P, RW, AP).unwrap();
    assert_eq!(a, TOP - 3 * P, "top-down below mmap_top");
    let b = map(&mut s, 0x1000_0000, 2 * P, RW, AP).unwrap();
    assert_eq!(b, 0x1000_0000, "a free hint is taken");
    let c = map(&mut s, 0x1000_0800, 2 * P, RW, AP).unwrap();
    assert_eq!(c, TOP - 5 * P, "a taken hint is not");
    assert_eq!(map(&mut s, 0xf000, P, RW, AP), Ok(MMAP_MIN_ADDR), "a low hint moves up to mmap_min_addr");
    assert_eq!(map(&mut s, 0x10, P, RW, AP), Ok(TOP - 6 * P), "rounded down to 0 it is no hint");
    assert_eq!(map(&mut s, 0x1000_0000, P, RW, AP | MAP_FIXED_NOREPLACE), Err(Errno::Exist));
    assert_eq!(map(&mut s, 0x1000_1000, 2 * P, PROT_READ, AP | MAP_FIXED), Ok(0x1000_1000));
    let v: std::vec::Vec<_> = s.tree.overlapping(0x1000_0000, 0x1000_3000).iter().map(|v| (v.start, v.end, v.prot)).collect();
    assert_eq!(v, [(0x1000_0000, 0x1000_1000, Prot::rw()), (0x1000_1000, 0x1000_3000, Prot::from_linux(1))]);
    assert_eq!(map(&mut s, 0, P, RW, AP | MAP_32BIT), Ok(0x4000_0000));
    let p = s.plan_mmap(0x2000_0000, P, RW, AP | MAP_FIXED, 0).unwrap();
    assert!(!p.replaces);
    assert!(s.plan_mmap(0, P, RW, AP | MAP_POPULATE, 0).unwrap().populate);
    assert!(!s.plan_mmap(0, P, RW, AP | MAP_POPULATE | MAP_NONBLOCK, 0).unwrap().populate);
}

#[test]
fn munmap_checks() {
    let s = Space::new(TOP);
    assert_eq!(s.plan_munmap(0x1001, P), Err(Errno::Inval));
    assert_eq!(s.plan_munmap(0x1000, 0), Err(Errno::Inval));
    assert_eq!(s.plan_munmap(TASK_SIZE - P, 2 * P), Err(Errno::Inval));
    assert_eq!(s.plan_munmap(0x1000, 1), Ok((0x1000, 0x2000)));
    assert_eq!(s.plan_munmap(TASK_SIZE - P, P), Ok((TASK_SIZE - P, TASK_SIZE)));
}

#[test]
fn mprotect_changes_up_to_the_first_hole() {
    let mut s = Space::new(TOP);
    let base = 0x5000_0000;
    map(&mut s, base, 4 * P, RW, AP | MAP_FIXED).unwrap();
    map(&mut s, base + 5 * P, 2 * P, RW, AP | MAP_FIXED).unwrap();
    assert_eq!(s.plan_mprotect(base + 1, P, 1), Err(Errno::Inval));
    assert_eq!(s.plan_mprotect(base, 0, 1), Ok(None), "len 0 succeeds");
    assert_eq!(s.plan_mprotect(base, P, 0x10), Err(Errno::Inval), "unknown bit");
    assert_eq!(s.plan_mprotect(base, P, PROT_GROWSDOWN | PROT_GROWSUP), Err(Errno::Inval));
    assert_eq!(s.plan_mprotect(base - P, 2 * P, 1), Err(Errno::NoMem), "hole at the start: nothing changes");
    assert_eq!(s.plan_mprotect(0x9000_0000, P, 1), Err(Errno::NoMem));
    assert_eq!(s.plan_mprotect(base, P, PROT_GROWSDOWN | 1), Err(Errno::Inval), "no growsdown mapping");
    assert_eq!(s.plan_mprotect(base, P, PROT_GROWSUP | 1), Err(Errno::Inval), "no growsup mapping");
    assert_eq!(s.plan_mprotect(base - P, P, PROT_GROWSUP | 1), Err(Errno::NoMem), "ENOMEM first");
    let p = s.plan_mprotect(base + 2 * P, 5 * P, PROT_READ | PROT_SEM).unwrap().unwrap();
    assert_eq!((p.applied_end, p.error), (base + 4 * P, Some(Errno::NoMem)));
    s.commit_mprotect(&p).unwrap();
    assert_eq!(s.tree.find(base + 3 * P).unwrap().prot, Prot::from_linux(1));
    assert_eq!(s.tree.find(base + 5 * P).unwrap().prot, Prot::rw(), "after the hole: unchanged");
    assert_eq!(s.tree.find(base + P).unwrap().prot, Prot::rw());
}

#[test]
fn brk_grows_shrinks_and_keeps_a_page_before_the_next_mapping() {
    let mut s = Space::new(TOP);
    assert_eq!(s.plan_brk(0x1234), BrkPlan::Keep(0), "no heap yet");
    s.init_brk(0x60_0010);
    assert_eq!(s.brk, 0x60_1000);
    assert_eq!(s.plan_brk(0), BrkPlan::Keep(0x60_1000));
    assert_eq!(s.plan_brk(0x60_0000), BrkPlan::Keep(0x60_1000), "below the start");
    let p = s.plan_brk(0x60_3001);
    assert_eq!(p, BrkPlan::Move { brk: 0x60_3001, unmap: None, map: Some((0x60_1000, 0x60_4000)) });
    assert_eq!(s.commit_brk(&p), Ok(0x60_3001));
    let p = s.plan_brk(0x60_3fff);
    assert_eq!(p, BrkPlan::Move { brk: 0x60_3fff, unmap: None, map: None }, "same page");
    s.commit_brk(&p).unwrap();
    let p = s.plan_brk(0x60_2000);
    assert_eq!(p, BrkPlan::Move { brk: 0x60_2000, unmap: Some((0x60_2000, 0x60_4000)), map: None });
    s.commit_brk(&p).unwrap();
    assert_eq!(s.tree.find(0x60_1000).unwrap().end, 0x60_2000);
    map(&mut s, 0x60_6000, P, RW, AP | MAP_FIXED).unwrap();
    assert_eq!(s.plan_brk(0x60_5000), BrkPlan::Move { brk: 0x60_5000, unmap: None, map: Some((0x60_2000, 0x60_5000)) });
    assert_eq!(s.plan_brk(0x60_5001), BrkPlan::Keep(0x60_2000), "would leave no free page");
}

/// FS-5: mprotect(PROT_WRITE) changes the VMAs before the first one that may not be written (a MAP_SHARED
/// mapping of a read-only fd, Linux VM_MAYWRITE) and answers EACCES there; PROT_READ|PROT_EXEC is fine.
#[test]
fn mprotect_stops_at_a_read_only_shared_file() {
    use crate::vma::{FileRef, Vma, VmaKind};
    struct NoPages;
    impl vfs::mmap::MapBacking for NoPages {
        fn get_page(&self, _pgoff: u64, _write: bool) -> Result<u64, vfs::pagecache::PageFault> {
            Err(vfs::pagecache::PageFault::Bus)
        }
        fn put_page(&self, _phys: u64) -> bool {
            false
        }
        fn shared(&self) -> bool {
            false
        }
        fn track_dirty(&self) -> bool {
            false
        }
        fn sync(&self, _first: u64, _last: u64) -> Result<(), i64> {
            Ok(())
        }
    }
    let b: alloc::sync::Arc<dyn vfs::mmap::MapBacking> = alloc::sync::Arc::new(NoPages);
    let mut s = Space::new(crate::MMAP_TOP);
    let ro = Prot { read: true, write: false, exec: false };
    s.tree.insert(Vma::anon(0x10_0000, 0x10_1000, ro, VmaKind::Anonymous)).unwrap();
    s.tree.insert(Vma { start: 0x10_1000, end: 0x10_2000, prot: ro, kind: VmaKind::File, file: Some(FileRef::file(b, 0, false, false)), flags: Default::default() }).unwrap();
    let p = s.plan_mprotect(0x10_0000, 0x2000, PROT_READ | PROT_WRITE).unwrap().unwrap();
    assert_eq!((p.applied_end, p.error), (0x10_1000, Some(Errno::Acces)));
    let p = s.plan_mprotect(0x10_1000, 0x1000, PROT_READ | PROT_WRITE).unwrap().unwrap();
    assert_eq!((p.applied_end, p.error), (0x10_1000, Some(Errno::Acces)), "nothing changes when the first VMA refuses");
    let p = s.plan_mprotect(0x10_0000, 0x2000, PROT_READ | PROT_EXEC).unwrap().unwrap();
    assert_eq!((p.applied_end, p.error), (0x10_2000, None));
}

/// MM-5b: `Space::committed` follows every change exactly (the sum of VM_ACCOUNT VMAs), as Linux's VM_ACCOUNT rules.
#[test]
fn commit_accounting_follows_the_tree() {
    let mut s = Space::new(TOP);
    let sum = |s: &Space| s.tree.iter().filter(|v| v.flags.account).map(|v| v.pages()).sum::<u64>();
    let a = map(&mut s, 0, 8 * P, RW, AP).unwrap();
    assert_eq!(s.committed, 8);
    map(&mut s, 0, 4 * P, PROT_READ, AP).unwrap();
    map(&mut s, 0, 4 * P, 0, AP | MAP_NORESERVE).unwrap();
    assert_eq!(s.committed, 8, "read-only and PROT_NONE are free");
    let nr = map(&mut s, 0, 2 * P, RW, AP | MAP_NORESERVE).unwrap();
    assert_eq!(s.committed, 10, "MAP_NORESERVE is ignored (mode 2)");
    // MAP_FIXED over half of the first mapping: the replaced part goes, the new part is charged.
    let p = s.plan_mmap(a + 2 * P, 4 * P, PROT_READ, AP | MAP_FIXED, 0).unwrap();
    assert!(p.replaces && !p.account);
    s.commit_mmap(&p, VmaKind::Anonymous).unwrap();
    assert_eq!(s.committed, 6);
    // mprotect: a read-only VMA becoming writable is charged once; back to read-only keeps it.
    let ro = map(&mut s, 0, 3 * P, PROT_READ, AP).unwrap();
    let pp = s.plan_mprotect(ro, 3 * P, RW).unwrap().unwrap();
    let v = s.tree.find(ro).unwrap().clone();
    assert!(Space::protect_charges(&v, Prot::rw()));
    s.commit_mprotect(&pp).unwrap();
    assert_eq!(s.committed, 9);
    let pp = s.plan_mprotect(ro, 3 * P, PROT_READ).unwrap().unwrap();
    s.commit_mprotect(&pp).unwrap();
    assert_eq!(s.committed, 9, "kept after RW -> R");
    let pp = s.plan_mprotect(ro, 3 * P, RW).unwrap().unwrap();
    s.commit_mprotect(&pp).unwrap();
    assert_eq!(s.committed, 9, "an accounted VMA is not charged again");
    // A split munmap gives back exactly its part.
    let (st, en) = s.plan_munmap(ro + P, P).unwrap();
    s.commit_munmap(st, en).unwrap();
    assert_eq!(s.committed, 8);
    s.commit_munmap(nr, nr + 2 * P).unwrap();
    assert_eq!(s.committed, 6);
    // brk: growth charged, shrink given back.
    s.init_brk(0x5555_0000_0000);
    let b = s.plan_brk(0x5555_0000_0000 + 5 * P);
    s.commit_brk(&b).unwrap();
    assert_eq!(s.committed, 11);
    let b = s.plan_brk(0x5555_0000_0000 + P);
    s.commit_brk(&b).unwrap();
    assert_eq!(s.committed, 7);
    assert_eq!(s.committed, sum(&s), "the books match the tree");
    assert_eq!(s.accounted(0, TASK_SIZE), s.committed);
}

#[test]
fn madvise_checks_in_linux_order() {
    assert_eq!(plan_madvise(0x1000, P, 99), Err(Errno::Inval), "invalid advice first");
    assert_eq!(plan_madvise(0x1000, P, 5), Err(Errno::Inval), "5..7 are not advice");
    assert_eq!(plan_madvise(0x1001, P, madv::DONTNEED), Err(Errno::Inval), "unaligned");
    assert_eq!(plan_madvise(0x1000, u64::MAX - 10, madv::DONTNEED), Err(Errno::Inval), "length rounds to 0");
    assert_eq!(plan_madvise(u64::MAX - 0xfff, 2 * P, madv::DONTNEED), Err(Errno::Inval), "end wraps");
    assert_eq!(plan_madvise(0x1000, 0, madv::HWPOISON), Ok(None), "length 0 before the capability check");
    assert_eq!(plan_madvise(0x1000, 1, madv::HWPOISON), Err(Errno::Perm));
    assert_eq!(plan_madvise(0x1000, 1, madv::DONTNEED), Ok(Some((0x1000, 0x2000))));
    for b in [0, 1, 2, 3, 4, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25] {
        assert!(madv::valid(b), "{b}");
    }
    assert!(!madv::valid(26) && !madv::valid(102), "MADV_GUARD_* (7.x) are not offered");
}
