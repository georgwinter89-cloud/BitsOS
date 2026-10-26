//! ktests of MM-ctr: the kept page and table numbers (`aspace/counters.rs`, VmSize/VmPeak in the VMA tree) agree
//! with a walk of the page tables after every kind of change (fills, fork, copy-on-write, munmap, DONTNEED,
//! mremap's move, file and shared mappings), the high-water marks keep their peaks, and smaps' Pss, Shared and
//! Private numbers follow the sharing; Referenced and Dirty come from the CPU's accessed and dirty bits.
//!
//! The Safe tests judge only their own spaces (kept against walked, the space's own underflows): on the Mini-PC
//! other programs run meanwhile, so system-wide numbers (user frames, the mismatch and underflow metrics) are
//! judged only by the QemuOnly tests.

use crate::aspace::counters::{MISMATCHES, UNDERFLOWS};
use crate::linux::{madv, MAP_ANONYMOUS, MAP_FIXED, MAP_PRIVATE, MAP_SHARED, MREMAP_MAYMOVE, PROT_READ};
use crate::tlb_ktest::{ap_activate, ap_write_in, aps, on, PROBE_ROOT, WRITE_VAL};
use crate::{AddressSpace, Fill, MemStats, PageUsage, PSS_SHIFT};
use alloc::format;
use alloc::string::String;
use core::sync::atomic::Ordering::SeqCst;
use hal::paging::probe;
use ktest::{kassert, ktest, TestResult};

const P: u64 = 4096;
const RW: u64 = 3;
const AP: u64 = MAP_ANONYMOUS | MAP_PRIVATE;

fn e<T: core::fmt::Debug>(x: T) -> String {
    format!("{x:?}")
}

fn lazy() -> Result<AddressSpace, String> {
    AddressSpace::with_fill(Fill::OnFault).map_err(|x| format!("address space: {x:?}"))
}

fn user() -> u64 {
    mm_core::frames().map_or(0, |f| f.stats().used_by_kind[mm_core::Kind::User as usize])
}

/// The kept numbers equal a fresh count by walking; returns them.
fn check(a: &AddressSpace, what: &str) -> Result<MemStats, String> {
    let (k, w) = (a.mem_stats(), a.mem_stats_walked());
    kassert!(k == w, "{what}: kept {k:?}\n walked {w:?}");
    kassert!(a.resident_pages() == k.resident && a.rss_pages() == k.rss, "{what}: lock-free readers differ");
    kassert!(a.counter_underflows() == 0, "{what}: a count of this space went below zero");
    Ok(k)
}

/// The smaps numbers of the VMA containing `va`.
fn usage_at(a: &AddressSpace, va: u64) -> Result<PageUsage, String> {
    let all = a.page_usage().map_err(e)?;
    all.into_iter().find(|(v, _)| v.start <= va && va < v.end).map(|x| x.1).ok_or_else(|| format!("no VMA at {va:#x}"))
}

/// Anonymous memory through fill, fork, copy-on-write, munmap, DONTNEED and mremap's move; an eager space.
fn anon() -> TestResult {
    {
        let a = lazy()?;
        let m = a.sys_mmap(0, 16 * P, RW, AP, 0).map_err(e)?;
        check(&a, "empty")?;
        for i in 0..10 {
            a.write_u64(m + i * P, i).map_err(e)?;
        }
        let s = check(&a, "filled")?;
        kassert!(s.anon == 10 && s.rss == 10 && s.file == 0 && s.shmem == 0 && s.hiwater_rss == 10, "after 10 stores {s:?}");
        kassert!(s.page_tables >= 3 && s.size == 16 && s.peak == 16, "tables and size {s:?}");
        let b = a.fork().map_err(e)?;
        let sb = check(&b, "child")?;
        kassert!(sb.anon == 10 && sb.hiwater_rss == 10 && sb.peak == sb.size, "child {sb:?}");
        check(&a, "parent after fork")?;
        // A copy-on-write store in the child: the child copies, the parent's page is its own again.
        b.write_u64(m + 9 * P, 0x99).map_err(e)?;
        kassert!(check(&b, "child copy")?.anon == 10 && check(&a, "parent after child copy")?.anon == 10);
        kassert!(usage_at(&a, m)?.private_dirty == P, "page 9 private in the parent");
        b.write_u64(m + 9 * P, 9).map_err(e)?;
        kassert!(check(&b, "child second store")?.anon == 10, "a store to its own copy changes nothing");
        // Shared by two spaces: Pss is half, all shared and dirty (program frames count dirty).
        let t = a.page_usage_total();
        kassert!(t.resident == 10 * P && t.anonymous == 10 * P && t.shared_dirty == 9 * P && t.private_dirty == P, "shared {t:?}");
        kassert!(t.pss == (P << PSS_SHIFT) + 9 * ((P << PSS_SHIFT) / 2) && t.pss_anon == t.pss && t.pss_dirty == t.pss, "Pss 1 + 9 halves {t:?}");
        // A copy-on-write store in the parent: still 10 pages each, two private in the parent.
        a.write_u64(m, 0x77).map_err(e)?;
        kassert!(check(&a, "parent copy")?.anon == 10 && check(&b, "child after parent copy")?.anon == 10);
        let t = a.page_usage_total();
        kassert!(t.private_dirty == 2 * P && t.shared_dirty == 8 * P, "two private {t:?}");
        kassert!(t.pss == 2 * (P << PSS_SHIFT) + 8 * ((P << PSS_SHIFT) / 2), "Pss of 2 own + 8 halves {t:?}");
        check(&b, "child before its drop")?;
        drop(b);
        let t = usage_at(&a, m)?;
        kassert!(t.private_dirty == 10 * P && t.pss == (10 * P) << PSS_SHIFT, "all private after the child {t:?}");
        // munmap and DONTNEED: RSS down, the high-water marks stay.
        a.sys_munmap(m, 4 * P).map_err(e)?;
        let s = check(&a, "munmap")?;
        kassert!(s.anon == 6 && s.hiwater_rss == 10 && s.size == 12 && s.peak == 16, "after munmap {s:?}");
        a.sys_madvise(m + 4 * P, 2 * P, madv::DONTNEED).map_err(e)?;
        let s = check(&a, "dontneed")?;
        kassert!(s.anon == 4 && s.hiwater_rss == 10, "after DONTNEED {s:?}");
        a.sys_munmap(m + 4 * P, 12 * P).map_err(e)?;
        let s = check(&a, "all gone")?;
        kassert!(s.rss == 0 && s.size == 0 && s.peak == 16 && s.hiwater_rss == 10, "empty {s:?}");
    }
    {
        // mremap's move (the VMA cannot grow in place): pages and tables move, VmPeak does not count the move.
        let c = lazy()?;
        let n = c.sys_mmap(0, 9 * P, RW, AP, 0).map_err(e)?;
        c.sys_mprotect(n + 8 * P, P, PROT_READ).map_err(e)?;
        for i in 0..8 {
            c.write_u64(n + i * P, i).map_err(e)?;
        }
        let to = c.sys_mremap(n, 8 * P, 16 * P, MREMAP_MAYMOVE, 0).map_err(e)?;
        kassert!(to != n, "the VMA grew in place instead of moving");
        let s = check(&c, "moved")?;
        kassert!(s.anon == 8 && s.size == 17 && s.peak == 17, "after the move {s:?}");
        kassert!(c.read_u64(to + 7 * P).map_err(e)? == 7, "content moved");
    }
    {
        let x = AddressSpace::new().map_err(e)?;
        x.sys_mmap(0, 3 * P, RW, AP, 0).map_err(e)?;
        kassert!(check(&x, "eager")?.anon == 3, "an eager space fills at mmap");
    }
    Ok(())
}
ktest!("mm.ctr.anon", Safe, anon);

/// File pages, private copies of them, shared anonymous memory; the map count of a page-cache frame gives Pss.
fn files() -> TestResult {
    use vfs::abi::{O_CREAT, O_RDWR};
    const PATH: &[u8] = b"/tmp/.mm-ctr-file";
    let ns = vfs::root_ns();
    if ns.lookup("/tmp").is_err() {
        return ktest::skip("no /tmp in this image");
    }
    let f = ns.open_flags(PATH, O_RDWR | O_CREAT, 0o600).map_err(e)?;
    let r = (|| -> TestResult {
        f.file.write_at(0, &[7u8; 2 * P as usize]).map_err(e)?;
        let a = lazy()?;
        let s = a.sys_mmap(0, 4 * P, RW, MAP_SHARED | MAP_ANONYMOUS, 0).map_err(e)?;
        a.write_u64(s, 1).map_err(e)?;
        a.write_u64(s + P, 2).map_err(e)?;
        let st = check(&a, "shared anon")?;
        kassert!(st.shmem == 2 && st.anon == 0 && st.rss == 2, "shmem {st:?}");
        let fp = a.sys_mmap_file(0, 2 * P, RW, MAP_PRIVATE, 0, &mut |len| vfs::mmap::check(&f, RW, MAP_PRIVATE, 0, len)).map_err(e)?;
        a.read_u64(fp).map_err(e)?;
        kassert!(check(&a, "private read")?.file == 1, "a read maps the file's page");
        a.write_u64(fp + P, 9).map_err(e)?;
        a.write_u64(fp, 9).map_err(e)?;
        let st = check(&a, "private copies")?;
        kassert!(st.file == 0 && st.anon == 2 && st.shmem == 2, "copies are the program's {st:?}");
        let fs = a.sys_mmap_file(0, 2 * P, RW, MAP_SHARED, 0, &mut |len| vfs::mmap::check(&f, RW, MAP_SHARED, 0, len)).map_err(e)?;
        a.read_u64(fs).map_err(e)?;
        let st = check(&a, "shared file")?;
        kassert!(st.file == 1 && st.rss == 5, "shared file page {st:?}");
        let one = usage_at(&a, fs)?;
        kassert!(one.resident == P && one.pss == P << PSS_SHIFT && one.private_clean + one.private_dirty == P, "one mapper {one:?}");
        let b = a.fork().map_err(e)?;
        let sb = check(&b, "child")?;
        kassert!(sb.anon == 2 && sb.file == 0 && sb.shmem == 0, "the child refaults file and shared pages {sb:?}");
        b.read_u64(fs).map_err(e)?;
        b.read_u64(s).map_err(e)?;
        let sb = check(&b, "child reads")?;
        kassert!(sb.file == 1 && sb.shmem == 1, "child {sb:?}");
        let two = usage_at(&a, fs)?;
        kassert!(two.pss == (P << PSS_SHIFT) / 2 && two.shared_clean + two.shared_dirty == P && two.pss_file == two.pss, "two mappers {two:?}");
        let sh = usage_at(&a, s)?;
        kassert!(sh.resident == 2 * P && sh.pss == (P << PSS_SHIFT) + (P << PSS_SHIFT) / 2 && sh.pss_shmem == sh.pss && sh.anonymous == 0, "shmem Pss {sh:?}");
        check(&b, "child before its drop")?;
        drop(b);
        kassert!(usage_at(&a, fs)?.pss == P << PSS_SHIFT, "the child's map count went with it");
        a.sys_munmap(fs, 2 * P).map_err(e)?;
        a.sys_madvise(s, 4 * P, madv::DONTNEED).map_err(e)?;
        let st = check(&a, "unmapped")?;
        kassert!(st.file == 0 && st.shmem == 0 && st.anon == 2 && st.hiwater_rss == 5, "{st:?}");
        Ok(())
    })();
    drop(f);
    let _ = ns.remove("/tmp/.mm-ctr-file");
    r
}
ktest!("mm.ctr.files", Safe, files);

/// Referenced and Dirty are the CPU's accessed and dirty bits: a page filled by the kernel is neither, a store
/// through another CPU's TLB makes it both, and mprotect keeps them (Linux does too). Shared anonymous memory,
/// whose dirty state is not implied as for a program's own frames.
fn referenced_dirty() -> TestResult {
    let aps = aps();
    let Some(&c) = aps.first() else { return ktest::skip("one CPU") };
    let a = lazy()?;
    let s = a.sys_mmap(0, P, RW, MAP_SHARED | MAP_ANONYMOUS, 0).map_err(e)?;
    a.read_u64(s).map_err(e)?;
    let u = usage_at(&a, s)?;
    kassert!(u.resident == P && u.referenced == 0 && u.private_clean == P && u.pss_dirty == 0, "filled by the kernel {u:?}");
    {
        let _s = probe::user_probes().map_err(e)?;
        PROBE_ROOT.store(a.root(), SeqCst);
        kassert!(on(c, ap_activate, a.root())? == a.root(), "CPU {c} did not load the root");
        WRITE_VAL.store(0x55, SeqCst);
        kassert!(on(c, ap_write_in, s)? == 0, "CPU {c} could not store");
    }
    let u = usage_at(&a, s)?;
    kassert!(u.referenced == P && u.private_dirty == P && u.pss_dirty == u.pss, "after a store {u:?}");
    a.sys_mprotect(s, P, PROT_READ).map_err(e)?;
    let u = usage_at(&a, s)?;
    kassert!(u.referenced == P && u.private_dirty == P, "mprotect dropped the bits {u:?}");
    check(&a, "after the store")?;
    Ok(())
}
ktest!("mm.ctr.referenced_dirty", Safe, referenced_dirty);

/// Fork's other ends: DONTNEED in a child on pages it shares with its parent, then a fork refused half-way (the
/// test hook fails the child's 6th page). The parent's kept numbers, its walk and its smaps sharing are what they
/// were before the refused fork (the half-made child's shares went back).
fn fork_paths() -> TestResult {
    const N: u64 = 16;
    let a = lazy()?;
    let m = a.sys_mmap(0, N * P, RW, AP, 0).map_err(e)?;
    for i in 0..N {
        a.write_u64(m + i * P, i).map_err(e)?;
    }
    let b = a.fork().map_err(e)?;
    b.sys_madvise(m + 4 * P, 4 * P, madv::DONTNEED).map_err(e)?;
    let sb = check(&b, "child after DONTNEED")?;
    kassert!(sb.anon == N - 4 && sb.hiwater_rss == N, "child {sb:?}");
    check(&a, "parent after the child's DONTNEED")?;
    let t = usage_at(&a, m)?;
    kassert!(t.private_dirty == 4 * P && t.shared_dirty == (N - 4) * P, "4 pages the parent's own again {t:?}");
    let (s0, t0) = (check(&a, "before the refused fork")?, a.page_usage_total());
    a.test_fail_fork_after(Some(5));
    let r = a.fork();
    a.test_fail_fork_after(None);
    kassert!(r.is_err(), "the injected failure did not refuse the fork");
    drop(r);
    let (s1, t1) = (check(&a, "after the refused fork")?, a.page_usage_total());
    kassert!(s1 == s0, "the parent's numbers changed\n before {s0:?}\n after {s1:?}");
    kassert!(t1 == t0, "the parent's sharing changed\n before {t0:?}\n after {t1:?}");
    check(&b, "child before its drop")?;
    drop(b);
    let t = a.page_usage_total();
    kassert!(t.private_dirty == N * P && t.pss == (N * P) << PSS_SHIFT, "all own after the child {t:?}");
    Ok(())
}
ktest!("mm.ctr.fork_paths", Safe, fork_paths);

/// The other ways pages and VMAs come and go: brk up and down, MAP_FIXED over resident pages, mprotect splitting
/// and merging a resident VMA, a kernel copy_to_user across several pages.
fn more_paths() -> TestResult {
    let a = lazy()?;
    // brk: a heap of 8 pages, all stored, then shrunk to 2.
    const HEAP: u64 = 0x4000_0000;
    a.init_brk(HEAP);
    kassert!(a.sys_brk(HEAP + 8 * P) == HEAP + 8 * P, "brk did not grow");
    for i in 0..8 {
        a.write_u64(HEAP + i * P, i).map_err(e)?;
    }
    let s = check(&a, "brk grown")?;
    kassert!(s.anon == 8 && s.size == 8, "brk grown {s:?}");
    kassert!(a.sys_brk(HEAP + 2 * P) == HEAP + 2 * P, "brk did not shrink");
    let s = check(&a, "brk shrunk")?;
    kassert!(s.anon == 2 && s.size == 2 && s.peak == 8 && s.hiwater_rss == 8, "brk shrunk {s:?}");
    // MAP_FIXED over 3 resident pages: they go, the VMAs cover the same pages.
    let m = a.sys_mmap(0, 8 * P, RW, AP, 0).map_err(e)?;
    for i in 0..8 {
        a.write_u64(m + i * P, i).map_err(e)?;
    }
    check(&a, "filled")?;
    kassert!(a.sys_mmap(m + 2 * P, 3 * P, RW, AP | MAP_FIXED, 0).map_err(e)? == m + 2 * P, "MAP_FIXED moved");
    let s = check(&a, "MAP_FIXED")?;
    kassert!(s.anon == 2 + 5 && s.size == 10, "brk 2 + 5 left {s:?}");
    kassert!(a.read_u64(m + 2 * P).map_err(e)? == 0, "the new mapping is zero");
    let s = check(&a, "read back")?;
    kassert!(s.anon == 8, "brk 2 + 5 old + 1 read {s:?}");
    // mprotect splits the resident VMA into three and merges it back: no page count changes.
    let v0 = s.vmas;
    a.sys_mprotect(m + 5 * P, 2 * P, PROT_READ).map_err(e)?;
    let s = check(&a, "split")?;
    kassert!(s.vmas >= v0 + 1 && s.anon == 8 && s.size == 10, "split {s:?}");
    a.sys_mprotect(m + 5 * P, 2 * P, RW).map_err(e)?;
    let s = check(&a, "merged")?;
    kassert!(s.anon == 8 && s.size == 10 && s.vmas <= v0 + 2, "merged {s:?}");
    kassert!(usage_at(&a, m + 6 * P)?.resident >= 2 * P, "smaps sees the pages after the merge");
    // copy_to_user across four pages of a fresh mapping fills them all.
    let c = a.sys_mmap(0, 6 * P, RW, AP, 0).map_err(e)?;
    a.copy_to_user(c + P - 8, &[0x5a; 2 * P as usize + 16]).map_err(e)?;
    let s = check(&a, "copy_to_user")?;
    kassert!(s.anon == 8 + 4 && s.size == 16, "copy_to_user {s:?}");
    a.sys_munmap(c, 6 * P).map_err(e)?;
    a.sys_munmap(m, 8 * P).map_err(e)?;
    let s = check(&a, "unmapped")?;
    kassert!(s.anon == 2 && s.size == 2 && s.hiwater_rss == 12, "unmapped {s:?}");
    Ok(())
}
ktest!("mm.ctr.more_paths", Safe, more_paths);

/// QEMU only (nothing else allocates there meanwhile): the Safe scenarios give back every user frame, and no
/// space ending meanwhile (the children, the refused fork's half-made child) had counts that differ from its
/// tables or went below zero.
fn frames_back() -> TestResult {
    let (u0, f0, m0) = (UNDERFLOWS.get(), user(), MISMATCHES.get());
    anon()?;
    fork_paths()?;
    more_paths()?;
    kassert!(UNDERFLOWS.get() == u0, "a counter went below zero");
    kassert!(MISMATCHES.get() == m0, "an ending space's counters differed from its tables");
    kassert!(user() == f0, "{} bytes of user frames not back", user().saturating_sub(f0));
    Ok(())
}
ktest!("mm.ctr.frames_back", QemuOnly, frames_back);

/// QEMU only: since boot, no address space's kept counts went below zero or differed from its tables at its end.
/// Named to run after the other `mm.` tests (name order), so it judges every path they and the tests before them
/// in the same boot used (static and dynamic programs, fork, exec, mremap, files).
fn clean_since_boot() -> TestResult {
    kassert!(UNDERFLOWS.get() == 0, "{} counter underflows since boot (event as.counter_underflow)", UNDERFLOWS.get());
    kassert!(MISMATCHES.get() == 0, "{} counter mismatches since boot (event as.counter_mismatch)", MISMATCHES.get());
    Ok(())
}
ktest!("mm.zz_counters_clean", QemuOnly, clean_since_boot);
