//! ktests of transparent huge pages (MM-13): the 2 MiB fault, its fallback to 4 KiB pages and the fault-around,
//! the split on each path that cuts a huge page (munmap, mprotect, madvise, mremap, a VMA split), fork and
//! copy-on-write (split or merged back), the counters against a walk (RSS, VmPTE with the deposited tables,
//! AnonHugePages), the mode and PR_SET_THP_DISABLE, the TLB on every CPU, and FD-1's frame pattern.

use crate::linux::{madv, MAP_ANONYMOUS, MAP_FIXED, MAP_POPULATE, MAP_PRIVATE, MAP_STACK, MREMAP_FIXED, MREMAP_MAYMOVE};
use crate::thp::{self, Mode};
use crate::tlb_ktest::{ap_activate, ap_read_in, ap_write_in, aps, on, FAULT, PROBE_ROOT, WRITE_VAL};
use crate::{share, AddressSpace, Fill, MemStats, Prot, VmaKind};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::Ordering::SeqCst;
use hal::paging::{self, tlb, PageFlags, PageSize};
use ktest::{kassert, ktest, TestResult};

const P: u64 = 4096;
const M2: u64 = 2 << 20;
const AP: u64 = MAP_ANONYMOUS | MAP_PRIVATE;
const RW: u64 = 3;
const PF_W: u64 = 2;
const PF_U: u64 = 4;
/// 2 MiB aligned, away from the other tests' fixed addresses.
const BASE: u64 = 0x33_0000_0000;

fn lazy() -> Result<AddressSpace, String> {
    AddressSpace::with_fill(Fill::OnFault).map_err(|x| format!("address space: {x:?}"))
}

fn e<T: core::fmt::Debug>(x: T) -> String {
    format!("{x:?}")
}

fn user() -> u64 {
    mm_core::frames().map_or(0, |f| f.stats().used_by_kind[mm_core::Kind::User as usize])
}

fn write_fault(a: &AddressSpace, va: u64) -> TestResult {
    a.handle_user_fault(va, PF_U | PF_W).map_err(|w| format!("write fault at {va:#x}: {w:?}"))
}

/// The page size mapping `va` (None: nothing there).
fn size(a: &AddressSpace, va: u64) -> Option<PageSize> {
    paging::translate_leaf(a.root(), va).map(|l| l.size)
}

fn huge(a: &AddressSpace, va: u64) -> bool {
    size(a, va) == Some(PageSize::Size2M)
}

/// The kept counters equal a walk of the page tables (RSS, VmPTE with the deposits).
fn counted(a: &AddressSpace, what: &str) -> Result<MemStats, String> {
    let (k, w) = (a.mem_stats(), a.mem_stats_walked());
    kassert!(k == w, "{what}: kept {k:?}\n walked {w:?}");
    kassert!(a.counter_underflows() == 0, "{what}: a counter went below zero");
    Ok(k)
}

/// A write fault at `va` that should map a huge page: false when no 2 MiB block was free (a fragmented machine;
/// the fallback has its own test), and the caller fails.
fn first_huge(a: &AddressSpace, va: u64) -> Result<bool, String> {
    write_fault(a, va)?;
    Ok(huge(a, va))
}

/// Free 2 MiB blocks `room` asks compaction for (the tests map up to four huge pages at once).
const ROOM_BLOCKS: usize = 8;

/// W35 MM-3: earlier tests (the reclaim and file system load tests) leave memory full of clean file cache, spread
/// over the 2 MiB blocks. The tests that need huge pages ask direct compaction for `ROOM_BLOCKS` free blocks (it
/// drops clean cache out of Movable pageblocks until MM-2 moves pages), up to 2 s, instead of dropping the whole
/// cache as before; the heap's empty slabs and what waits for a grace period go back first. The blocks are held
/// while more are made, then freed for the test.
fn room() {
    mm_core::HEAP.shrink();
    mm_core::grace::expedite();
    let (Some(f), Some(b)) = (mm_core::frames(), mm_core::boot_buddy()) else { return };
    let mut held: Vec<u64> = Vec::new();
    if held.try_reserve_exact(ROOM_BLOCKS).is_err() {
        return;
    }
    let t0 = time::now_ns();
    while held.len() < ROOM_BLOCKS && time::now_ns() - t0 < 2_000_000_000 {
        if let Some(p) = f.alloc_light(mm_core::HUGE_ORDER, mm_core::Zone::Normal, mm_core::Kind::Other) {
            held.push(p);
            continue;
        }
        // Deferred after a failure, or nothing to clear right now: try again shortly.
        if !b.compact_direct(mm_core::HUGE_ORDER) {
            sched::sleep_ms(10);
        }
    }
    for p in held {
        f.free(p, mm_core::HUGE_ORDER, mm_core::Kind::Other);
    }
}

/// The fault took 4 KiB pages where a huge page was wanted: a skip only when no 2 MiB block is free right now (a
/// fragmented machine), a failure when one is (the huge path broke; a silent skip would hide it).
fn no_block() -> TestResult {
    let Some(f) = mm_core::frames() else { return ktest::fail("no frame allocator") };
    match f.alloc_light(mm_core::HUGE_ORDER, mm_core::Zone::Normal, mm_core::Kind::User) {
        Some(p) => {
            f.free(p, mm_core::HUGE_ORDER, mm_core::Kind::User);
            Err(String::from("a 2 MiB block is free, but the fault took 4 KiB pages"))
        }
        None => Err(format!("no free 2 MiB block: {}", census())),
    }
}

/// What holds the 2 MiB blocks now (the failure explains itself in the gate's output).
fn census() -> String {
    mm_core::block_census(mm_core::HUGE_ORDER).map_or(String::from("no boot allocator"), |c| format!("{c}"))
}

/// Writes `tag + i` into the first word of every 4 KiB page of [m, m + len).
fn stamp(a: &AddressSpace, m: u64, len: u64, tag: u64) -> TestResult {
    for i in 0..len / P {
        a.write_u64(m + i * P, tag + i).map_err(e)?;
    }
    Ok(())
}

fn stamped(a: &AddressSpace, m: u64, len: u64, tag: u64, what: &str) -> TestResult {
    for i in 0..len / P {
        let v = a.read_u64(m + i * P).map_err(e)?;
        kassert!(v == tag + i, "{what}: page {i} holds {v:#x}, want {:#x}", tag + i);
    }
    Ok(())
}

/// Restores the THP mode when a test ends (also on failure).
struct ModeBack(Mode);

impl Drop for ModeBack {
    fn drop(&mut self) {
        thp::set_mode(self.0);
    }
}

/// A write fault in an aligned 2 MiB inside a private anonymous VMA maps one 2 MiB page: zeroed, 512 pages of
/// RSS, AnonHugePages, no new commit, its page table deposited (VmPTE); a read fault, a cut 2 MiB, MADV_NOHUGEPAGE,
/// MAP_STACK, a stack VMA and an eager space stay page by page; munmap gives everything back.
fn fault() -> TestResult {
    room();
    let before = user();
    {
        let a = lazy()?;
        let m = a.sys_mmap(BASE, 8 * M2, RW, AP | MAP_FIXED, 0).map_err(e)?;
        let commit = a.committed_pages();
        let allocs = thp::FAULT_ALLOC.get();
        if !first_huge(&a, m + 5 * P + 8)? {
            return no_block();
        }
        kassert!(thp::FAULT_ALLOC.get() > allocs, "the huge fault was not counted");
        let l = paging::translate_leaf(a.root(), m + 5 * P).ok_or("gone")?;
        kassert!(l.page_virt == m && l.flags.contains(PageFlags::WRITABLE | PageFlags::USER), "{l:?}");
        kassert!(mm_core::is_user_block(l.phys & !(M2 - 1), mm_core::HUGE_ORDER), "the frame is one order-9 user block");
        kassert!(a.read_u64(m + M2 - 8).map_err(e)? == 0 && a.read_u64(m).map_err(e)? == 0, "a huge page is zeroed");
        let s = counted(&a, "one huge page")?;
        kassert!(s.rss == 512 && s.anon == 512 && a.huge_pages() == 1, "{s:?}");
        kassert!(a.committed_pages() == commit, "a huge page charged {} more pages", a.committed_pages() - commit);
        kassert!(paging::count_user_tables(a.root()) + 1 == s.page_tables, "the deposited table counts in VmPTE: {s:?}");
        kassert!(a.anon_huge_bytes() == M2 && a.anon_huge_in(m, m + 8 * M2) == M2 && a.anon_huge_in(m + M2, m + 8 * M2) == 0, "AnonHugePages");
        // Global: other programs (on the Mini-PC) map and free huge pages meanwhile, so only "ours is in it".
        kassert!(thp::anon_huge_bytes() >= M2, "/proc/meminfo AnonHugePages");
        let u = a.page_usage().map_err(e)?;
        let (_, pu) = u.iter().find(|(v, _)| v.start == m).ok_or("no VMA")?;
        kassert!(pu.resident == M2 && pu.anonymous == M2 && pu.private_dirty == M2 && pu.pss == M2 << crate::PSS_SHIFT, "smaps {pu:?}");
        // A read fault in the next 2 MiB takes only its page (Linux maps the huge zero page there, uncounted).
        a.handle_user_fault(m + M2 + 3 * P, PF_U).map_err(e)?;
        kassert!(size(&a, m + M2 + 3 * P) == Some(PageSize::Size4K), "a read fault took {:?}", size(&a, m + M2 + 3 * P));
        // A write fault there now meets a page table: 4 KiB and the 64 KiB fault-around (no khugepaged).
        write_fault(&a, m + M2 + 40 * P)?;
        kassert!(size(&a, m + M2 + 40 * P) == Some(PageSize::Size4K) && size(&a, m + M2 + 47 * P).is_some(), "the 2 MiB with a table");
        // A user copy into fresh memory takes a huge page as a fault would (Linux: the kernel's copy faults).
        a.write_u64(m + 2 * M2 + 99, 7).map_err(e)?;
        kassert!(huge(&a, m + 2 * M2), "a copy_to_user into fresh memory");
        a.sys_munmap(m, 8 * M2).map_err(e)?;
        let s = counted(&a, "after munmap")?;
        kassert!(s.rss == 0 && a.huge_pages() == 0 && s.page_tables == paging::count_user_tables(a.root()), "{s:?}");
        // Page by page: a VMA without a whole aligned 2 MiB, MADV_NOHUGEPAGE, MAP_STACK, a stack VMA.
        let cut = a.sys_mmap(BASE + P, M2, RW, AP | MAP_FIXED, 0).map_err(e)?;
        write_fault(&a, cut + M2 / 2)?;
        kassert!(size(&a, cut + M2 / 2) == Some(PageSize::Size4K), "a cut 2 MiB");
        let nh = a.sys_mmap(BASE + 4 * M2, 2 * M2, RW, AP | MAP_FIXED, 0).map_err(e)?;
        a.sys_madvise(nh, 2 * M2, madv::NOHUGEPAGE).map_err(e)?;
        write_fault(&a, nh + 8)?;
        kassert!(size(&a, nh) == Some(PageSize::Size4K) && size(&a, nh + P).is_none(), "MADV_NOHUGEPAGE: no huge page, no fault-around");
        let st = a.sys_mmap(BASE + 8 * M2, 2 * M2, RW, AP | MAP_FIXED | MAP_STACK, 0).map_err(e)?;
        write_fault(&a, st + 8)?;
        kassert!(size(&a, st) == Some(PageSize::Size4K), "MAP_STACK");
        a.map_fixed(BASE + 12 * M2, 2 * M2, Prot::from_linux(RW), VmaKind::Stack).map_err(e)?;
        write_fault(&a, BASE + 12 * M2 + 8)?;
        kassert!(size(&a, BASE + 12 * M2) == Some(PageSize::Size4K), "the stack VMA");
        // MAP_POPULATE fills a writable mapping with huge pages too (Linux's populate faults for writing).
        let pop = a.sys_mmap(BASE + 16 * M2, 2 * M2, RW, AP | MAP_FIXED | MAP_POPULATE, 0).map_err(e)?;
        kassert!(huge(&a, pop) && huge(&a, pop + M2), "MAP_POPULATE");
        counted(&a, "page by page cases")?;
        // An eager space (no fault path) never maps one.
        let ea = AddressSpace::new().map_err(e)?;
        let em = ea.sys_mmap(BASE, 2 * M2, RW, AP | MAP_FIXED, 0).map_err(e)?;
        kassert!(size(&ea, em) == Some(PageSize::Size4K), "an eager space");
    }
    kassert!(user() == before, "{} bytes of user frames not back", user().saturating_sub(before));
    Ok(())
}
ktest!("mm.thp.fault", Safe, fault);

/// Without a free 2 MiB block a fault takes its page and the 64 KiB fault-around (Linux VM_FAULT_FALLBACK).
fn fallback() -> TestResult {
    let before = user();
    {
        let a = lazy()?;
        let m = a.sys_mmap(BASE, 2 * M2, RW, AP | MAP_FIXED, 0).map_err(e)?;
        a.test_fail_huge(true);
        let fb = thp::FAULT_FALLBACK.get();
        write_fault(&a, m + 5 * P)?;
        kassert!(thp::FAULT_FALLBACK.get() > fb, "the fallback was not counted");
        kassert!((0..16).all(|i| size(&a, m + i * P) == Some(PageSize::Size4K)) && size(&a, m + 16 * P).is_none(), "4 KiB pages and the 64 KiB block");
        a.write_u64(m + M2 + 8, 1).map_err(e)?;
        kassert!(size(&a, m + M2) == Some(PageSize::Size4K), "a user copy falls back too");
        a.test_fail_huge(false);
        write_fault(&a, m + M2 / 2)?;
        kassert!(size(&a, m + M2 / 2) == Some(PageSize::Size4K), "a 2 MiB with a page table stays 4 KiB");
        counted(&a, "fallback")?;
    }
    kassert!(user() == before, "{} bytes of user frames not back", user().saturating_sub(before));
    Ok(())
}
ktest!("mm.thp.fallback", Safe, fallback);

/// Every path that cuts a huge page splits it into 4 KiB entries over the same frames (content kept, counters
/// equal the walk, the deposited table used, nothing charged), and a whole one goes without a split.
fn split() -> TestResult {
    room();
    let before = user();
    {
        let a = lazy()?;
        let fresh = |k: u64| -> Result<Option<u64>, String> {
            let m = a.sys_mmap(BASE + k * 4 * M2, M2, RW, AP | MAP_FIXED, 0).map_err(e)?;
            if !first_huge(&a, m)? {
                return Ok(None);
            }
            stamp(&a, m, M2, k << 32)?;
            Ok(Some(m))
        };
        let Some(m) = fresh(0)? else { return no_block() };
        // munmap in the middle.
        let (splits, tables) = (thp::SPLIT_PMD.get(), a.mem_stats().page_tables);
        a.sys_munmap(m + M2 / 2, P).map_err(e)?;
        kassert!(thp::SPLIT_PMD.get() > splits && a.huge_pages() == 0, "munmap in the middle split it");
        kassert!(size(&a, m) == Some(PageSize::Size4K) && size(&a, m + M2 / 2).is_none(), "4 KiB entries and the hole");
        kassert!(a.mem_stats().page_tables == tables, "the split used the deposited table");
        stamped(&a, m, M2 / 2, 0, "munmap: below the hole")?;
        stamped(&a, m + M2 / 2 + P, M2 / 2 - P, (M2 / 2 + P) / P, "munmap: above the hole")?;
        let s = counted(&a, "munmap split")?;
        kassert!(s.anon == 511, "{s:?}");
        // mprotect of a part.
        let Some(m) = fresh(1)? else { return no_block() };
        a.sys_mprotect(m, 16 * P, 1).map_err(e)?;
        let ro = paging::translate_leaf(a.root(), m).ok_or("gone")?;
        let rw = paging::translate_leaf(a.root(), m + 16 * P).ok_or("gone")?;
        kassert!(ro.size == PageSize::Size4K && !ro.flags.contains(PageFlags::WRITABLE) && rw.flags.contains(PageFlags::WRITABLE), "mprotect part: {ro:?} {rw:?}");
        stamped(&a, m, M2, 1 << 32, "mprotect")?;
        a.sys_mprotect(m, 16 * P, RW).map_err(e)?;
        // madvise(DONTNEED) of a part: zeros there, the rest kept.
        let Some(m) = fresh(2)? else { return no_block() };
        a.sys_madvise(m + 8 * P, 8 * P, madv::DONTNEED).map_err(e)?;
        kassert!(a.read_u64(m + 8 * P).map_err(e)? == 0, "DONTNEED part not zero");
        stamped(&a, m, 8 * P, 2 << 32, "DONTNEED below")?;
        stamped(&a, m + 16 * P, M2 - 16 * P, (2 << 32) + 16, "DONTNEED above")?;
        // A VMA split at an unaligned address (madvise NOHUGEPAGE on a part).
        let Some(m) = fresh(3)? else { return no_block() };
        a.sys_madvise(m + M2 - 64 * P, 64 * P, madv::NOHUGEPAGE).map_err(e)?;
        kassert!(size(&a, m) == Some(PageSize::Size4K), "a VMA split inside a huge page splits it");
        stamped(&a, m, M2, 3 << 32, "VMA split")?;
        // mremap to an address with the same offset in 2 MiB: moves whole; to another offset: split and moved.
        let Some(m) = fresh(4)? else { return no_block() };
        let to = BASE + 40 * M2;
        let splits = thp::SPLIT_PMD.get();
        let n = a.sys_mremap(m, M2, M2, MREMAP_MAYMOVE | MREMAP_FIXED, to).map_err(e)?;
        kassert!(n == to && huge(&a, to) && size(&a, m).is_none() && thp::SPLIT_PMD.get() == splits, "mremap whole");
        stamped(&a, to, M2, 4 << 32, "mremap whole")?;
        let odd = BASE + 44 * M2 + 16 * P;
        a.sys_mremap(to, M2, M2, MREMAP_MAYMOVE | MREMAP_FIXED, odd).map_err(e)?;
        kassert!(size(&a, odd) == Some(PageSize::Size4K) && thp::SPLIT_PMD.get() > splits, "mremap to another offset");
        stamped(&a, odd, M2, 4 << 32, "mremap moved page by page")?;
        // A whole huge page unmapped: no split, the block freed whole.
        let Some(m) = fresh(5)? else { return no_block() };
        let held = user();
        a.sys_munmap(m, M2).map_err(e)?;
        kassert!(held - user() == M2, "a whole huge page: {} bytes back", held - user());
        let s = counted(&a, "after the splits")?;
        kassert!(a.huge_pages() == 0 && s.page_tables == paging::count_user_tables(a.root()), "no deposit left: {s:?}");
    }
    kassert!(user() == before, "{} bytes of user frames not back", user().saturating_sub(before));
    Ok(())
}
ktest!("mm.thp.split", Safe, split);

/// fork keeps 2 MiB entries in both spaces, read-only over 512 shared frames; a write splits the writer's entry and
/// copies one page; once a space holds all 512 alone again, a write merges them and keeps the 2 MiB entry.
fn fork() -> TestResult {
    room();
    let before = user();
    {
        let a = lazy()?;
        let m = a.sys_mmap(BASE, 2 * M2, RW, AP | MAP_FIXED, 0).map_err(e)?;
        if !first_huge(&a, m)? || !first_huge(&a, m + M2)? {
            return no_block();
        }
        stamp(&a, m, 2 * M2, 0x77 << 32)?;
        let held = user();
        let b = a.fork().map_err(e)?;
        kassert!(user() == held, "fork copied {} bytes", user() - held);
        let (la, lb) = (paging::translate_leaf(a.root(), m).ok_or("parent")?, paging::translate_leaf(b.root(), m).ok_or("child")?);
        kassert!(la.size == PageSize::Size2M && lb.size == PageSize::Size2M && la.phys == lb.phys, "2 MiB in both: {la:?} {lb:?}");
        kassert!(!la.flags.contains(PageFlags::WRITABLE) && !lb.flags.contains(PageFlags::WRITABLE), "read-only in both");
        kassert!(!mm_core::is_user_block(la.phys, mm_core::HUGE_ORDER) && (0..512).all(|i| share::others(la.phys + i * P) == 1), "512 shared frames");
        kassert!(b.anon_huge_bytes() == 2 * M2 && b.committed_pages() == a.committed_pages(), "child's AnonHugePages and commit");
        let pu = b.page_usage().map_err(e)?;
        let (_, u) = pu.iter().find(|(v, _)| v.start == m).ok_or("no VMA")?;
        kassert!(u.pss == (2 * M2 << crate::PSS_SHIFT) / 2 && u.shared_dirty == 2 * M2, "child smaps after fork: {u:?}");
        counted(&a, "parent after fork")?;
        counted(&b, "child after fork")?;
        // The child writes page 3 of the first 2 MiB: its entry splits, the page is copied.
        let (cs, cc) = (thp::COW_SPLIT.get(), crate::aspace::fault::COW_COPIES.get());
        b.handle_user_fault(m + 3 * P, PF_U | PF_W).map_err(e)?;
        b.write_u64(m + 3 * P, 0xC0FFEE).map_err(e)?;
        kassert!(thp::COW_SPLIT.get() > cs && crate::aspace::fault::COW_COPIES.get() > cc, "split and one copy");
        kassert!(size(&b, m) == Some(PageSize::Size4K) && huge(&b, m + M2) && huge(&a, m), "only the child's first entry split");
        stamped(&a, m, 2 * M2, 0x77 << 32, "parent after the child's write")?;
        kassert!(user() == held + P, "one page copied: {} bytes", user() - held);
        // The parent writes the same page: it holds that frame alone now, but shares the other 511: split, reuse.
        a.handle_user_fault(m + 3 * P, PF_U | PF_W).map_err(e)?;
        kassert!(size(&a, m) == Some(PageSize::Size4K), "the parent's entry split");
        kassert!(user() == held + P, "the parent's write copied: {} bytes", user() - held);
        counted(&a, "parent after its write")?;
        counted(&b, "child after its write")?;
        // The child ends (fork + exec): the parent holds the second 2 MiB's frames alone again; a write merges them.
        drop(b);
        let merged = thp::MERGED.get();
        a.handle_user_fault(m + M2 + 9 * P, PF_U | PF_W).map_err(e)?;
        let l = paging::translate_leaf(a.root(), m + M2).ok_or("gone")?;
        kassert!(l.size == PageSize::Size2M && l.flags.contains(PageFlags::WRITABLE), "still 2 MiB, writable: {l:?}");
        kassert!(thp::MERGED.get() > merged && mm_core::is_user_block(l.phys & !(M2 - 1), mm_core::HUGE_ORDER), "merged back into one block");
        stamped(&a, m + M2, M2, (0x77 << 32) + 512, "after the merge")?;
        counted(&a, "parent at the end")?;
        // PR_SET_THP_DISABLE goes to the child.
        a.set_thp_disabled(true);
        let c = a.fork().map_err(e)?;
        kassert!(c.thp_disabled(), "the child lost PR_SET_THP_DISABLE");
    }
    kassert!(user() == before, "{} bytes of user frames not back", user().saturating_sub(before));
    Ok(())
}
ktest!("mm.thp.fork", Safe, fork);

/// A fork refused part-way with huge pages (`test_fail_fork_after`): before the first 2 MiB entry (nothing split),
/// inside it, and after it (its frames split and shared, the child's drop gives them back). Each time the parent
/// keeps both 2 MiB entries, its data, its commit and counters, no frame stays shared or held; a write then takes
/// each entry back writable and whole (merged), and a fork works again.
fn fork_refused() -> TestResult {
    room();
    let before = user();
    {
        let a = lazy()?;
        let m = a.sys_mmap(BASE, 2 * M2, RW, AP | MAP_FIXED, 0).map_err(e)?;
        if !first_huge(&a, m)? || !first_huge(&a, m + M2)? {
            return no_block();
        }
        stamp(&a, m, 2 * M2, 0x55 << 32)?;
        let (held, c0, s0) = (user(), a.committed_pages(), counted(&a, "before")?);
        let heads = [m, m + M2].map(|va| paging::translate_leaf(a.root(), va).map(|l| l.phys));
        let [Some(h0), Some(h1)] = heads else { return Err(String::from("no leaf")) };
        for n in [0, 1, 511, 512, 513, 1023] {
            a.test_fail_fork_after(Some(n));
            let r = a.fork();
            a.test_fail_fork_after(None);
            kassert!(r.is_err(), "fail after {n}: the fork went through");
            drop(r);
            let what = format!("fail after {n}");
            kassert!(huge(&a, m) && huge(&a, m + M2) && a.huge_pages() == 2, "{what}: the parent lost a 2 MiB entry");
            kassert!(a.anon_huge_bytes() == 2 * M2, "{what}: AnonHugePages {}", a.anon_huge_bytes());
            kassert!(user() == held, "{what}: {} bytes of user frames held", user().abs_diff(held));
            kassert!(a.committed_pages() == c0, "{what}: commit {} != {c0}", a.committed_pages());
            let s = counted(&a, &what)?;
            kassert!(s == s0, "{what}: {s:?}\n before {s0:?}");
            for h in [h0, h1] {
                kassert!((0..512).all(|i| share::others(h + i * P) == 0), "{what}: a frame at {h:#x} is still shared");
            }
            stamped(&a, m, 2 * M2, 0x55 << 32, &what)?;
        }
        // The stores take both entries back: writable, whole, one block each (the first one merged back).
        for va in [m + 8, m + M2 + 8] {
            a.handle_user_fault(va, PF_U | PF_W).map_err(e)?;
            let l = paging::translate_leaf(a.root(), va).ok_or("gone")?;
            kassert!(l.size == PageSize::Size2M && l.flags.contains(PageFlags::WRITABLE), "after the store at {va:#x}: {l:?}");
            kassert!(mm_core::is_user_block(l.phys & !(M2 - 1), mm_core::HUGE_ORDER), "{va:#x}: not one block again");
        }
        kassert!(user() == held, "the stores copied {} bytes", user().abs_diff(held));
        stamped(&a, m, 2 * M2, 0x55 << 32, "after the stores")?;
        counted(&a, "after the stores")?;
        let b = a.fork().map_err(e)?;
        stamped(&b, m, 2 * M2, 0x55 << 32, "the child of a fork after the refused ones")?;
        counted(&b, "child")?;
    }
    kassert!(user() == before, "{} bytes of user frames not back", user().saturating_sub(before));
    Ok(())
}
ktest!("mm.thp.fork_refused", Safe, fork_refused);

/// Takes every free block of order 9 or more, so the next fault finds no free 2 MiB block (unless one is freed or
/// cleared meanwhile: the caller looks at what the fault got).
fn take_huge_blocks(f: &dyn mm_core::FrameAlloc, held: &mut Vec<(u64, u32)>) {
    for order in (mm_core::HUGE_ORDER..=mm_core::MAX_ORDER).rev() {
        while held.len() < held.capacity() {
            let Some(p) = f.alloc_light(order, mm_core::Zone::Normal, mm_core::Kind::Other) else { break };
            held.push((p, order));
        }
    }
}

/// Without a free 2 MiB block (`thp::steps`, defrag `madvise`, the default): a fault outside MADV_HUGEPAGE never
/// stalls, wakes kcompactd for order 9 and takes 4 KiB pages; inside, it compacts directly and maps a huge page
/// exactly when that made a block, else wakes kcompactd too. Each case is tried on up
/// to four fresh 2 MiB, as a block someone frees between the take and the fault serves the fault directly.
fn defrag() -> TestResult {
    let Some(f) = mm_core::frames() else { return ktest::fail("no frame allocator") };
    let before = user();
    let mut held: Vec<(u64, u32)> = Vec::new();
    if held.try_reserve_exact((f.stats().total_bytes / M2) as usize + 64).is_err() {
        return ktest::fail("no room for the list of held blocks");
    }
    let r = (|| -> TestResult {
        let a = lazy()?;
        let m = a.sys_mmap(BASE, 8 * M2, RW, AP | MAP_FIXED, 0).map_err(e)?;
        a.sys_madvise(m + 4 * M2, 4 * M2, madv::HUGEPAGE).map_err(e)?;
        let mut plain = false;
        for k in 0..4 {
            take_huge_blocks(f, &mut held);
            let (noted, stalls) = (mm_core::compact::wakes_asked(), thp::DIRECT_STALL.get());
            write_fault(&a, m + k * M2)?;
            if huge(&a, m + k * M2) {
                continue;
            }
            kassert!(mm_core::compact::wakes_asked() > noted, "without MADV_HUGEPAGE: kcompactd not woken for order 9");
            kassert!(thp::DIRECT_STALL.get() == stalls, "a fault without MADV_HUGEPAGE stalled for a block");
            plain = true;
            break;
        }
        kassert!(plain, "a 2 MiB block came free before each of four faults without MADV_HUGEPAGE");
        let mut madvised = false;
        for k in 4..8 {
            take_huge_blocks(f, &mut held);
            let (noted, stalls, allocs) = (mm_core::compact::wakes_asked(), thp::DIRECT_STALL.get(), thp::DIRECT_ALLOC.get());
            write_fault(&a, m + k * M2)?;
            let (got, stalled, made) = (huge(&a, m + k * M2), thp::DIRECT_STALL.get() > stalls, thp::DIRECT_ALLOC.get() > allocs);
            if got && !stalled {
                continue;
            }
            kassert!(stalled, "MADV_HUGEPAGE: 4 KiB pages without the direct step");
            kassert!(got == made, "MADV_HUGEPAGE: huge page {got}, direct step made a block {made}");
            kassert!(got || mm_core::compact::wakes_asked() > noted, "MADV_HUGEPAGE: the direct step failed and kcompactd was not woken");
            klog::event!(Info, "mm", "ktest.thp_defrag", direct_huge = got, held_blocks = held.len(), census = census().as_str());
            madvised = true;
            break;
        }
        kassert!(madvised, "a 2 MiB block came free before each of four faults with MADV_HUGEPAGE");
        counted(&a, "defrag")?;
        Ok(())
    })();
    for (p, o) in held {
        f.free(p, o, mm_core::Kind::Other);
    }
    r?;
    kassert!(user() == before, "{} bytes of user frames not back", user().saturating_sub(before));
    Ok(())
}
// QEMU only: it holds every free 2 MiB block for a moment and makes compaction clear cache for order 9.
ktest!("mm.thp.defrag", QemuOnly, defrag);

/// The modes of /sys/kernel/mm/transparent_hugepage/enabled and PR_SET_THP_DISABLE (system-wide switch: QEMU only).
fn modes() -> TestResult {
    let _back = ModeBack(thp::mode());
    // QEMU runs nothing else: the global AnonHugePages must come back exactly (Safe tests only check their space).
    let blocks = thp::anon_huge_bytes();
    let a = lazy()?;
    let m = a.sys_mmap(BASE, 8 * M2, RW, AP | MAP_FIXED, 0).map_err(e)?;
    thp::set_mode(Mode::Never);
    write_fault(&a, m)?;
    kassert!(size(&a, m) == Some(PageSize::Size4K) && size(&a, m + P).is_none(), "never: page by page, no fault-around");
    thp::set_mode(Mode::Madvise);
    write_fault(&a, m + 2 * M2)?;
    kassert!(size(&a, m + 2 * M2) == Some(PageSize::Size4K), "madvise without MADV_HUGEPAGE");
    a.sys_madvise(m + 4 * M2, M2, madv::HUGEPAGE).map_err(e)?;
    if !first_huge(&a, m + 4 * M2)? {
        return no_block();
    }
    thp::set_mode(Mode::Always);
    a.set_thp_disabled(true);
    write_fault(&a, m + 6 * M2)?;
    kassert!(size(&a, m + 6 * M2) == Some(PageSize::Size4K) && size(&a, m + 6 * M2 + P).is_none(), "PR_SET_THP_DISABLE: page by page");
    kassert!(huge(&a, m + 4 * M2), "PR_SET_THP_DISABLE keeps what is mapped");
    kassert!(thp::store(b"never\n").is_ok() && thp::enabled_text() == "always madvise [never]\n" && thp::store(b"sometimes").is_err(), "enabled");
    counted(&a, "modes")?;
    kassert!(thp::anon_huge_bytes() == blocks + M2, "/proc/meminfo AnonHugePages: {} -> {}", blocks, thp::anon_huge_bytes());
    drop(a);
    kassert!(thp::anon_huge_bytes() == blocks, "AnonHugePages not back: {} -> {}", blocks, thp::anon_huge_bytes());
    Ok(())
}
ktest!("mm.thp.modes", QemuOnly, modes);

/// On every CPU: APs read through a 2 MiB entry; a munmap of a part splits it (one round for the split, one for
/// the unmap) and the APs then fault on the hole and read the rest right; an mprotect of a part stops their
/// writes there; a munmap of whole huge pages is one round of 2 MiB invalidations.
fn tlb_all() -> TestResult {
    room();
    let aps = aps();
    let before = user();
    let _probes = paging::probe::user_probes().map_err(e)?;
    let r = (|| -> TestResult {
        let a = lazy()?;
        PROBE_ROOT.store(a.root(), SeqCst);
        let m = a.sys_mmap(BASE, 4 * M2, RW, AP | MAP_FIXED, 0).map_err(e)?;
        if !first_huge(&a, m)? || !first_huge(&a, m + 2 * M2)? || !first_huge(&a, m + 3 * M2)? {
            return no_block();
        }
        stamp(&a, m, M2, 0x55 << 32)?;
        for &c in &aps {
            on(c, ap_activate, a.root())?;
            kassert!(on(c, ap_read_in, m + 100 * P)? == (0x55 << 32) + 100, "CPU {c} reads the huge page wrong");
        }
        let g = tlb::generation(a.root()).unwrap_or(0);
        a.sys_munmap(m + 256 * P, P).map_err(e)?;
        let rounds = tlb::generation(a.root()).unwrap_or(0) - g;
        kassert!(rounds <= 2, "split + munmap took {rounds} rounds");
        for &c in &aps {
            let r = on(c, ap_read_in, m + 256 * P)?;
            kassert!(r & FAULT != 0, "CPU {c} still reads the unmapped page: {r:#x}");
            kassert!(on(c, ap_read_in, m + 300 * P)? == (0x55 << 32) + 300, "CPU {c} reads the split page wrong");
        }
        a.sys_mprotect(m, 8 * P, 1).map_err(e)?;
        for &c in &aps {
            WRITE_VAL.store(0x99, SeqCst);
            kassert!(on(c, ap_write_in, m + 2 * P)? & FAULT != 0, "CPU {c} wrote through a read-only part");
        }
        // Whole huge pages: one round, flushed one invlpg per 2 MiB entry.
        let (s0, g0) = (tlb::stats(), tlb::generation(a.root()).unwrap_or(0));
        a.sys_munmap(m + 2 * M2, 2 * M2).map_err(e)?;
        let (s1, g1) = (tlb::stats(), tlb::generation(a.root()).unwrap_or(0));
        kassert!(g1 - g0 <= 1, "two whole huge pages took {} rounds", g1 - g0);
        klog::event!(Info, "mm", "ktest.thp_tlb", aps = aps.len(), rounds = g1 - g0, page_flushes = s1.page_flushes - s0.page_flushes,
            full_flushes = s1.full_flushes - s0.full_flushes);
        counted(&a, "tlb")?;
        for &c in &aps {
            on(c, ap_activate, paging::home_root())?;
        }
        Ok(())
    })();
    for &c in &aps {
        let _ = on(c, ap_activate, paging::home_root());
    }
    r?;
    kassert!(user() == before, "{} bytes of user frames not back", user().saturating_sub(before));
    Ok(())
}
ktest!("mm.thp.tlb", Safe, tlb_all);

/// FD-1's pattern (Chromium's software compositor): a fresh 13.8 MB buffer per frame, written front to back and
/// unmapped, once with huge pages and once without free 2 MiB blocks (MM-12's 64 KiB fault-around: the state
/// before MM-13). Correctness only in QEMU: the faults per frame, the content, every frame back; us per frame of
/// both for the Mini-PC (`ktest.thp_fd1`).
fn fd1_frames() -> TestResult {
    room();
    const LEN: u64 = 13_800_000;
    const FRAMES: u64 = 30;
    let before = user();
    let a = lazy()?;
    let pages = LEN.div_ceil(P);
    let run = |no_blocks: bool| -> Result<(u64, u64, u64), String> {
        a.test_fail_huge(no_blocks);
        let (mut faults, mut huge_pages) = (0u64, 0u64);
        let t0 = time::now_ns();
        for f in 0..FRAMES {
            let m = a.sys_mmap(0, LEN, RW, AP, 0).map_err(e)?;
            for i in 0..pages {
                let va = m + i * P;
                if paging::translate(a.root(), va).is_none() {
                    write_fault(&a, va)?;
                    faults += 1;
                }
            }
            // Per frame: earlier tests can leave memory fragmented, so some frames find no free 2 MiB block.
            let h = a.huge_pages();
            kassert!(h == 0 || h >= 5, "frame {f}: {h} huge pages; {}", census());
            huge_pages += h;
            a.write_u64(m + LEN - 8, f).map_err(e)?;
            kassert!(a.read_u64(m + LEN - 8).map_err(e)? == f && a.read_u64(m + LEN / 2).map_err(e)? == 0, "frame {f}: content");
            a.sys_munmap(m, LEN).map_err(e)?;
        }
        Ok((faults, huge_pages, time::now_ns() - t0))
    };
    let (faults, huge_pages, ns) = run(false)?;
    let (faults4k, huge4k, ns4k) = run(true)?;
    a.test_fail_huge(false);
    counted(&a, "fd1")?;
    kassert!(a.resident_pages() == 0 && user() == before, "frames not back: {} bytes", user().saturating_sub(before));
    // An unaligned 13.8 MB mapping holds at least 5 whole aligned 2 MiB (checked per frame above); the rest is
    // 4 KiB with fault-around.
    kassert!(faults <= faults4k && huge4k == 0, "{faults} faults with huge pages, {faults4k} without ({huge4k} huge pages there)");
    kassert!(faults4k < FRAMES * (pages / 16 + 64), "{faults4k} faults for {FRAMES} frames of {pages} pages");
    klog::event!(Info, "mm", "ktest.thp_fd1", frames = FRAMES, pages_per_frame = pages, faults_per_frame = faults / FRAMES,
        huge_per_frame = huge_pages / FRAMES, us_per_frame = ns / FRAMES / 1000, around_faults_per_frame = faults4k / FRAMES,
        around_us_per_frame = ns4k / FRAMES / 1000);
    Ok(())
}
// Only with the desktop stopped: it compares all user frames of the machine, and a running desktop moves them too (4.11., W30 round).
ktest!("mm.thp.fd1", Screen, fd1_frames);
