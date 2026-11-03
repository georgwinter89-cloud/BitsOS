//! ktests of the anonymous fault-around (MM-12): a write fault fills the missing pages of its aligned 64 KiB
//! block when the block lies inside the faulting VMA, charged nothing new, counted in RSS; blocks cut by the VMA,
//! stacks, shared anonymous memory, MADV_RANDOM/NOHUGEPAGE and read faults stay page by page; a frame that
//! cannot be had ends the round.

use crate::linux::{madv, MAP_ANONYMOUS, MAP_FIXED, MAP_GROWSDOWN, MAP_PRIVATE, MAP_SHARED, MAP_STACK};
use crate::{share, AddressSpace, Fill, MemStats, Prot, VmaKind};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use hal::paging;
use ktest::{kassert, ktest, TestResult};

const P: u64 = 4096;
const BLOCK: u64 = 16 * P;
const AP: u64 = MAP_ANONYMOUS | MAP_PRIVATE;
const RW: u64 = 3;
const PF_W: u64 = 2;
const PF_U: u64 = 4;
/// 64 KiB aligned (and 2 MiB aligned, so a block never shares its page table with another test's).
const BASE: u64 = 0x31_0000_0000;

fn lazy() -> Result<AddressSpace, String> {
    AddressSpace::with_fill(Fill::OnFault).map_err(|x| format!("address space: {x:?}"))
}

fn e<T: core::fmt::Debug>(x: T) -> String {
    format!("{x:?}")
}

/// The pages of [lo, hi) that have a frame, as page indexes from `lo`.
fn filled(a: &AddressSpace, lo: u64, hi: u64) -> Vec<u64> {
    (0..(hi - lo) / P).filter(|i| paging::translate(a.root(), lo + i * P).is_some()).collect()
}

fn span(lo: u64, hi: u64) -> Vec<u64> {
    (lo..hi).collect()
}

/// The kept counters equal a walk of the page tables (RSS for /proc/self/status and statm).
fn counted(a: &AddressSpace, what: &str) -> Result<MemStats, String> {
    let (k, w) = (a.mem_stats(), a.mem_stats_walked());
    kassert!(k == w, "{what}: kept {k:?}\n walked {w:?}");
    kassert!(a.resident_pages() == k.resident && a.rss_pages() == k.rss, "{what}: lock-free readers differ");
    Ok(k)
}

fn write_fault(a: &AddressSpace, va: u64) -> TestResult {
    a.handle_user_fault(va, PF_U | PF_W).map_err(|w| format!("write fault at {va:#x}: {w:?}"))
}

fn block() -> TestResult {
    let a = lazy()?;
    let m = a.sys_mmap(BASE, 4 * BLOCK, RW, AP | MAP_FIXED, 0).map_err(e)?;
    let commit = a.committed_pages();
    kassert!(commit == 64, "the mapping charged its 64 pages ({commit})");
    write_fault(&a, m + 5 * P + 8)?;
    kassert!(filled(&a, m, m + 4 * BLOCK) == span(0, 16), "one write fills its block: {:?}", filled(&a, m, m + 4 * BLOCK));
    kassert!(a.committed_pages() == commit, "fault-around charged {} more pages", a.committed_pages() - commit);
    let s = counted(&a, "after the block")?;
    kassert!(s.rss == 16 && s.anon == 16 && s.hiwater_rss == 16, "RSS counts the block {s:?}");
    // A read fault fills only its page.
    a.handle_user_fault(m + 20 * P, PF_U).map_err(e)?;
    kassert!(filled(&a, m + BLOCK, m + 2 * BLOCK) == [4], "a read fault: {:?}", filled(&a, m + BLOCK, m + 2 * BLOCK));
    // A page already present in the block keeps its frame and content.
    a.write_u64(m + 33 * P, 0x5EED).map_err(e)?;
    let (keep, _) = paging::translate(a.root(), m + 33 * P).ok_or("the copy did not fill its page")?;
    write_fault(&a, m + 40 * P)?;
    kassert!(filled(&a, m + 2 * BLOCK, m + 3 * BLOCK) == span(0, 16));
    kassert!(paging::translate(a.root(), m + 33 * P).map(|x| x.0) == Some(keep), "the present page was replaced");
    kassert!(a.read_u64(m + 33 * P).map_err(e)? == 0x5EED && a.read_u64(m + 34 * P).map_err(e)? == 0, "contents");
    // The read-faulted block gets the rest on the next write fault there.
    write_fault(&a, m + 17 * P)?;
    kassert!(filled(&a, m + BLOCK, m + 2 * BLOCK) == span(0, 16));
    kassert!(filled(&a, m + 3 * BLOCK, m + 4 * BLOCK).is_empty(), "the last block is untouched");
    let s = counted(&a, "after three blocks")?;
    kassert!(s.anon == 48 && a.committed_pages() == commit, "{s:?}, commit {}", a.committed_pages());
    a.sys_munmap(m, 4 * BLOCK).map_err(e)?;
    let s = counted(&a, "after munmap")?;
    kassert!(s.rss == 0 && a.committed_pages() == 0, "{s:?}");
    Ok(())
}
ktest!("mm.as.around", Safe, block);

/// A block the faulting VMA does not cover whole stays page by page: the VMA's start and end inside the block,
/// a block boundary, a guard page and a VMA of other rights in the block; MADV_RANDOM on a block.
fn edges() -> TestResult {
    let a = lazy()?;
    // Start and end inside one block.
    let m = a.sys_mmap(BASE + 3 * P, 5 * P, RW, AP | MAP_FIXED, 0).map_err(e)?;
    write_fault(&a, m + P)?;
    kassert!(filled(&a, BASE, BASE + BLOCK) == [4], "start/end inside: {:?}", filled(&a, BASE, BASE + BLOCK));
    // Across a block boundary: only the faulting page's block.
    let b = BASE + 4 * BLOCK;
    a.sys_mmap(b + 14 * P, 4 * P, RW, AP | MAP_FIXED, 0).map_err(e)?;
    write_fault(&a, b + 15 * P)?;
    kassert!(filled(&a, b, b + 2 * BLOCK) == [15], "across a boundary: {:?}", filled(&a, b, b + 2 * BLOCK));
    write_fault(&a, b + 17 * P)?;
    kassert!(filled(&a, b, b + 2 * BLOCK) == [15, 17]);
    // The VMA's end inside the block, its start not: the full first block fills, the cut one does not.
    let c = BASE + 6 * BLOCK;
    a.sys_mmap(c, BLOCK + 5 * P, RW, AP | MAP_FIXED, 0).map_err(e)?;
    write_fault(&a, c + BLOCK + 2 * P)?;
    write_fault(&a, c + 3 * P)?;
    kassert!(filled(&a, c, c + 2 * BLOCK) == span(0, 16).into_iter().chain([18]).collect::<Vec<_>>(), "{:?}", filled(&a, c, c + 2 * BLOCK));
    // A guard page (PROT_NONE) and a read-only VMA inside the block stay empty.
    let g = BASE + 8 * BLOCK;
    a.sys_mmap(g, BLOCK, RW, AP | MAP_FIXED, 0).map_err(e)?;
    a.sys_mprotect(g + 6 * P, P, 0).map_err(e)?;
    a.sys_mprotect(g + 10 * P, 2 * P, 1).map_err(e)?;
    write_fault(&a, g + 2 * P)?;
    write_fault(&a, g + 8 * P)?;
    kassert!(filled(&a, g, g + BLOCK) == [2, 8], "a block with a guard: {:?}", filled(&a, g, g + BLOCK));
    // MADV_RANDOM switches it off for its part; MADV_SEQUENTIAL and MADV_NORMAL keep it on.
    let r = BASE + 12 * BLOCK;
    a.sys_mmap(r, 3 * BLOCK, RW, AP | MAP_FIXED, 0).map_err(e)?;
    a.sys_madvise(r, BLOCK, madv::RANDOM).map_err(e)?;
    write_fault(&a, r + 10 * P)?;
    write_fault(&a, r + P)?;
    kassert!(filled(&a, r, r + BLOCK) == [1, 10], "MADV_RANDOM: {:?}", filled(&a, r, r + BLOCK));
    a.sys_madvise(r + BLOCK, BLOCK, madv::SEQUENTIAL).map_err(e)?;
    write_fault(&a, r + BLOCK)?;
    kassert!(filled(&a, r + BLOCK, r + 2 * BLOCK) == span(0, 16), "MADV_SEQUENTIAL keeps it");
    a.sys_madvise(r, 3 * BLOCK, madv::NORMAL).map_err(e)?;
    write_fault(&a, r + 2 * BLOCK)?;
    kassert!(filled(&a, r + 2 * BLOCK, r + 3 * BLOCK) == span(0, 16), "MADV_NORMAL turns it back on");
    kassert!(a.vmas().iter().filter(|v| v.start >= r && v.end <= r + 3 * BLOCK).count() == 1, "NORMAL merged the parts back");
    counted(&a, "edges")?;
    Ok(())
}
ktest!("mm.as.around_edges", Safe, edges);

/// Stacks (the main stack's VMA, MAP_STACK, MAP_GROWSDOWN), MADV_NOHUGEPAGE and shared anonymous memory stay
/// page by page; the heap and writable ELF segments (private anonymous memory) get the block.
fn kinds() -> TestResult {
    let a = lazy()?;
    let one = |a: &AddressSpace, base: u64, what: &str| -> TestResult {
        write_fault(a, base + 5 * P)?;
        kassert!(filled(a, base, base + BLOCK) == [5], "{what}: {:?}", filled(a, base, base + BLOCK));
        Ok(())
    };
    a.map_fixed(BASE, BLOCK, Prot::rw(), VmaKind::Stack).map_err(e)?;
    one(&a, BASE, "the main stack")?;
    a.sys_mmap(BASE + 2 * BLOCK, BLOCK, RW, AP | MAP_FIXED | MAP_STACK, 0).map_err(e)?;
    one(&a, BASE + 2 * BLOCK, "MAP_STACK")?;
    a.sys_mmap(BASE + 4 * BLOCK, BLOCK, RW, AP | MAP_FIXED | MAP_GROWSDOWN, 0).map_err(e)?;
    one(&a, BASE + 4 * BLOCK, "MAP_GROWSDOWN")?;
    a.sys_mmap(BASE + 6 * BLOCK, BLOCK, RW, AP | MAP_FIXED, 0).map_err(e)?;
    a.sys_madvise(BASE + 6 * BLOCK, BLOCK, madv::NOHUGEPAGE).map_err(e)?;
    one(&a, BASE + 6 * BLOCK, "MADV_NOHUGEPAGE")?;
    // MADV_HUGEPAGE clears VM_NOHUGEPAGE (MAP_STACK's too): the next write fault fills the block.
    a.sys_madvise(BASE + 2 * BLOCK, BLOCK, madv::HUGEPAGE).map_err(e)?;
    write_fault(&a, BASE + 2 * BLOCK + 9 * P)?;
    kassert!(filled(&a, BASE + 2 * BLOCK, BASE + 3 * BLOCK) == span(0, 16), "after MADV_HUGEPAGE");
    let s = a.sys_mmap(0, BLOCK, RW, MAP_SHARED | MAP_ANONYMOUS, 0).map_err(e)?;
    kassert!(s.is_multiple_of(BLOCK), "the shared mapping at {s:#x} is not block aligned");
    one(&a, s, "shared anonymous memory")?;
    let st = counted(&a, "kinds")?;
    kassert!(st.anon == 4 + 15 && st.shmem == 1, "{st:?}");
    // The heap.
    let heap = BASE + 8 * BLOCK;
    a.init_brk(heap);
    kassert!(a.sys_brk(heap + 20 * P) == heap + 20 * P, "brk did not grow");
    write_fault(&a, heap + 3 * P)?;
    kassert!(filled(&a, heap, heap + 2 * BLOCK) == span(0, 16), "the heap: {:?}", filled(&a, heap, heap + 2 * BLOCK));
    // A writable ELF segment (data and bss).
    let img = BASE + 12 * BLOCK;
    a.map_fixed(img, BLOCK, Prot::rw(), VmaKind::Image).map_err(e)?;
    write_fault(&a, img + 7 * P)?;
    kassert!(filled(&a, img, img + BLOCK) == span(0, 16), "the ELF segment: {:?}", filled(&a, img, img + BLOCK));
    counted(&a, "heap and image")?;
    Ok(())
}
ktest!("mm.as.around_kinds", Safe, kinds);

/// Free memory ends during the round (the ktest hook stands in for `alloc_zeroed_light` refusing): the fault
/// still succeeds with its own page, the round stops quietly, nothing is refused, the counters stay true.
fn short() -> TestResult {
    let a = lazy()?;
    let m = a.sys_mmap(BASE, 2 * BLOCK, RW, AP | MAP_FIXED, 0).map_err(e)?;
    a.test_fail_around_after(Some(3));
    let r = write_fault(&a, m + 8 * P);
    let got = filled(&a, m, m + BLOCK);
    a.test_fail_around_after(Some(0));
    let r2 = write_fault(&a, m + BLOCK + P);
    let got2 = filled(&a, m + BLOCK, m + 2 * BLOCK);
    a.test_fail_around_after(None);
    r?;
    r2?;
    kassert!(got == [0, 1, 2, 8], "three around pages, then the round ends: {got:?}");
    kassert!(got2 == [1], "no frame at all: only the faulting page {got2:?}");
    // The missing pages fill when touched (a copy, then a write fault with frames again).
    kassert!(a.read_u64(m + 12 * P).map_err(e)? == 0);
    write_fault(&a, m + 4 * P)?;
    kassert!(filled(&a, m, m + BLOCK) == span(0, 16), "{:?}", filled(&a, m, m + BLOCK));
    let s = counted(&a, "short rounds")?;
    kassert!(s.anon == 17 && a.committed_pages() == 32, "{s:?}");
    Ok(())
}
ktest!("mm.as.around_short", Safe, short);

/// After fork: the child's write fault fills the missing pages of its block but leaves a copy-on-write page it
/// shares with the parent alone (same frame, still read-only, still shared), charges nothing new on either side;
/// a MADV_WIPEONFORK part comes empty to the child and gets the block. A VMA cut below a block by mremap
/// (shrink) or mprotect (a 3-page part) stays page by page.
fn fork() -> TestResult {
    let a = lazy()?;
    let m = a.sys_mmap(BASE, 2 * BLOCK, RW, AP | MAP_FIXED, 0).map_err(e)?;
    a.sys_madvise(m + BLOCK, BLOCK, madv::WIPEONFORK).map_err(e)?;
    a.handle_user_fault(m + 2 * P, PF_U).map_err(e)?;
    a.write_u64(m + 2 * P, 0xA2).map_err(e)?;
    a.handle_user_fault(m + BLOCK + 2 * P, PF_U).map_err(e)?;
    a.write_u64(m + BLOCK + 2 * P, 0xB2).map_err(e)?;
    kassert!(filled(&a, m, m + 2 * BLOCK) == [2, 18], "the parent's two pages: {:?}", filled(&a, m, m + 2 * BLOCK));
    let (shared, _) = paging::translate(a.root(), m + 2 * P).ok_or("parent page 2 missing")?;
    let b = a.fork().map_err(e)?;
    let (ca, cb) = (a.committed_pages(), b.committed_pages());
    kassert!(filled(&b, m, m + 2 * BLOCK) == [2], "the child got its COW page, not the wiped one");
    write_fault(&b, m + 9 * P)?;
    kassert!(filled(&b, m, m + BLOCK) == span(0, 16), "the child's block: {:?}", filled(&b, m, m + BLOCK));
    let (pb, fb) = paging::translate(b.root(), m + 2 * P).ok_or("child page 2 missing")?;
    kassert!(pb == shared && !fb.contains(paging::PageFlags::WRITABLE) && share::others(shared) == 1,
        "the round touched the shared page: frame {pb:#x}/{shared:#x}, flags {fb:?}, others {}", share::others(shared));
    kassert!(filled(&a, m, m + BLOCK) == [2], "the parent's block changed: {:?}", filled(&a, m, m + BLOCK));
    kassert!(a.read_u64(m + 2 * P).map_err(e)? == 0xA2 && b.read_u64(m + 2 * P).map_err(e)? == 0xA2, "contents");
    write_fault(&b, m + BLOCK + 5 * P)?;
    kassert!(filled(&b, m + BLOCK, m + 2 * BLOCK) == span(0, 16), "the wiped part: {:?}", filled(&b, m + BLOCK, m + 2 * BLOCK));
    kassert!(b.read_u64(m + BLOCK + 2 * P).map_err(e)? == 0 && a.read_u64(m + BLOCK + 2 * P).map_err(e)? == 0xB2);
    kassert!(a.committed_pages() == ca && b.committed_pages() == cb, "commit {}/{} after {ca}/{cb}", a.committed_pages(), b.committed_pages());
    counted(&a, "fork parent")?;
    counted(&b, "fork child")?;
    drop(b);
    kassert!(share::exclusive(shared), "the child's share did not come back");
    // mremap shrinks a block-sized VMA in place to 3 pages; mprotect leaves a 3-page part between two others.
    let r = BASE + 4 * BLOCK;
    a.sys_mmap(r, BLOCK, RW, AP | MAP_FIXED, 0).map_err(e)?;
    kassert!(a.sys_mremap(r, BLOCK, 3 * P, 0, 0).map_err(e)? == r, "the shrink moved");
    write_fault(&a, r + P)?;
    kassert!(filled(&a, r, r + BLOCK) == [1], "after mremap: {:?}", filled(&a, r, r + BLOCK));
    let q = BASE + 6 * BLOCK;
    a.sys_mmap(q, BLOCK, RW, AP | MAP_FIXED, 0).map_err(e)?;
    a.sys_mprotect(q + 4 * P, 3 * P, 1).map_err(e)?;
    a.sys_mprotect(q + 4 * P, 3 * P, RW).map_err(e)?;
    let parts = a.vmas().iter().filter(|v| v.start >= q && v.end <= q + BLOCK).count();
    write_fault(&a, q + 5 * P)?;
    // mprotect back to RW merges the parts again (Linux does too), so the block is whole once more.
    kassert!(parts == 1 && filled(&a, q, q + BLOCK) == span(0, 16), "merged back: {parts} parts, {:?}", filled(&a, q, q + BLOCK));
    let t = BASE + 8 * BLOCK;
    a.sys_mmap(t, BLOCK, RW, AP | MAP_FIXED, 0).map_err(e)?;
    a.sys_mprotect(t, 4 * P, 1).map_err(e)?;
    a.sys_mprotect(t + 7 * P, 9 * P, 1).map_err(e)?;
    write_fault(&a, t + 5 * P)?;
    kassert!(filled(&a, t, t + BLOCK) == [5], "a 3-page part: {:?}", filled(&a, t, t + BLOCK));
    counted(&a, "splits")?;
    Ok(())
}
ktest!("mm.as.around_fork", Safe, fork);

/// Numbers for the Mini-PC, never judged here: a writer going front to back through 4 MiB of fresh memory, once
/// with the fault-around, once page by page (MADV_RANDOM), once with huge pages (MM-13). Kernel side only (no #PF
/// entry, no user copy).
fn cost() -> TestResult {
    let a = lazy()?;
    let pages = 1024;
    // MM-13: the 4 MiB mapping is 2 MiB aligned and would take huge pages; without free 2 MiB blocks the faults
    // fall back to fault-around.
    a.test_fail_huge(true);
    let run = |advice: Option<u64>| -> Result<(u64, u64), String> {
        let m = a.sys_mmap(0, pages * P, RW, AP, 0).map_err(e)?;
        if let Some(x) = advice {
            a.sys_madvise(m, pages * P, x).map_err(e)?;
        }
        let (t0, mut faults) = (time::now_ns(), 0);
        for i in 0..pages {
            if paging::translate(a.root(), m + i * P).is_none() {
                write_fault(&a, m + i * P)?;
                faults += 1;
            }
        }
        let ns = time::now_ns() - t0;
        a.sys_munmap(m, pages * P).map_err(e)?;
        Ok((faults, ns))
    };
    let (fa, na) = run(None)?;
    let (fp, np) = run(Some(madv::RANDOM))?;
    a.test_fail_huge(false);
    let (fh, nh) = run(None)?;
    // An unaligned mapping has a cut block at each end (page by page).
    kassert!(fp == pages && fa <= pages / 16 + 30, "faults: {fa} with fault-around, {fp} page by page");
    // With huge pages only when 2 MiB blocks were free (light allocation): at most the 4 KiB way.
    kassert!(fh <= fa, "faults: {fh} with huge pages, {fa} with fault-around");
    klog::event!(Info, "mm", "ktest.around_cost", pages = pages, around_faults = fa, around_ns_per_page = na / pages,
        page_faults = fp, page_ns_per_page = np / pages, huge_faults = fh, huge_ns_per_page = nh / pages);
    Ok(())
}
ktest!("mm.as.around_cost", Safe, cost);
