//! ktests of MM-5b's strict commit (R2, Linux overcommit mode 2): what is charged when, a process's own limit
//! with every refusal path (no SIGSEGV/SIGBUS afterwards: every promised page can be touched), the system limit
//! with tmpfs and shared anonymous memory (QemuOnly: it lowers the system's limit for a moment), V8/Chromium's
//! 70 GB reservation with 32 GB alignment that must not cost memory, and a replay of the Y-NUMBERS pattern.

use crate::linux::{madv, Errno, MAP_ANONYMOUS, MAP_FIXED, MAP_NORESERVE, MAP_PRIVATE, MAP_SHARED};
use crate::{commit, AddressSpace, Fill, Prot, VmaKind};
use alloc::format;
use alloc::string::String;
use hal::paging;
use ktest::{kassert, ktest, TestResult};

const P: u64 = 4096;
const R: u64 = 1;
const RW: u64 = 3;
const RX: u64 = 5;
const AP: u64 = MAP_ANONYMOUS | MAP_PRIVATE;
const GB: u64 = 1 << 30;

fn e<T: core::fmt::Debug>(x: T) -> String {
    format!("{x:?}")
}

fn lazy() -> Result<AddressSpace, String> {
    AddressSpace::with_fill(Fill::OnFault).map_err(|x| format!("address space: {x:?}"))
}

fn user() -> u64 {
    mm_core::frames().map_or(0, |f| f.stats().used_by_kind[mm_core::Kind::User as usize])
}

/// What each call charges (Linux `accountable_mapping`, mprotect, brk, mode 2's MAP_NORESERVE).
fn accounting() -> TestResult {
    let a = lazy()?;
    let c = || a.committed_pages();
    let m1 = a.sys_mmap(0, 16 * P, RW, AP, 0).map_err(e)?;
    kassert!(c() == 16, "private writable: charged ({})", c());
    let res = a.sys_mmap(0, GB, 0, AP | MAP_NORESERVE, 0).map_err(e)?;
    kassert!(c() == 16, "PROT_NONE reservation: free ({})", c());
    a.sys_mmap(0, 8 * P, RW, AP | MAP_NORESERVE, 0).map_err(e)?;
    kassert!(c() == 24, "MAP_NORESERVE is ignored for writable private memory ({})", c());
    let ro = a.sys_mmap(0, 4 * P, R, AP, 0).map_err(e)?;
    kassert!(c() == 24, "read-only: free ({})", c());
    a.sys_mprotect(ro, 4 * P, RW).map_err(e)?;
    kassert!(c() == 28, "mprotect to writable charges ({})", c());
    a.sys_mprotect(ro, 4 * P, R).map_err(e)?;
    kassert!(c() == 28, "back to read-only keeps the charge, as Linux ({})", c());
    a.sys_munmap(ro, 4 * P).map_err(e)?;
    kassert!(c() == 24, "munmap gives it back ({})", c());
    a.sys_mprotect(res + 64 * P, 4 * P, RW).map_err(e)?;
    kassert!(c() == 28, "committing part of a reservation ({})", c());
    a.init_brk(0x50_0000_0000);
    a.brk(0x50_0000_0000 + 3 * P);
    kassert!(c() == 31, "brk growth ({})", c());
    a.brk(0x50_0000_0000 + P);
    kassert!(c() == 29, "brk shrink ({})", c());
    if vfs::root_ns().lookup("/dev/shm").is_ok() {
        a.sys_mmap(0, 4 * P, RW, MAP_SHARED | MAP_ANONYMOUS, 0).map_err(e)?;
        kassert!(c() == 29, "shared anonymous memory is charged as its tmpfs object ({})", c());
    }
    a.sys_munmap(m1, 16 * P).map_err(e)?;
    kassert!(c() == 13, "munmap ({})", c());
    a.map_fixed(0x60_0000_0000, 2 * P, Prot::rw(), VmaKind::Stack).map_err(e)?;
    kassert!(c() == 15, "a writable fixed mapping (stack) ({})", c());
    let b = a.fork().map_err(e)?;
    kassert!(b.committed_pages() == 15, "the child is charged the same ({})", b.committed_pages());
    let s = a.mem_stats();
    kassert!(s.committed == 15 && s.size >= GB / P, "stats {s:?}");
    Ok(())
}
ktest!("mm.commit.accounting", Safe, accounting);

/// A process's own limit: every way to ask for more is refused with ENOMEM (brk keeps the old break, mprotect
/// changes the VMAs before the refused one, as Linux); nothing promised is lost: every charged page can still be
/// touched (no SIGSEGV/SIGBUS from an empty pool).
fn space_limit() -> TestResult {
    let a = lazy()?;
    let m = a.sys_mmap(0, 6 * P, RW, AP, 0).map_err(e)?;
    a.set_commit_limit(Some(16));
    kassert!(a.sys_mmap(0, 11 * P, RW, AP, 0) == Err(Errno::NoMem), "past the limit");
    kassert!(a.sys_mmap(0, 11 * P, RW, AP | MAP_NORESERVE, 0) == Err(Errno::NoMem), "MAP_NORESERVE does not help");
    let n = a.sys_mmap(0, 8 * P, RW, AP, 0).map_err(e)?;
    let res = a.sys_mmap(0, 100 * P, 0, AP, 0).map_err(e)?;
    kassert!(a.committed_pages() == 14, "{}", a.committed_pages());
    // Two VMAs that cannot merge (R and RX); the commit allows one of them.
    let v1 = a.sys_mmap(0x70_0000_0000, 2 * P, R, AP | MAP_FIXED, 0).map_err(e)?;
    a.sys_mmap(v1 + 2 * P, 2 * P, RX, AP | MAP_FIXED, 0).map_err(e)?;
    kassert!(a.sys_mprotect(v1, 4 * P, RW) == Err(Errno::NoMem), "the second VMA does not fit");
    let vm = a.vmas();
    let w = |at: u64| vm.iter().find(|v| v.start <= at && at < v.end).map(|v| v.prot.write);
    kassert!(w(v1) == Some(true) && w(v1 + 2 * P) == Some(false), "the first VMA changed, the second did not");
    kassert!(a.committed_pages() == 16, "{}", a.committed_pages());
    kassert!(a.sys_mprotect(res, P, RW) == Err(Errno::NoMem), "committing a reservation");
    a.init_brk(0x50_0000_0000);
    kassert!(a.brk(0x50_0000_0000 + P) == 0x50_0000_0000, "brk keeps the old break");
    kassert!(a.sys_mremap(m, 6 * P, 8 * P, 1, 0) == Err(Errno::NoMem), "mremap growth");
    if vfs::root_ns().lookup("/dev/shm").is_ok() {
        // Shared memory is charged to the system, not to the process (as tmpfs on Linux).
        a.sys_mmap(0, 4 * P, RW, MAP_SHARED | MAP_ANONYMOUS, 0).map_err(e)?;
    }
    // Everything promised works.
    for i in 0..6 {
        a.write_u64(m + i * P, i).map_err(e)?;
    }
    for i in 0..8 {
        a.write_u64(n + i * P, i).map_err(e)?;
    }
    a.write_u64(v1, 1).map_err(e)?;
    let b = a.fork().map_err(e)?;
    kassert!(b.commit_limit() == Some(16) && b.committed_pages() == 16, "the child inherits limit and charge");
    for i in 0..8 {
        b.write_u64(n + i * P, i + 100).map_err(e)?;
    }
    a.set_commit_limit(None);
    kassert!(a.sys_mmap(0, 11 * P, RW, AP, 0).is_ok(), "without the limit");
    Ok(())
}
ktest!("mm.commit.space_limit", Safe, space_limit);

/// The system's limit (QemuOnly: it lowers it for every program for a moment): private memory, mode 2's
/// MAP_NORESERVE and shared anonymous memory get ENOMEM at it, a new tmpfs page ENOSPC (FS-14: tmpfs files are
/// charged per page like Linux's VM_NORESERVE shmem, so a growing ftruncate is free); what was promised still works.
fn system_limit() -> TestResult {
    let a = lazy()?;
    let ns = vfs::root_ns();
    // The file exists before the limit drops (its inode and name are charged too).
    let f = if ns.lookup("/tmp").is_ok() { Some(ns.open_flags(b"/tmp/.mm-commit-grow", vfs::abi::O_RDWR | vfs::abi::O_CREAT, 0o600).map_err(e)?) } else { None };
    commit::set_test_limit(Some(commit::committed() + 64));
    let r = (|| -> TestResult {
        kassert!(a.sys_mmap(0, 65 * P, RW, AP, 0) == Err(Errno::NoMem), "past the system limit");
        let m = a.sys_mmap(0, 64 * P, RW, AP, 0).map_err(e)?;
        kassert!(a.sys_mmap(0, P, RW, AP | MAP_NORESERVE, 0) == Err(Errno::NoMem), "MAP_NORESERVE");
        kassert!(a.sys_mmap(0, GB, 0, AP, 0).is_ok(), "a reservation still costs nothing");
        if let Some(f) = &f {
            let t = f.inode().ok_or("no inode")?.truncate(16 * P);
            kassert!(t.is_ok(), "tmpfs growth by ftruncate is free: {t:?}");
            let w = f.pwrite(0, b"x");
            kassert!(w == Err(uapi::errno::ENOSPC), "a new tmpfs page past the limit: {w:?}");
        }
        if ns.lookup("/dev/shm").is_ok() {
            kassert!(a.sys_mmap(0, 4 * P, RW, MAP_SHARED | MAP_ANONYMOUS, 0) == Err(Errno::NoMem), "shared anonymous memory");
        }
        for i in 0..64 {
            a.write_u64(m + i * P, i).map_err(e)?;
        }
        // A fork needs 64 more pages: refused, the system's commit unchanged.
        let sys = commit::committed();
        kassert!(a.fork().is_err(), "fork past the system limit");
        kassert!(commit::committed() == sys, "a refused fork kept {} pages charged", commit::committed() as i64 - sys as i64);
        // Room for the child, but the fork fails half-way: the child's charge goes back with it.
        commit::set_test_limit(Some(sys + 64));
        a.test_fail_fork_after(Some(10));
        let r = a.fork();
        a.test_fail_fork_after(None);
        kassert!(r.is_err(), "the injected failure did not refuse the fork");
        drop(r);
        kassert!(commit::committed() == sys, "a fork that failed half-way kept {} pages charged", commit::committed() as i64 - sys as i64);
        let b = a.fork().map_err(e)?;
        kassert!(commit::committed() == sys + 64, "the child is charged 64 pages");
        drop(b);
        kassert!(commit::committed() == sys, "the child's charge went back");
        Ok(())
    })();
    commit::set_test_limit(None);
    if f.is_some() {
        drop(f);
        let _ = ns.remove("/tmp/.mm-commit-grow");
    }
    r
}
ktest!("mm.commit.system_limit", QemuOnly, system_limit);

/// V8/Chromium: reserve 70 GB with 32 GB alignment (map 102 GB PROT_NONE, cut both ends), then commit what the
/// program uses (Y-NUMBERS: ~950 MB; here at most a quarter of the commit limit). The reservation costs no frame,
/// no page table and no commit; only touched pages cost frames.
fn reserve_70g() -> TestResult {
    const RES: u64 = 70 * GB;
    const ALIGN: u64 = 32 * GB;
    let a = lazy()?;
    let frames0 = user();
    let raw = a.sys_mmap(0, RES + ALIGN, 0, AP | MAP_NORESERVE, 0).map_err(e)?;
    let base = (raw + ALIGN - 1) & !(ALIGN - 1);
    if base > raw {
        a.sys_munmap(raw, base - raw).map_err(e)?;
    }
    a.sys_munmap(base + RES, raw + RES + ALIGN - (base + RES)).map_err(e)?;
    kassert!(base.is_multiple_of(ALIGN), "{base:#x} not 32 GB aligned");
    kassert!(a.committed_pages() == 0 && a.resident_pages() == 0, "the reservation cost {} pages / {} frames", a.committed_pages(), a.resident_pages());
    kassert!(paging::count_user_tables(a.root()) == 0, "page tables for a reservation");
    kassert!(user() == frames0, "frames for a reservation");
    let commit = (950 << 20).min(commit::limit().saturating_sub(commit::committed()) / 4 * P) & !(P - 1);
    a.sys_mprotect(base, commit, RW).map_err(e)?;
    kassert!(a.committed_pages() == commit / P, "commit {} pages", a.committed_pages());
    a.write_u64(base, 1).map_err(e)?;
    a.write_u64(base + commit - 8, 2).map_err(e)?;
    kassert!(a.resident_pages() == 2 && user() - frames0 == 2 * P, "touched pages only: {} resident", a.resident_pages());
    kassert!(a.read_u64(base + RES - P).is_err(), "the reserve's tail is still PROT_NONE");
    a.sys_munmap(base, RES).map_err(e)?;
    kassert!(a.committed_pages() == 0 && user() == frames0, "all back");
    klog::event!(Info, "mm", "ktest.reserve_70g", base = klog::Hex(base), committed_mb = commit >> 20; "70 GB reserve with 32 GB alignment cost nothing");
    Ok(())
}
ktest!("mm.commit.reserve_70g", Safe, reserve_70g);

/// The Y-NUMBERS pattern (Cobalt on the Mini-PC, YK-PLAN 2): a 70 GB reserve, ~2,250 small commits of 4-20 KB
/// inside it, DONTNEED of many of them (80/s there), decommits; the books stay exact throughout.
fn y_numbers() -> TestResult {
    let t0 = time::now_ns();
    let a = lazy()?;
    let frames0 = user();
    let base = a.sys_mmap(0, 70 * GB, 0, AP | MAP_NORESERVE, 0).map_err(e)?;
    let mut charged = 0;
    for i in 0..2250u64 {
        let pages = 1 + i % 5;
        let at = base + i * 8 * P;
        a.sys_mprotect(at, pages * P, RW).map_err(e)?;
        a.write_u64(at, i).map_err(e)?;
        charged += pages;
        if i % 3 == 0 {
            a.sys_madvise(at, pages * P, madv::DONTNEED).map_err(e)?;
            kassert!(a.read_u64(at).map_err(e)? == 0, "DONTNEED reads zero after");
        }
    }
    kassert!(a.committed_pages() == charged, "commit {} != {charged}", a.committed_pages());
    let (t1, vmas) = (time::now_ns(), a.vmas().len());
    // Decommit the way V8 does: MAP_FIXED PROT_NONE over the range (drops pages and charge).
    for i in 0..2250u64 {
        a.sys_mmap(base + i * 8 * P, 8 * P, 0, AP | MAP_FIXED | MAP_NORESERVE, 0).map_err(e)?;
    }
    kassert!(a.committed_pages() == 0 && a.resident_pages() == 0, "after decommit: {} pages, {} resident", a.committed_pages(), a.resident_pages());
    drop(a);
    kassert!(user() == frames0, "{} bytes not back", user().saturating_sub(frames0));
    klog::event!(Info, "mm", "ktest.y_numbers", commits = 2250, vmas = vmas, commit_ms = (t1 - t0) / 1_000_000,
        total_ms = (time::now_ns() - t0) / 1_000_000; "Y-NUMBERS replay (timing is judged on the Mini-PC only)");
    Ok(())
}
ktest!("mm.commit.y_numbers", Safe, y_numbers);
