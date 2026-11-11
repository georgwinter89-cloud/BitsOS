//! Page faults, filling, copy-on-write and the user copies (MM-5a, MM-5b).
//!
//! A missing page gets its frame on the first touch: zeroed for anonymous memory, the page-cache frame (or a
//! private copy for a private write) for a file, the device's page for a device mapping. A write to a present
//! read-only page of a writable VMA is one of: copy-on-write after a fork (the program's own frame shared with
//! another space: copied, or taken back writable once this space is its last holder; Linux `do_wp_page`), the
//! first write to a private file page (copied), the dirty mark of a tracked shared file page or device page.

use super::huge::{self, HUGE};
use super::{keep_read_only, page_flags, read_only, AsCore, Class, Inner, Owner, CACHE_BITS, UNCONFIRMED};
use crate::fault::{self, Access, FaultReason, SpuriousRun};
use crate::vma::{FileRef, Prot, Vma, VmaKind};
use crate::{share, MmError, PAGE};
use alloc::vec::Vec;
use core::sync::atomic::Ordering::Relaxed;
use hal::paging::{self, tlb, PageFlags, PageSize, Released};
use ksync::MutexGuard;
use vfs::pagecache::{MapHint, PageCache, PageFault};

klog::counter!(FAULT_FILLS, "mm.as.fault_fills");
klog::counter!(FAULT_SPURIOUS, "mm.as.fault_spurious");
klog::counter!(FAULT_REFUSED, "mm.as.fault_refused");
klog::counter!(COPY_FILLS, "mm.as.copy_fills");
klog::counter!(SPARE_UNUSED, "mm.as.spare_unused");
klog::counter!(FILE_FILLS, "mm.as.file_fills");
klog::counter!(FILE_COPIES, "mm.as.file_copies");
klog::counter!(pub(crate) COW_COPIES, "mm.as.cow_copies");
klog::counter!(pub(crate) COW_REUSED, "mm.as.cow_reused");
klog::counter!(DEVICE_FILLS, "mm.as.device_fills");
klog::counter!(AROUND_FILLS, "mm.as.around_fills");
klog::counter!(AROUND_SHORT, "mm.as.around_short");
// BL-4: file pages a read fault mapped beside its own (`file_fault_around`).
klog::counter!(FILE_AROUND, "mm.as.file_around");
/// Pages per fault-around block: 64 KiB, Linux's default `fault_around_bytes` (linux-6.6/mm/memory.c:4445).
const AROUND_PAGES: u64 = 16;

/// MM-13: the 2 MiB block of one fault, made without the space's lock.
enum Block {
    /// Not asked for yet.
    Untried,
    /// Wanted for a VMA with (true) or without MADV_HUGEPAGE: `fault_locked` asks for it without the lock.
    Wanted(bool),
    /// Zeroed, not mapped yet.
    Made(u64),
    /// None was free (or the VMA no longer wants one): 4 KiB pages.
    Refused,
    /// Mapped: the fault is served.
    Mapped,
}

/// What a user copy needs from a page.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Need {
    Read,
    Write,
    /// The loader: any page of a mapping that is not PROT_NONE, read-only ones too.
    Load,
}

/// The bytes from `a` to the end of its page, at most `left` (a copy by a thread that does not own the root goes
/// page by page with interrupts off, `tlb::without_lazy`).
fn page_chunk(a: u64, left: usize) -> usize {
    ((PAGE - (a & (PAGE - 1))) as usize).min(left)
}

pub(crate) fn file_fault(e: PageFault) -> FaultReason {
    match e {
        PageFault::Bus | PageFault::Io => FaultReason::Bus,
        PageFault::NoMemory => FaultReason::NoMemory,
    }
}

/// BL-4: the page cache behind a plain file mapping (not a device, not a SysV segment's or another backing's).
fn file_cache(f: &FileRef) -> Option<&PageCache> {
    if f.device.is_some() {
        return None;
    }
    f.backing.file()?.inode().page_cache()
}

/// BL-4: read-ahead for a fault on file page `pgoff` of `v` (linux-7/mm/filemap.c:3555 `filemap_fault`: sync or
/// async mmap read-ahead before the page is taken; `PageCache::fault_ra`), with the VMA's hints: MADV_RANDOM,
/// MADV_SEQUENTIAL, PROT_EXEC and its file range.
fn file_ra(v: &Vma, f: &FileRef, pgoff: u64) {
    let Some(c) = file_cache(f) else { return };
    let first = f.pgoff;
    let h = MapHint { random: v.flags.rand_read, sequential: v.flags.seq_read, exec: v.prot.exec, first, end: first + (v.end - v.start) / PAGE };
    c.fault_ra(pgoff, &h);
}

/// BL-4 (fixer): whether a fault at `page` of `v` reads the file (a plain file VMA whose page is not cached yet):
/// that read-ahead then runs without the space's lock (`file_ra_unlocked`).
fn file_ra_reads(v: &Vma, page: u64) -> bool {
    let (Some(f), Some(pgoff)) = (&v.file, v.pgoff_at(page)) else { return false };
    file_cache(f).is_some_and(|c| c.reads_for_fault(pgoff))
}

/// BL-4 (fixer): the fault's sync read-ahead before the space's lock is taken again (linux-7/mm/filemap.c
/// `maybe_unlock_mmap_for_io`: Linux drops mmap_lock for the read and retries the fault), so the other threads
/// of the program fault and map meanwhile (B4). `v` is the fault's own copy of the VMA: its file stays referenced
/// even if the range is unmapped meanwhile; the retried fault looks at the space again.
fn file_ra_unlocked(v: &Vma, page: u64) {
    if let (Some(f), Some(pgoff)) = (&v.file, v.pgoff_at(page)) {
        file_ra(v, f, pgoff);
    }
}

/// BL-4 (fixer): MADV_WILLNEED on file VMA `v` for [s, e) (linux-7/mm/madvise.c:290 `madvise_willneed`:
/// `vfs_fadvise(POSIX_FADV_WILLNEED)` on the mapped file range): queues the range's read-ahead, never waits.
pub(super) fn file_will_need(v: &Vma, s: u64, e: u64) {
    let (Some(f), Some(pgoff)) = (&v.file, v.pgoff_at(s)) else { return };
    if let Some(c) = file_cache(f) {
        c.will_need(pgoff.saturating_mul(PAGE), e.saturating_sub(s));
    }
}

/// Who owns the frame a VMA maps at a page (for handing an old frame back).
fn owner_of(v: &Vma) -> Owner {
    match &v.file {
        Some(f) if f.device.is_some() => Owner::Device(Some(f.backing.clone())),
        Some(f) => Owner::File(f.backing.clone(), v.kind == VmaKind::Anonymous),
        None => Owner::Anon,
    }
}

/// Whether a write fault in `v` fills its 64 KiB block (MM-12): private anonymous memory charged to the commit
/// (heap, ELF data and bss, mmap); not a stack (the main stack's VMA, MAP_GROWSDOWN, and MAP_STACK through
/// VM_NOHUGEPAGE: they grow a page at a time from the top); not where the program asked for page-exact memory
/// (MADV_RANDOM, MADV_NOHUGEPAGE: the block stands in for Linux's huge pages, which NOHUGEPAGE switches off).
/// Shared anonymous memory, file and device VMAs have a file. A guard range is PROT_NONE: no write fault.
fn around(v: &Vma) -> bool {
    let f = v.flags;
    v.file.is_none() && f.account && v.kind != VmaKind::Stack && !f.growsdown && !f.nohugepage && !f.rand_read
}

impl AsCore {
    /// A zeroed frame at `va` with the rights of `prot`: `spare` when the caller made one, else a new one.
    fn fill_page(&self, va: u64, prot: Prot, spare: &mut Option<u64>) -> Result<(), FaultReason> {
        let f = match spare.take() {
            Some(f) => f,
            None => mm_core::alloc_zeroed(mm_core::Kind::User).ok_or(FaultReason::NoMemory)?,
        };
        self.anon_hint.store(true, Relaxed);
        if paging::map_user_4k(self.root, va, f, page_flags(prot), &mut || self.alloc_table()).is_err() {
            super::free_user(f);
            return Err(FaultReason::NoMemory);
        }
        self.ctr.add(Class::Anon, 1);
        Ok(())
    }

    /// Gives the missing page at `va` of VMA `v` its frame: zeroed for anonymous memory; for a file VMA the
    /// page-cache frame (shared, or private and only read) or a private copy of it (a private write); for a
    /// device VMA the device's page with its memory type.
    pub(crate) fn fill_at(&self, va: u64, v: &Vma, write: bool) -> Result<(), FaultReason> {
        self.fill_at_with(va, v, write, &mut None, true)
    }

    /// `fill_at`, an anonymous page taking `spare` (a zeroed frame made before the lock) when there is one;
    /// `ra`: a file page's read-ahead still has to run (false: the fault ran it before the lock, `fault_with`).
    fn fill_at_with(&self, va: u64, v: &Vma, write: bool, spare: &mut Option<u64>, ra: bool) -> Result<(), FaultReason> {
        let (Some(f), Some(pgoff)) = (&v.file, v.pgoff_at(va)) else { return self.fill_page(va, v.prot, spare) };
        if let Some(d) = &f.device {
            let pg = d.fault(pgoff, write).map_err(file_fault)?;
            let fl = PageFlags::user(pg.writable && v.prot.write, v.prot.exec).with_cache(pg.cache);
            paging::map_user_4k(self.root, va, pg.phys, fl, &mut || self.alloc_table()).map_err(|_| FaultReason::NoMemory)?;
            self.ctr.add(Class::Device, 1);
            DEVICE_FILLS.inc();
            return Ok(());
        }
        if ra {
            file_ra(v, f, pgoff);
        }
        if f.shared || !write {
            let phys = f.backing.get_page(pgoff, write && f.shared).map_err(file_fault)?;
            let ro = !f.shared || (f.backing.track_dirty() && !write);
            let fl = if ro { read_only(v.prot) } else { page_flags(v.prot) };
            if paging::map_user_4k(self.root, va, phys, fl, &mut || self.alloc_table()).is_err() {
                f.backing.put_page(phys);
                return Err(FaultReason::NoMemory);
            }
            let c = Class::of(phys, &owner_of(v));
            self.ctr.add(c, 1);
            if c.cached() {
                share::map_file(phys);
            }
            FILE_FILLS.inc();
            return Ok(());
        }
        // A private write: copy the file's page now (the pin is only held for the copy, never mapped).
        let src = f.backing.get_page(pgoff, false).map_err(file_fault)?;
        let copy = mm_core::alloc_copy(src, mm_core::Kind::User);
        f.backing.put_page(src);
        let copy = copy.ok_or(FaultReason::NoMemory)?;
        if paging::map_user_4k(self.root, va, copy, page_flags(v.prot), &mut || self.alloc_table()).is_err() {
            super::free_user(copy);
            return Err(FaultReason::NoMemory);
        }
        self.ctr.add(Class::Anon, 1);
        FILE_COPIES.inc();
        Ok(())
    }

    /// Puts `new` with `fl` where the present page `page` was, in one step (no table freed or allocated), and
    /// hands the old frame to its owner once the shootdown is confirmed (or queues it for the drop).
    fn swap_page(&self, g: &mut Inner, page: u64, v: &Vma, new: u64, fl: PageFlags) -> Result<(), FaultReason> {
        let (old, o) = paging::replace_user_4k(self.root, page, new, fl).map_err(|_| FaultReason::Repeated)?;
        if !o.confirmed() {
            UNCONFIRMED.inc();
            klog::event!(Warn, "mm", "as.shootdown_unconfirmed", root = klog::Hex(self.root), start = klog::Hex(page),
                end = klog::Hex(page + PAGE), cpus = o.unanswered, resends = o.resends, not_ready = o.not_ready;
                "a CPU did not answer the TLB shootdown of a copy-on-write: the old frame stays out of use until the address space ends");
        }
        let owner = owner_of(v);
        let (was, now) = (Class::of(old, &owner), Class::of(new, &owner));
        self.ctr.swap(was, now);
        if was.cached() {
            share::unmap_file(old);
        }
        if now.cached() {
            share::map_file(new);
        }
        let r = Released { phys: old, size: PageSize::Size4K, table: false, confirmed: o.confirmed() };
        Self::give_back(&r, &owner, &mut g.deferred, &mut g.lost);
        Ok(())
    }

    /// The private copy of the present page `page` (frame `old`) with the VMA's rights: a fork's shared frame or
    /// a page-cache frame of a private file mapping.
    pub(crate) fn copy_into_place(&self, g: &mut Inner, page: u64, v: &Vma, old: u64) -> Result<(), FaultReason> {
        let copy = mm_core::alloc_copy(old, mm_core::Kind::User).ok_or(FaultReason::NoMemory)?;
        if let Err(e) = self.swap_page(g, page, v, copy, page_flags(v.prot)) {
            super::free_user(copy);
            return Err(e);
        }
        if v.file.is_some() { FILE_COPIES.inc() } else { COW_COPIES.inc() }
        Ok(())
    }

    /// A write to the present read-only page `page` (frame `phys`) of VMA `v`, which allows writes: the device's
    /// dirty mark, a tracked shared file's dirty mark, or copy-on-write (a frame only this space holds is simply
    /// made writable again).
    pub(crate) fn write_present(&self, g: &mut Inner, page: u64, v: &Vma, phys: u64) -> Result<(), FaultReason> {
        // MM-13: a 2 MiB leaf becomes writable whole when this space alone holds its frames, else it is split and
        // the 4 KiB page is handled below.
        if let Some(l) = paging::translate_leaf(self.root, page).filter(|l| l.size == PageSize::Size2M) {
            if self.huge_write(g, page, v, l)? {
                return Ok(());
            }
            let phys = paging::translate(self.root, page).ok_or(FaultReason::Repeated)?.0;
            return self.write_small(g, page, v, phys);
        }
        self.write_small(g, page, v, phys)
    }

    /// `write_present` of a 4 KiB leaf.
    fn write_small(&self, g: &mut Inner, page: u64, v: &Vma, phys: u64) -> Result<(), FaultReason> {
        // Only rights are added here (same frame): no shootdown round (MM-tlb2, `paging::grant_user`); a change of
        // the memory type falls back to one.
        let set = |fl: PageFlags| paging::grant_user(self.root, page, PAGE, &mut |_| Some(fl)).map(|_| ()).map_err(|_| FaultReason::Repeated);
        if let (Some(f), Some(pgoff)) = (&v.file, v.pgoff_at(page)) {
            if let Some(d) = &f.device {
                let pg = d.fault(pgoff, true).map_err(file_fault)?;
                if !pg.writable {
                    return Err(FaultReason::ReadOnly);
                }
                let fl = PageFlags::user(true, v.prot.exec).with_cache(pg.cache);
                return if pg.phys == phys { set(fl) } else { self.swap_page(g, page, v, pg.phys, fl) };
            }
            if f.shared {
                let p = f.backing.get_page(pgoff, true).map_err(file_fault)?;
                f.backing.put_page(p);
                return set(page_flags(v.prot));
            }
        }
        if mm_core::is_user_frame(phys) && share::exclusive(phys) {
            COW_REUSED.inc();
            return set(page_flags(v.prot));
        }
        self.copy_into_place(g, page, v, phys)
    }

    /// Gives every page of [start, end) that lies in an accessible VMA a frame (eager fill, MAP_POPULATE,
    /// and before an mprotect makes a range accessible in eager mode: then with the VMA's current rights).
    /// MM-13: an aligned 2 MiB of a VMA that may have huge pages takes one (Linux's populate write-faults a
    /// writable private mapping, `populate_vma_page_range`, and so gets THP).
    pub(crate) fn populate(&self, g: &mut Inner, start: u64, end: u64, only_accessible: bool) -> Result<(), FaultReason> {
        let mut at = start;
        while at < end {
            let Some(v) = g.space.tree.next_from(at).filter(|v| v.start < end).cloned() else { break };
            at = v.end;
            if only_accessible && !v.prot.accessible() {
                continue;
            }
            let mut va = v.start.max(start);
            while va < v.end.min(end) {
                if paging::translate(self.root, va).is_none() {
                    if va.is_multiple_of(HUGE)
                        && va + HUGE <= end
                        && let Some(base) = self.huge_base(g, &v, va)
                        && self.fill_huge_now(g, base, &v)
                    {
                        va += HUGE;
                        continue;
                    }
                    match self.fill_at(va, &v, false) {
                        // A file page past the end of the file stays empty (it would be SIGBUS on access).
                        Err(FaultReason::Bus) => break,
                        r => r?,
                    }
                }
                va += PAGE;
            }
        }
        Ok(())
    }

    /// Serves a user page fault at `addr` with the #PF `error` code: fills the page (or re-syncs a stale entry,
    /// or copies on write) when the VMA allows the access; otherwise the reason for the V2 fault message. A
    /// caller outside a thread's fault loop (futex, ktests): every spurious fault is the first of its run.
    pub fn handle_user_fault(&self, addr: u64, error: u64) -> Result<(), FaultReason> {
        self.handle_thread_fault(addr, error, &mut SpuriousRun::default())
    }

    /// As `handle_user_fault`, for the fault loop of one thread: `run` is that thread's own run of spurious
    /// faults (`SpuriousRun`), so threads racing for one fresh page never count against each other.
    pub fn handle_thread_fault(&self, addr: u64, error: u64, run: &mut SpuriousRun) -> Result<(), FaultReason> {
        klog::trace::page_fault(addr);
        let r = self.fault_locked(addr, error, run);
        if let Err(why) = r {
            FAULT_REFUSED.inc();
            if why == FaultReason::NoMemory {
                klog::event!(Warn, "mm", "as.fault_no_memory", addr = klog::Hex(addr);
                    "no frame for a committed user page: the kernel's own memory ate into POOL_KEEP");
            } else if why == FaultReason::Repeated {
                klog::event!(Warn, "mm", "as.fault_repeated", addr = klog::Hex(addr), error = klog::Hex(error), cpu = hal::percpu::this_cpu();
                    "a thread faulted on a valid page again and again, flushed each time: the program ends instead of looping");
            } else {
                klog::event!(Debug, "mm", "as.fault_refused", addr = klog::Hex(addr), error = klog::Hex(error), reason = why.text());
            }
        }
        r
    }

    /// MM-tlb2: a fault on a missing page zeroes its frame before it takes the space's lock (Linux's
    /// do_anonymous_page allocates before the page-table lock too, linux-6.6/mm/memory.c:4113, lock at 4133),
    /// so threads of one program that fault in fresh buffers at the same time (Cobalt's decoders) hold the lock
    /// only for the mapping: with the zeroing inside, they queued on it and every hand-over woke a halted CPU
    /// with a reschedule IPI (200k/s on 4 CPUs in `mm.tlb.cobalt`, 7.5k/s now). An unused frame goes back.
    fn fault_locked(&self, addr: u64, error: u64, run: &mut SpuriousRun) -> Result<(), FaultReason> {
        if addr >= hal::USER_TOP {
            return Err(FaultReason::KernelAddress);
        }
        // Checker finding (fixer): the VMA is not known before the lock, so the frame is made only while missing
        // pages keep turning out anonymous; an unused one turns the hint off until the next anonymous fill.
        let want = error & 1 == 0 && self.anon_hint.load(Relaxed);
        let mut spare = if want { mm_core::alloc_zeroed(mm_core::Kind::User) } else { None };
        // MM-13: a write fault that may take a 2 MiB page asks for its block once (`None`): zeroed here, without
        // the lock, then the fault looks again (another thread may have served the range meanwhile).
        let mut block = Block::Untried;
        let r = loop {
            match self.fault_with(addr, error, run, &mut spare, &mut block, false) {
                Some(r) => break r,
                None => {
                    let madvised = matches!(block, Block::Wanted(true));
                    block = crate::thp::fault_block(madvised, true).map_or(Block::Refused, Block::Made);
                }
            }
        };
        if let Block::Made(f) = block {
            huge::free_block(f);
            crate::thp::UNUSED.inc();
        }
        if let Some(f) = spare {
            super::free_user(f);
            // A huge page took the fault: the next missing page is likely anonymous still.
            if !matches!(block, Block::Mapped) {
                self.anon_hint.store(false, Relaxed);
                SPARE_UNUSED.inc();
            }
        }
        r
    }

    /// One look at the fault under the lock; None: a 2 MiB block is wanted first (`fault_locked`).
    /// `ra_done`: this fault's file read-ahead ran already (`file_ra_unlocked`).
    fn fault_with(&self, addr: u64, error: u64, run: &mut SpuriousRun, spare: &mut Option<u64>, block: &mut Block, ra_done: bool) -> Option<Result<(), FaultReason>> {
        let mut g = self.inner.lock();
        let vma = g.space.tree.find(addr).cloned();
        let access = match fault::check(error, vma.as_ref().map(|v| v.prot)) {
            Ok(a) => a,
            Err(e) => return Some(Err(e)),
        };
        let Some(v) = vma else { return Some(Err(FaultReason::NotMapped)) };
        let page = addr & !(PAGE - 1);
        if !ra_done && file_ra_reads(&v, page) && paging::translate(self.root, page).is_none() {
            drop(g);
            file_ra_unlocked(&v, page);
            return self.fault_with(addr, error, run, spare, block, true);
        }
        if access == Access::Write
            && paging::translate(self.root, page).is_none()
            && let Some(base) = self.huge_base(&g, &v, page)
        {
            match core::mem::replace(block, Block::Refused) {
                Block::Untried if g.huge_fail => crate::thp::FAULT_FALLBACK.inc(),
                Block::Untried => {
                    *block = Block::Wanted(v.flags.hugepage);
                    return None;
                }
                Block::Made(f) => match self.map_huge(&mut g, base, &v, f) {
                    Ok(()) => {
                        *block = Block::Mapped;
                        run.reset();
                        FAULT_FILLS.inc();
                        return Some(Ok(()));
                    }
                    Err(f) => {
                        huge::free_block(f);
                        crate::thp::FAULT_FALLBACK.inc();
                    }
                },
                Block::Wanted(_) | Block::Refused | Block::Mapped => {}
            }
        }
        Some(self.fault_page(&mut g, addr, error, page, &v, access, run, spare, ra_done))
    }

    /// The fault on the 4 KiB page `page` (MM-5a..MM-12).
    #[allow(clippy::too_many_arguments)]
    fn fault_page(&self, g: &mut Inner, addr: u64, error: u64, page: u64, v: &Vma, access: Access, run: &mut SpuriousRun, spare: &mut Option<u64>, ra_done: bool) -> Result<(), FaultReason> {
        let Some(l) = paging::translate_leaf(self.root, page) else {
            run.reset();
            self.fill_at_with(page, &v, access == Access::Write, spare, !ra_done)?;
            FAULT_FILLS.inc();
            // MM-13: the block stands in for a huge page, so it follows the THP switches too (mode, PR_SET_THP_DISABLE:
            // Linux then fills page by page); it stays where no huge page fits (a cut 2 MiB, no free block).
            if access == Access::Write && around(&v) && !self.thp_disabled() && crate::thp::mode_allows(v.flags.hugepage, v.flags.nohugepage) {
                self.fault_around(g, page, &v);
            } else if access != Access::Write {
                self.file_fault_around(page, &v);
            }
            return Ok(());
        };
        let (phys, fl) = (l.phys, l.flags);
        // A write to a page kept read-only on purpose: copy-on-write, a private file copy, a dirty mark.
        if access == Access::Write && fl.contains(PageFlags::USER) && !fl.contains(PageFlags::WRITABLE) {
            run.reset();
            return self.write_present(g, page, &v, phys);
        }
        // Present: another thread filled it while this one waited for the lock, a translation this CPU cached
        // before the entry changed, or an entry behind its VMA.
        let n = run.note(self.root, page, time::now_ns())?;
        FAULT_SPURIOUS.inc();
        let ok = fl.contains(PageFlags::USER) && (access != Access::Write || fl.contains(PageFlags::WRITABLE)) && (access != Access::Fetch || !fl.contains(PageFlags::NX));
        if n > 1 {
            klog::event!(Debug, "mm", "as.fault_spurious", addr = klog::Hex(addr), error = klog::Hex(error), in_row = n, entry_ok = ok);
        }
        if ok {
            // invlpg drops this address in the current PCID, which is this space's on this CPU (no KPTI).
            paging::flush_page(page);
        } else {
            let base = if !fl.contains(PageFlags::WRITABLE) && keep_read_only(&v, phys, l.size) { read_only(v.prot) } else { page_flags(v.prot) };
            let want = PageFlags(base.0 | (fl.0 & CACHE_BITS));
            // The whole leaf (MM-13: a 2 MiB one too, never a part of it).
            if let Ok((_, o)) = paging::grant_user(self.root, l.page_virt, l.size.bytes(), &mut |_| Some(want))
                && !o.confirmed()
            {
                UNCONFIRMED.inc();
                klog::event!(Warn, "mm", "as.shootdown_unconfirmed", root = klog::Hex(self.root), start = klog::Hex(page),
                    end = klog::Hex(page + PAGE), cpus = o.unanswered, resends = o.resends, not_ready = o.not_ready;
                    "a CPU did not answer the TLB shootdown of a fault repair: its pending IPI flushes before it runs user code again");
            }
        }
        Ok(())
    }

    /// MM-12 (FD-1): after a write fault filled a page of a VMA `around` accepts, the other missing pages of its
    /// aligned 64 KiB block get zeroed frames too when the whole block lies inside the VMA (Linux's rule for a
    /// huge page, `transhuge_vma_suitable`, linux-6.6/include/linux/huge_mm.h:100: small or cut VMAs, a heap of a
    /// few pages, the ends of a mapping stay page by page, as on Linux), so a program writing a fresh buffer front to
    /// back pays the fault entry, the space lock and the walk once per block instead of per page (Chromium's
    /// software compositor: a new 13.8 MB layer per video frame). Linux reaches the same with transparent huge
    /// pages (CachyOS: `always`, 2 MiB at a time); its own fault-around (`do_fault_around`, memory.c:4504) is
    /// for file pages. Frames come from `alloc_zeroed_light`: only memory free now, never reclaim or the OOM
    /// path for pages nobody touched; the first refusal ends the round quietly (the faulting page is in).
    /// Nothing is charged: `around` takes only VM_ACCOUNT VMAs, whose every page the mapping charged.
    /// No TLB flush: every entry made here was not present (x86 caches no not-present translation).
    fn fault_around(&self, g: &Inner, page: u64, v: &Vma) {
        let (lo, hi) = (page & !(AROUND_PAGES * PAGE - 1), (page & !(AROUND_PAGES * PAGE - 1)) + AROUND_PAGES * PAGE);
        if lo < v.start || hi > v.end {
            return;
        }
        // One walk for the block (it lies in one page table): bit i = page lo + i*PAGE is present.
        let mut present = 0u32;
        paging::for_each_leaf(self.root, lo, hi, &mut |l| {
            let (s, e) = (l.virt.max(lo), (l.virt + l.size.bytes()).min(hi));
            let mut a = s;
            while a < e {
                present |= 1 << ((a - lo) / PAGE);
                a += PAGE;
            }
        });
        present |= 1 << ((page - lo) / PAGE);
        let mut n = 0;
        let mut short = false;
        let mut va = lo;
        while va < hi {
            if present & 1 << ((va - lo) / PAGE) == 0 {
                let mut f = match g.around_fail_after {
                    Some(k) if n >= k => None,
                    _ => mm_core::alloc_zeroed_light(mm_core::Kind::User),
                };
                if f.is_none() || self.fill_page(va, v.prot, &mut f).is_err() {
                    short = true;
                    break;
                }
                n += 1;
            }
            va += PAGE;
        }
        AROUND_FILLS.add(n);
        if short {
            // Handled: the faulting page is in, the rest fills when touched (by the normal, reclaiming path).
            AROUND_SHORT.inc();
            klog::event!(Debug, "mm", "as.around_short", addr = klog::Hex(page), filled = n);
        }
    }

    /// BL-4: after a read fault filled a page of a file VMA, the other missing pages of its aligned 64 KiB block
    /// (inside the VMA) that the page cache holds now are mapped too (linux-7/mm/memory.c:5934 `do_fault_around`
    /// with `fault_around_bytes` 64 KiB, mm/filemap.c:3899 `filemap_map_pages`): no I/O, no wait, pages not
    /// cached or the read-ahead mark are left to their own fault (`PageCache::ready_for_map`). Each page gets what
    /// a read fault of it gets (`fill_at_with`: read-only when private or dirty-tracked) and its pin. A page
    /// reclaimed between the look and the pin is read as its own fault would read it (rare: reclaim takes only
    /// pages nobody holds). No TLB flush: every entry made here was not present.
    fn file_fault_around(&self, page: u64, v: &Vma) {
        let Some(f) = &v.file else { return };
        let Some(c) = file_cache(f) else { return };
        let lo = (page & !(AROUND_PAGES * PAGE - 1)).max(v.start);
        let hi = ((page & !(AROUND_PAGES * PAGE - 1)) + AROUND_PAGES * PAGE).min(v.end);
        let mut present = 0u32;
        paging::for_each_leaf(self.root, lo, hi, &mut |l| {
            let (s, e) = (l.virt.max(lo), (l.virt + l.size.bytes()).min(hi));
            let mut a = s;
            while a < e {
                present |= 1 << ((a - lo) / PAGE);
                a += PAGE;
            }
        });
        present |= 1 << ((page - lo) / PAGE);
        let ro = !f.shared || f.backing.track_dirty();
        let fl = if ro { read_only(v.prot) } else { page_flags(v.prot) };
        let owner = owner_of(v);
        let mut n = 0;
        let mut va = lo;
        while va < hi {
            let Some(pgoff) = v.pgoff_at(va) else { break };
            if present & 1 << ((va - lo) / PAGE) == 0 && c.ready_for_map(pgoff) {
                let Ok(phys) = f.backing.get_page(pgoff, false) else { break };
                if paging::map_user_4k(self.root, va, phys, fl, &mut || self.alloc_table()).is_err() {
                    f.backing.put_page(phys);
                    break;
                }
                let k = Class::of(phys, &owner);
                self.ctr.add(k, 1);
                if k.cached() {
                    share::map_file(phys);
                }
                n += 1;
            }
            va += PAGE;
        }
        FILE_AROUND.add(n);
    }

    /// Makes [addr, addr+len) ready for a copy: fills missing pages the VMAs allow (file pages from their file,
    /// a private copy for a write), breaks copy-on-write for writes (and for the loader's forced writes), refuses
    /// the rest.
    pub(crate) fn ensure(&self, g: &mut Inner, addr: u64, len: usize, need: Need) -> Result<(), MmError> {
        if len == 0 {
            return Ok(());
        }
        let end = addr.checked_add(len as u64).filter(|&e| e <= hal::USER_TOP).ok_or(MmError::Fault)?;
        let mut va = addr & !(PAGE - 1);
        let to_mm = |r: FaultReason| match r {
            FaultReason::NoMemory => MmError::NoMemory,
            _ => MmError::Fault,
        };
        while va < end {
            match paging::translate_leaf(self.root, va) {
                Some(l) => {
                    let (phys, fl) = (l.phys, l.flags);
                    if !fl.contains(PageFlags::USER) {
                        return Err(MmError::Fault);
                    }
                    if !fl.contains(PageFlags::WRITABLE) && need != Need::Read {
                        let v = g.space.tree.find(va).cloned().ok_or(MmError::Fault)?;
                        if need == Need::Write {
                            if !v.prot.write {
                                return Err(MmError::Fault);
                            }
                            self.write_present(g, va, &v, phys).map_err(to_mm)?;
                        } else if l.size == PageSize::Size2M {
                            // MM-13: the loader's write through a read-only 2 MiB leaf another space shares: split it,
                            // then the 4 KiB rule below copies the page.
                            if v.private() && !huge::own_exclusive(phys, l.size) {
                                if !self.split_huge_at(g, va, tlb::Cause::Cow) {
                                    return Err(MmError::NoMemory);
                                }
                                continue;
                            }
                        } else if v.private() && !(mm_core::is_user_frame(phys) && share::exclusive(phys)) {
                            // The loader writes through read-only pages: never into a frame another space or
                            // the page cache holds.
                            self.copy_into_place(g, va, &v, phys).map_err(to_mm)?;
                        }
                    }
                }
                None => {
                    let v = g.space.tree.find(va).cloned().ok_or(MmError::Fault)?;
                    let allowed = match need {
                        Need::Read | Need::Load => v.prot.accessible(),
                        Need::Write => v.prot.write,
                    };
                    if !allowed {
                        return Err(MmError::Fault);
                    }
                    // MM-13: a write into fresh memory takes a huge page as the program's own fault would.
                    if need == Need::Write
                        && let Some(base) = self.huge_base(g, &v, va)
                        && self.fill_huge_now(g, base, &v)
                    {
                        COPY_FILLS.inc();
                        continue;
                    }
                    self.fill_at(va, &v, need == Need::Write).map_err(to_mm)?;
                    COPY_FILLS.inc();
                }
            }
            va += PAGE;
        }
        Ok(())
    }

    fn locked_for(&self, addr: u64, len: usize, need: Need) -> Result<MutexGuard<'_, Inner>, MmError> {
        let mut g = self.inner.lock();
        self.ensure(&mut g, addr, len, need)?;
        Ok(g)
    }

    pub fn copy_from_user(&self, dst: &mut [u8], src: u64) -> Result<(), MmError> {
        let _g = self.locked_for(src, dst.len(), Need::Read)?;
        let root = self.root;
        if tlb::owns_here(root) {
            return hal::usercopy::copy_from_user(root, dst, src).map_err(|_| MmError::Fault);
        }
        let mut off = 0usize;
        while off < dst.len() {
            let a = src.wrapping_add(off as u64);
            let n = page_chunk(a, dst.len() - off);
            let d = &mut dst[off..off + n];
            tlb::without_lazy(root, || hal::usercopy::copy_from_user(root, d, a)).map_err(|_| MmError::Fault)?;
            off += n;
        }
        Ok(())
    }

    pub fn copy_to_user(&self, dst: u64, src: &[u8]) -> Result<(), MmError> {
        let _g = self.locked_for(dst, src.len(), Need::Write)?;
        let root = self.root;
        if tlb::owns_here(root) {
            return hal::usercopy::copy_to_user(root, dst, src).map_err(|_| MmError::Fault);
        }
        let mut off = 0usize;
        while off < src.len() {
            let a = dst.wrapping_add(off as u64);
            let n = page_chunk(a, src.len() - off);
            let s = &src[off..off + n];
            tlb::without_lazy(root, || hal::usercopy::copy_to_user(root, a, s)).map_err(|_| MmError::Fault)?;
            off += n;
        }
        Ok(())
    }

    /// PF-6 (fs-PF-4 CR 2): copies the page-cache frame parts `(frame, off, n)` back to back to user memory from
    /// `dst`, straight from each frame (no buffer in between when this thread owns the space here; else through a
    /// small stack buffer, `frame_via_buffer`), taking `mm.as` once for the whole call and filling
    /// user pages as `copy_to_user` does. Returns the bytes that reached the program: all, or exactly those before
    /// the first user page that cannot be written (Linux's short `copy_page_to_iter`, linux-6.6/lib/iov_iter.c:
    /// the read ends there). A part that leaves its frame ends the copy too. Never call it under a page-cache
    /// lock (`vfs.inode.index`: `mm.as` is outside it, CONTRACTS §4).
    pub fn copy_frames_to_user(&self, dst: u64, parts: &[(&mm_core::PageFrame, usize, usize)]) -> usize {
        let total = parts.iter().try_fold(0usize, |t, p| t.checked_add(p.2));
        let Some(total) = total.filter(|&t| t > 0) else { return 0 };
        let mut g = self.inner.lock();
        let ok = self.ensure_prefix(&mut g, dst, total, Need::Write);
        let root = self.root;
        let owns = tlb::owns_here(root);
        let mut done = 0usize;
        for &(frame, off, n) in parts {
            if done >= ok {
                break;
            }
            let n = n.min(ok - done);
            let Some(at) = dst.checked_add(done as u64) else { break };
            // Straight from the frame when this thread owns the space here (the fast path); the rest (not owned,
            // the fast path closed) through a small kernel buffer, under the `mm.as` held here, which the walk
            // needs (a walker is no TLB holder).
            let mut got = if owns { frame.copy_to_user(root, off, n, at) } else { 0 };
            if got < n {
                got += Self::frame_via_buffer(root, owns, frame, off + got, n - got, at + got as u64);
            }
            done += got;
            if got < n {
                break;
            }
        }
        drop(g);
        done
    }

    /// `copy_frames_to_user`'s other way, under `mm.as`: `n` bytes of `frame` from `off` to `at` through a stack
    /// buffer and `copy_to_user` (fast path or table walk; for a thread that does not own `root`, the walk with
    /// interrupts off, `tlb::without_lazy`). A chunk never crosses a user page, so a failed chunk is a page that
    /// cannot be written: the bytes before it count.
    fn frame_via_buffer(root: u64, owns: bool, frame: &mm_core::PageFrame, off: usize, n: usize, at: u64) -> usize {
        let mut buf = [0u8; 512];
        let mut k = 0usize;
        while k < n {
            let a = at + k as u64;
            let c = page_chunk(a, n - k).min(buf.len());
            if !frame.read(off + k, &mut buf[..c]) {
                break;
            }
            let s = &buf[..c];
            let r = if owns { hal::usercopy::copy_to_user(root, a, s) } else { tlb::without_lazy(root, || hal::usercopy::copy_to_user(root, a, s)) };
            if r.is_err() {
                break;
            }
            k += c;
        }
        k
    }

    /// PF-6 (fs-PF-5 CR 2): `copy_to_user` that takes no lock and never handles a fault (Linux
    /// `copy_to_user_nofault`): for lockless readers inside a grace section, and as the first try of a copy whose
    /// user pages are usually present. Only a thread that owns this space here (CR3 holds it, not lazily) copies;
    /// otherwise, and at the first user page not present and writable, it ends: the bytes before count, the
    /// caller's locked path (`copy_to_user`, which fills pages) does the rest.
    pub fn copy_to_user_nofault(&self, dst: u64, src: &[u8]) -> usize {
        if !tlb::owns_here(self.root) {
            return 0;
        }
        hal::usercopy::copy_to_user_nofault(self.root, dst, src)
    }

    /// `copy_to_user_nofault` straight from a page-cache frame (`n` bytes from `off`).
    pub fn copy_frame_to_user_nofault(&self, dst: u64, frame: &mm_core::PageFrame, off: usize, n: usize) -> usize {
        if !tlb::owns_here(self.root) {
            return 0;
        }
        frame.copy_to_user_nofault(self.root, off, n, dst)
    }

    /// `ensure` page by page: the bytes from `addr` (at most `len`) whose user pages are ready for `need`, up to
    /// the first page that is not.
    fn ensure_prefix(&self, g: &mut Inner, addr: u64, len: usize, need: Need) -> usize {
        let mut done = 0usize;
        while done < len {
            let Some(a) = addr.checked_add(done as u64) else { break };
            let k = page_chunk(a, len - done);
            if self.ensure(g, a, k, need).is_err() {
                break;
            }
            done += k;
        }
        done
    }

    /// Reads a NUL-terminated string of at most `max` bytes (without the NUL); pages after the NUL stay
    /// untouched.
    pub fn read_cstr(&self, src: u64, max: usize) -> Result<Vec<u8>, MmError> {
        let mut out: Vec<u8> = Vec::new();
        let mut a = src;
        while out.len() < max {
            let n = ((PAGE - (a & (PAGE - 1))) as usize).min(max - out.len());
            let at = out.len();
            out.try_reserve(n).map_err(|_| MmError::NoMemory)?;
            out.resize(at + n, 0);
            self.copy_from_user(&mut out[at..], a)?;
            if let Some(i) = out[at..].iter().position(|&b| b == 0) {
                out.truncate(at + i);
                return Ok(out);
            }
            a += n as u64;
        }
        Err(MmError::Fault)
    }

    pub fn read_u64(&self, src: u64) -> Result<u64, MmError> {
        let mut b = [0u8; 8];
        self.copy_from_user(&mut b, src)?;
        Ok(u64::from_le_bytes(b))
    }

    pub fn write_u64(&self, dst: u64, v: u64) -> Result<(), MmError> {
        self.copy_to_user(dst, &v.to_le_bytes())
    }

    /// Writes into user memory ignoring write protection (the loader fills read-only text pages). Always walks
    /// the tables (`copy_to_user_force`), so a lazily held root's stale entries cannot matter here.
    pub fn write_loader(&self, dst: u64, src: &[u8]) -> Result<(), MmError> {
        let _g = self.locked_for(dst, src.len(), Need::Load)?;
        hal::usercopy::copy_to_user_force(self.root, dst, src).map_err(|_| MmError::Fault)
    }
}
