//! Page faults, filling, copy-on-write and the user copies (MM-5a, MM-5b).
//!
//! A missing page gets its frame on the first touch: zeroed for anonymous memory, the page-cache frame (or a
//! private copy for a private write) for a file, the device's page for a device mapping. A write to a present
//! read-only page of a writable VMA is one of: copy-on-write after a fork (the program's own frame shared with
//! another space: copied, or taken back writable once this space is its last holder; Linux `do_wp_page`), the
//! first write to a private file page (copied), the dirty mark of a tracked shared file page or device page.

use super::{keep_read_only, page_flags, read_only, AsCore, Class, Inner, Owner, CACHE_BITS, UNCONFIRMED};
use crate::fault::{self, Access, FaultReason, SpuriousRun};
use crate::vma::{Prot, Vma, VmaKind};
use crate::{share, MmError, PAGE};
use alloc::vec::Vec;
use core::sync::atomic::Ordering::Relaxed;
use hal::paging::{self, tlb, PageFlags, PageSize, Released};
use ksync::MutexGuard;
use vfs::pagecache::PageFault;

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

/// Who owns the frame a VMA maps at a page (for handing an old frame back).
fn owner_of(v: &Vma) -> Owner {
    match &v.file {
        Some(f) if f.device.is_some() => Owner::Device,
        Some(f) => Owner::File(f.backing.clone(), v.kind == VmaKind::Anonymous),
        None => Owner::Anon,
    }
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
        self.fill_at_with(va, v, write, &mut None)
    }

    /// `fill_at`, an anonymous page taking `spare` (a zeroed frame made before the lock) when there is one.
    fn fill_at_with(&self, va: u64, v: &Vma, write: bool, spare: &mut Option<u64>) -> Result<(), FaultReason> {
        let (Some(f), Some(pgoff)) = (&v.file, v.pgoff_at(va)) else { return self.fill_page(va, v.prot, spare) };
        if let Some(d) = &f.device {
            let pg = d.fault(pgoff, write).map_err(file_fault)?;
            let fl = PageFlags::user(pg.writable && v.prot.write, v.prot.exec).with_cache(pg.cache);
            paging::map_user_4k(self.root, va, pg.phys, fl, &mut || self.alloc_table()).map_err(|_| FaultReason::NoMemory)?;
            self.ctr.add(Class::Device, 1);
            DEVICE_FILLS.inc();
            return Ok(());
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
    pub(crate) fn populate(&self, g: &Inner, start: u64, end: u64, only_accessible: bool) -> Result<(), FaultReason> {
        for v in g.space.tree.overlapping(start, end) {
            if only_accessible && !v.prot.accessible() {
                continue;
            }
            let mut va = v.start.max(start);
            while va < v.end.min(end) {
                if paging::translate(self.root, va).is_none() {
                    match self.fill_at(va, v, false) {
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
        let r = self.fault_with(addr, error, run, &mut spare);
        if let Some(f) = spare {
            super::free_user(f);
            self.anon_hint.store(false, Relaxed);
            SPARE_UNUSED.inc();
        }
        r
    }

    fn fault_with(&self, addr: u64, error: u64, run: &mut SpuriousRun, spare: &mut Option<u64>) -> Result<(), FaultReason> {
        let mut g = self.inner.lock();
        let vma = g.space.tree.find(addr).cloned();
        let access = fault::check(error, vma.as_ref().map(|v| v.prot))?;
        let Some(v) = vma else { return Err(FaultReason::NotMapped) };
        let page = addr & !(PAGE - 1);
        let Some((phys, fl)) = paging::translate(self.root, page) else {
            run.reset();
            self.fill_at_with(page, &v, access == Access::Write, spare)?;
            FAULT_FILLS.inc();
            return Ok(());
        };
        // A write to a page kept read-only on purpose: copy-on-write, a private file copy, a dirty mark.
        if access == Access::Write && fl.contains(PageFlags::USER) && !fl.contains(PageFlags::WRITABLE) {
            run.reset();
            return self.write_present(&mut g, page, &v, phys);
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
            let base = if !fl.contains(PageFlags::WRITABLE) && keep_read_only(&v, phys) { read_only(v.prot) } else { page_flags(v.prot) };
            let want = PageFlags(base.0 | (fl.0 & CACHE_BITS));
            if let Ok((_, o)) = paging::grant_user(self.root, page, PAGE, &mut |_| Some(want))
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
            match paging::translate(self.root, va) {
                Some((phys, fl)) => {
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
