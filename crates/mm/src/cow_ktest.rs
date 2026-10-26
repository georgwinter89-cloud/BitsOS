//! ktests of MM-5b's fork: copy-on-write of private memory with share counts, shared anonymous memory and file
//! mappings across fork, MADV_DONTFORK/WIPEONFORK, and the TLB under fork on every CPU (the gate's `-smp 16` run
//! has 15 APs holding the parent's writable translations while the BSP forks).

use crate::aspace::fault::{COW_COPIES, COW_REUSED};
use crate::linux::{madv, MAP_ANONYMOUS, MAP_PRIVATE, MAP_SHARED};
use crate::tlb_ktest::{ap_activate, ap_write_in, aps, on, FAULT, PF_P, PF_W, PROBE_ROOT, WRITE_VAL};
use crate::{share, AddressSpace, Fill};
use alloc::format;
use alloc::string::String;
use core::sync::atomic::Ordering::SeqCst;
use hal::paging::{self, probe, PageFlags};
use ktest::{kassert, ktest, TestResult};

const P: u64 = 4096;
const RW: u64 = 3;
const AP: u64 = MAP_ANONYMOUS | MAP_PRIVATE;
const PF_U: u64 = 4;

fn e<T: core::fmt::Debug>(x: T) -> String {
    format!("{x:?}")
}

fn lazy() -> Result<AddressSpace, String> {
    AddressSpace::with_fill(Fill::OnFault).map_err(|x| format!("address space: {x:?}"))
}

fn user() -> u64 {
    mm_core::frames().map_or(0, |f| f.stats().used_by_kind[mm_core::Kind::User as usize])
}

fn leaf(a: &AddressSpace, va: u64) -> Result<(u64, PageFlags), String> {
    paging::translate(a.root(), va).ok_or_else(|| format!("{va:#x} not present"))
}

/// Parent and child share every private page read-only after fork; the first write copies (or, for the last
/// holder, takes the frame back writable); counts, commit and frames add up and all come back.
fn fork_cow() -> TestResult {
    let before = user();
    {
        let a = lazy()?;
        let m = a.sys_mmap(0, 8 * P, RW, AP, 0).map_err(e)?;
        for i in 0..8 {
            a.write_u64(m + i * P, 0x100 + i).map_err(e)?;
        }
        let held = user();
        kassert!(held - before == 8 * P, "8 pages filled ({} bytes)", held - before);
        let b = a.fork().map_err(e)?;
        kassert!(user() == held, "fork copied {} bytes", user() - held);
        kassert!(a.committed_pages() == 8 && b.committed_pages() == 8, "commit {} / {}", a.committed_pages(), b.committed_pages());
        for i in 0..8 {
            let ((pa, fa), (pb, fb)) = (leaf(&a, m + i * P)?, leaf(&b, m + i * P)?);
            kassert!(pa == pb && share::others(pa) == 1, "page {i}: frames {pa:#x}/{pb:#x}, others {}", share::others(pa));
            kassert!(!fa.contains(PageFlags::WRITABLE) && !fb.contains(PageFlags::WRITABLE), "page {i} writable after fork");
            kassert!(b.read_u64(m + i * P).map_err(e)? == 0x100 + i, "child page {i}");
        }
        // The parent writes page 0: its own copy; the child keeps the old frame, now its alone.
        let old0 = leaf(&a, m)?.0;
        let copies = COW_COPIES.get();
        a.write_u64(m, 0xAAAA).map_err(e)?;
        kassert!(COW_COPIES.get() == copies + 1, "one copy");
        kassert!(leaf(&a, m)?.0 != old0 && leaf(&b, m)?.0 == old0 && share::exclusive(old0), "the parent got a copy");
        kassert!(b.read_u64(m).map_err(e)? == 0x100 && a.read_u64(m).map_err(e)? == 0xAAAA, "contents apart");
        // The child is the last holder of page 0: a write fault takes it back writable, no copy.
        let reused = COW_REUSED.get();
        b.handle_user_fault(m, PF_U | PF_W | PF_P).map_err(e)?;
        kassert!(COW_REUSED.get() == reused + 1 && COW_COPIES.get() == copies + 1, "taken back, not copied");
        let (p0, f0) = leaf(&b, m)?;
        kassert!(p0 == old0 && f0.contains(PageFlags::WRITABLE), "the child's page 0 is writable in place");
        // A real write fault in the child copies page 1; the parent keeps its value.
        b.handle_user_fault(m + P, PF_U | PF_W | PF_P).map_err(e)?;
        b.write_u64(m + P, 0xBBBB).map_err(e)?;
        kassert!(a.read_u64(m + P).map_err(e)? == 0x101 && b.read_u64(m + P).map_err(e)? == 0xBBBB);
        // A grandchild: three holders of page 2.
        let p2 = leaf(&a, m + 2 * P)?.0;
        let c = b.fork().map_err(e)?;
        kassert!(share::others(p2) == 2, "three holders: others {}", share::others(p2));
        kassert!(c.read_u64(m).map_err(e)? == 0x100 && c.read_u64(m + P).map_err(e)? == 0xBBBB, "the grandchild sees the child's state");
        drop(c);
        kassert!(share::others(p2) == 1, "the grandchild gave its share back");
        drop(b);
        for i in 0..8 {
            kassert!(share::exclusive(leaf(&a, m + i * P)?.0), "page {i} still counted shared after the child went");
        }
        kassert!(user() - before == 8 * P, "the parent's 8 pages left, {} bytes held", user() - before);
        // Exclusive again: a write takes each page back without a copy.
        let copies = COW_COPIES.get();
        for i in 1..8 {
            a.write_u64(m + i * P, i).map_err(e)?;
        }
        kassert!(COW_COPIES.get() == copies, "no copy for pages only the parent holds");
    }
    kassert!(user() == before, "{} bytes of user frames not back", user().saturating_sub(before));
    Ok(())
}
ktest!("mm.cow.fork", Safe, fork_cow);

/// Shared anonymous memory stays one object across fork (pages first touched after the fork too); private file
/// copies are shared copy-on-write and never reach the file; shared file writes reach the file from both sides.
fn fork_shared() -> TestResult {
    use vfs::abi::{O_CREAT, O_RDWR};
    const PATH: &[u8] = b"/tmp/.mm-cow-file";
    let ns = vfs::root_ns();
    if ns.lookup("/tmp").is_err() || ns.lookup("/dev/shm").is_err() {
        return ktest::skip("no /tmp or /dev/shm in this image");
    }
    let before = user();
    let f = ns.open_flags(PATH, O_RDWR | O_CREAT, 0o600).map_err(e)?;
    let r = (|| -> TestResult {
        f.file.write_at(0, &0x1111u64.to_le_bytes()).map_err(e)?;
        let a = lazy()?;
        let s = a.sys_mmap(0, 4 * P, RW, MAP_SHARED | MAP_ANONYMOUS, 0).map_err(e)?;
        a.write_u64(s, 1).map_err(e)?;
        kassert!(a.committed_pages() == 0, "shared memory is charged as its tmpfs object, not as private memory");
        let fp = a.sys_mmap_file(0, P, RW, MAP_PRIVATE, 0, &mut |len| vfs::mmap::check(&f, RW, MAP_PRIVATE, 0, len)).map_err(e)?;
        a.write_u64(fp, 0x2222).map_err(e)?;
        let fs = a.sys_mmap_file(0, P, RW, MAP_SHARED, 0, &mut |len| vfs::mmap::check(&f, RW, MAP_SHARED, 0, len)).map_err(e)?;
        let b = a.fork().map_err(e)?;
        kassert!(b.read_u64(s).map_err(e)? == 1, "the child sees the shared page");
        b.write_u64(s, 2).map_err(e)?;
        kassert!(a.read_u64(s).map_err(e)? == 2, "the parent sees the child's store");
        b.write_u64(s + 3 * P, 3).map_err(e)?;
        kassert!(a.read_u64(s + 3 * P).map_err(e)? == 3, "a page first touched after the fork is shared too");
        a.sys_madvise(s, P, madv::DONTNEED).map_err(e)?;
        kassert!(a.read_u64(s).map_err(e)? == 2, "DONTNEED keeps shared data");
        kassert!(b.read_u64(fp).map_err(e)? == 0x2222, "the child sees the parent's private copy");
        b.write_u64(fp, 0x3333).map_err(e)?;
        kassert!(a.read_u64(fp).map_err(e)? == 0x2222, "the child's write copied");
        kassert!(b.read_u64(fs).map_err(e)? == 0x1111, "the shared file mapping shows the file");
        b.write_u64(fs, 0x4444).map_err(e)?;
        kassert!(a.read_u64(fs).map_err(e)? == 0x4444, "a shared file store is seen by the parent");
        let mut buf = [0u8; 8];
        f.file.read_at(0, &mut buf).map_err(e)?;
        kassert!(u64::from_le_bytes(buf) == 0x4444, "and by the file");
        drop(b);
        drop(a);
        Ok(())
    })();
    drop(f);
    let _ = ns.remove("/tmp/.mm-cow-file");
    r?;
    kassert!(user() == before, "{} bytes of user frames not back", user().saturating_sub(before));
    Ok(())
}
ktest!("mm.cow.shared_and_files", Safe, fork_shared);

/// MADV_DONTFORK keeps a range out of the child (and out of its commit), MADV_WIPEONFORK gives it empty;
/// MADV_DOFORK/KEEPONFORK undo them.
fn dontfork() -> TestResult {
    let a = lazy()?;
    let m1 = a.sys_mmap(0, 2 * P, RW, AP, 0).map_err(e)?;
    let m2 = a.sys_mmap(0, 2 * P, RW, AP, 0).map_err(e)?;
    a.write_u64(m1, 0x11).map_err(e)?;
    a.write_u64(m2, 0x22).map_err(e)?;
    a.sys_madvise(m1, 2 * P, madv::DONTFORK).map_err(e)?;
    a.sys_madvise(m2, 2 * P, madv::WIPEONFORK).map_err(e)?;
    let b = a.fork().map_err(e)?;
    kassert!(b.vmas().iter().all(|v| v.end <= m1 || v.start >= m1 + 2 * P), "a DONTFORK range reached the child");
    kassert!(b.read_u64(m2).map_err(e)? == 0, "a WIPEONFORK range is empty in the child");
    kassert!(a.read_u64(m2).map_err(e)? == 0x22, "and untouched in the parent");
    kassert!(b.committed_pages() == a.committed_pages() - 2, "commit: child {} parent {}", b.committed_pages(), a.committed_pages());
    drop(b);
    a.sys_madvise(m1, 2 * P, madv::DOFORK).map_err(e)?;
    a.sys_madvise(m2, 2 * P, madv::KEEPONFORK).map_err(e)?;
    let c = a.fork().map_err(e)?;
    kassert!(c.read_u64(m1).map_err(e)? == 0x11 && c.read_u64(m2).map_err(e)? == 0x22, "inherited again");
    Ok(())
}
ktest!("mm.cow.dontfork", Safe, dontfork);

/// A refused fork leaves the parent as it was: refused up front by the space's commit limit (nothing touched), and
/// refused half-way (the test hook fails the 6th page): the child's shares, tables and charge all go back, no
/// frame leaks, and the parent's pages (made read-only for the copy-on-write) are taken back writable on the
/// next store, same frames, same content. (The system-wide commit is checked in `mm.commit.system_limit`.)
fn fork_refused() -> TestResult {
    const N: u64 = 16;
    let a = lazy()?;
    let m = a.sys_mmap(0, N * P, RW, AP, 0).map_err(e)?;
    for i in 0..N {
        a.write_u64(m + i * P, 0x500 + i).map_err(e)?;
    }
    let frames: alloc::vec::Vec<u64> = (0..N).map(|i| leaf(&a, m + i * P).map(|x| x.0)).collect::<Result<_, _>>()?;
    let (c0, u0) = (a.committed_pages(), user());
    let intact = |what: &str| -> TestResult {
        kassert!(a.committed_pages() == c0, "{what}: commit {} != {c0}", a.committed_pages());
        kassert!(user() == u0, "{what}: {} bytes of user frames leaked", user().saturating_sub(u0));
        for (i, &f) in frames.iter().enumerate() {
            kassert!(share::others(f) == 0, "{what}: page {i} still shared ({})", share::others(f));
            kassert!(leaf(&a, m + i as u64 * P)?.0 == f, "{what}: page {i} moved");
        }
        Ok(())
    };
    // Up front: the space's own limit is below what the child would be charged.
    a.set_commit_limit(Some(N / 2));
    kassert!(a.fork().is_err(), "fork past the space's limit");
    a.set_commit_limit(None);
    intact("limit")?;
    kassert!(frames.iter().enumerate().all(|(i, _)| leaf(&a, m + i as u64 * P).is_ok_and(|x| x.1.contains(PageFlags::WRITABLE))),
        "a fork refused up front must not touch the parent's pages");
    // Half-way: five pages shared with the child, then the child's sixth map fails.
    a.test_fail_fork_after(Some(5));
    let r = a.fork();
    a.test_fail_fork_after(None);
    kassert!(r.is_err(), "the injected failure did not refuse the fork");
    drop(r);
    intact("half-way")?;
    for i in 0..N {
        a.write_u64(m + i * P, 0x900 + i).map_err(e)?;
        kassert!(a.read_u64(m + i * P).map_err(e)? == 0x900 + i, "page {i} after the store");
    }
    // Same frames (`intact`), writable again: taken back, not copied.
    intact("after the stores")?;
    kassert!(frames.iter().enumerate().all(|(i, _)| leaf(&a, m + i as u64 * P).is_ok_and(|x| x.1.contains(PageFlags::WRITABLE))),
        "a page stayed read-only after the store");
    // And a fork works again.
    let b = a.fork().map_err(e)?;
    kassert!(b.read_u64(m).map_err(e)? == 0x900, "child after the refused forks");
    Ok(())
}
ktest!("mm.cow.fork_refused", Safe, fork_refused);

/// The TLB under fork: every AP has the parent's page loaded writable (it just wrote it); after the fork each
/// AP's store must fault (present + write), or it would land in the frame the child now shares. 20 rounds.
fn smp_fork() -> TestResult {
    let aps = aps();
    if aps.is_empty() {
        return ktest::skip("one CPU");
    }
    let before = user();
    let leaked = crate::aspace::AS_LEAKED.get();
    {
        let a = lazy()?;
        let m = a.sys_mmap(0, 4 * P, RW, AP, 0).map_err(e)?;
        let _s = probe::user_probes().map_err(e)?;
        PROBE_ROOT.store(a.root(), SeqCst);
        for round in 0..20u64 {
            // The parent's page 0 writable again (after the last round's child went, it is the last holder).
            a.write_u64(m, round).map_err(e)?;
            for &c in &aps {
                kassert!(on(c, ap_activate, a.root())? == a.root(), "CPU {c} did not load the root");
                WRITE_VAL.store(round, SeqCst);
                kassert!(on(c, ap_write_in, m)? == 0, "round {round}: CPU {c} could not write before the fork");
            }
            let b = a.fork().map_err(e)?;
            for &c in &aps {
                WRITE_VAL.store(0xDEAD_0000 | c as u64, SeqCst);
                let w = on(c, ap_write_in, m)?;
                kassert!(w & FAULT != 0 && w & (PF_P | PF_W) == PF_P | PF_W, "round {round}: CPU {c} wrote a page shared with the child: {w:#x}");
            }
            kassert!(b.read_u64(m).map_err(e)? == round, "round {round}: the child's page changed");
            // The parent's write fault copies (the child still holds the frame); the APs can write again.
            a.handle_user_fault(m, PF_U | PF_W | PF_P).map_err(e)?;
            let c0 = aps[0];
            WRITE_VAL.store(0xBEEF, SeqCst);
            kassert!(on(c0, ap_write_in, m)? == 0, "round {round}: CPU {c0} cannot write the parent's copy");
            kassert!(b.read_u64(m).map_err(e)? == round && a.read_u64(m).map_err(e)? == 0xBEEF, "round {round}: copies mixed up");
            drop(b);
        }
    }
    kassert!(crate::aspace::AS_LEAKED.get() == leaked, "an address space leaked");
    kassert!(user() == before, "{} bytes of user frames not back", user().saturating_sub(before));
    klog::event!(Info, "mm", "ktest.cow_smp", aps = aps.len(), rounds = 20; "stores after fork fault on every CPU");
    Ok(())
}
ktest!("mm.cow.smp_tlb", Safe, smp_fork);
