//! `madvise` and `mincore` (MM-5b) with Linux 6.6's semantics (mm/madvise.c, mm/mincore.c).
//!
//! madvise walks the VMAs of the range like `madvise_walk_vmas`: holes are skipped but answer ENOMEM at the end,
//! a VMA that refuses the advice answers at once. What each advice does here:
//! - DONTNEED (and DONTNEED_LOCKED): the pages go (frames back, or back to the file/page cache); the next access
//!   reads zero for anonymous memory, the file's content for a file mapping. The commit stays (Linux).
//! - FREE: anonymous memory only (else EINVAL); freed at once (Linux frees lazily under memory pressure, V2 has
//!   no reclaim, so it gives the memory back now; the content after FREE is undefined on both).
//! - WILLNEED, COLD, PAGEOUT, NORMAL/RANDOM/SEQUENTIAL, MERGEABLE/UNMERGEABLE, HUGEPAGE/NOHUGEPAGE, DONTDUMP/DODUMP:
//!   accepted (no swap, no KSM, no huge pages, no core dumps in V2), with Linux's refusals for device VMAs.
//! - DONTFORK/DOFORK, WIPEONFORK/KEEPONFORK: VMA flags `fork` obeys (split at the range like Linux).
//! - POPULATE_READ/WRITE: faults the pages in (a write breaks copy-on-write); EINVAL without the right, EFAULT
//!   for a page past the end of a file.
//! - REMOVE: EINVAL/EACCES as Linux; on a shared mapping of a writable fd the cached pages of the range are
//!   zeroed, so every mapping and read(2) sees zeros as after Linux's hole punch; the frames stay until the page
//!   cache can punch holes (FS change request), the file size stays.
//! - COLLAPSE: EINVAL (no huge pages). HWPOISON/SOFT_OFFLINE: EPERM (`plan_madvise`).
//!
//! Device VMAs count as Linux's VM_PFNMAP|VM_IO|VM_DONTEXPAND: DONTNEED/FREE/COLD/PAGEOUT/POPULATE, DOFORK and
//! DODUMP are EINVAL there.

use super::{AsCore, Inner};
use crate::fault::FaultReason;
use crate::linux::{madv, plan_madvise, Errno};
use crate::vma::Vma;
use crate::PAGE;
use alloc::vec::Vec;
use hal::paging::{self, PageFlags};

klog::counter!(DONTNEED_CALLS, "mm.as.dontneed_calls");

/// Linux `access_ok` on x86-64 for a run-time size: the end must not wrap and must stay in the lower half.
fn access_ok(addr: u64, len: u64) -> bool {
    let sum = addr.wrapping_add(len);
    (sum as i64) >= 0 && sum >= addr
}

/// MADV_REMOVE without a hole punch in the page cache: the cached pages of file pages `first..first + n` read zero
/// from now on (pages not cached are holes already). Bounded by what exists, not by the range: the walk stops at
/// the end of the file and, for tmpfs, jumps from one cached page to the next (`seek_data`), so a huge sparse
/// range does not hold the address-space lock for millions of empty lookups (B4).
fn zero_cached(f: &crate::vma::FileRef, first: u64, n: u64) {
    let Some(cache) = f.backing.file().and_then(|m| m.inode().page_cache()) else { return };
    let zeros = [0u8; 512];
    let end = first.saturating_add(n).min(cache.size().div_ceil(PAGE));
    let mut pgoff = first;
    while pgoff < end {
        // The next page with data at or after `pgoff` (tmpfs: the next cached page; with a backend: `pgoff`).
        let Some(at) = pgoff.checked_mul(PAGE).and_then(|pos| cache.seek_data(pos, false)) else { break };
        pgoff = pgoff.max(at / PAGE);
        if pgoff >= end {
            break;
        }
        if let Some(p) = cache.cached(pgoff) {
            for off in (0..PAGE as usize).step_by(zeros.len()) {
                p.write(off, &zeros);
            }
            p.set_dirty();
        }
        pgoff += 1;
    }
}

impl AsCore {
    /// `madvise(start, len, behavior)`.
    pub fn sys_madvise(&self, start: u64, len: u64, behavior: u64) -> Result<(), Errno> {
        let Some((start, end)) = plan_madvise(start, len, behavior)? else { return Ok(()) };
        let mut g = self.inner.lock();
        let mut at = start;
        let mut unmapped = false;
        loop {
            let Some(v) = g.space.tree.next_from(at).cloned() else { return Err(Errno::NoMem) };
            if at < v.start {
                unmapped = true;
                at = v.start;
                if at >= end {
                    break;
                }
            }
            let to = v.end.min(end);
            self.advise_one(&mut g, &v, at, to, behavior)?;
            at = to;
            if at >= end {
                break;
            }
        }
        if unmapped { Err(Errno::NoMem) } else { Ok(()) }
    }

    fn advise_one(&self, g: &mut Inner, v: &Vma, s: u64, e: u64, behavior: u64) -> Result<(), Errno> {
        let device = v.file.as_ref().is_some_and(|f| f.device.is_some());
        let set = |g: &mut Inner, f: &mut dyn FnMut(&mut Vma)| g.space.tree.update(s, e, f).map_err(|_| Errno::Again);
        match behavior {
            madv::DONTNEED | madv::DONTNEED_LOCKED | madv::FREE => {
                if device || (behavior == madv::FREE && v.file.is_some()) {
                    return Err(Errno::Inval);
                }
                let segs = Self::segments(g, s, e)?;
                self.unmap_pages(g, s, e, &segs);
                DONTNEED_CALLS.inc();
                Ok(())
            }
            madv::COLD | madv::PAGEOUT if device => Err(Errno::Inval),
            madv::POPULATE_READ | madv::POPULATE_WRITE => self.populate_advice(g, v, s, e, behavior == madv::POPULATE_WRITE),
            madv::REMOVE => match &v.file {
                None => Err(Errno::Inval),
                // `vma_is_shared_maywrite` (CachyOS 7.2; 6.6 still asked for VM_WRITE): MAP_SHARED of a writable fd
                // (VM_MAYWRITE). A shared mapping of a read-only fd must never zero the file.
                Some(f) if !f.shared || !f.may_write => Err(Errno::Acces),
                Some(f) if f.device.is_some() => Err(Errno::OpNotSupp),
                Some(f) => {
                    zero_cached(f, v.pgoff_at(s).unwrap_or(0), (e - s) / PAGE);
                    Ok(())
                }
            },
            madv::DODUMP if device => Err(Errno::Inval),
            madv::DONTFORK => set(g, &mut |x| x.flags.dontfork = true),
            madv::DOFORK if device => Err(Errno::Inval),
            madv::DOFORK => set(g, &mut |x| x.flags.dontfork = false),
            madv::WIPEONFORK if v.file.is_some() => Err(Errno::Inval),
            madv::WIPEONFORK => set(g, &mut |x| x.flags.wipeonfork = true),
            madv::KEEPONFORK => set(g, &mut |x| x.flags.wipeonfork = false),
            madv::COLLAPSE => Err(Errno::Inval),
            _ => Ok(()),
        }
    }

    /// MADV_POPULATE_READ/WRITE (linux-6.6/mm/madvise.c `madvise_populate`, gup.c `check_vma_flags`): every page of
    /// [s, e) present (written: writable, copy-on-write broken).
    fn populate_advice(&self, g: &mut Inner, v: &Vma, s: u64, e: u64, write: bool) -> Result<(), Errno> {
        let device = v.file.as_ref().is_some_and(|f| f.device.is_some());
        if device || (write && !v.prot.write) || (!write && !v.prot.read) {
            return Err(Errno::Inval);
        }
        let err = |r: FaultReason| match r {
            FaultReason::Bus => Errno::Fault,
            _ => Errno::NoMem,
        };
        let mut va = s;
        while va < e {
            match paging::translate(self.root, va) {
                None => self.fill_at(va, v, write).map_err(err)?,
                Some((phys, fl)) if write && !fl.contains(PageFlags::WRITABLE) => self.write_present(g, va, v, phys).map_err(err)?,
                Some(_) => {}
            }
            va += PAGE;
        }
        Ok(())
    }

    /// `mincore(start, len, vec)`: one byte per page, 1 = resident (a frame mapped here, or a file page in the page
    /// cache). EINVAL for an unaligned start, ENOMEM for a range outside user space or with an unmapped page,
    /// EFAULT for `vec`; results are copied in chunks of 4096 pages, as Linux does (a hole after the first chunk
    /// leaves the earlier bytes written).
    pub fn sys_mincore(&self, start: u64, len: u64, vec: u64) -> Result<(), Errno> {
        if start & (PAGE - 1) != 0 {
            return Err(Errno::Inval);
        }
        if !access_ok(start, len) {
            return Err(Errno::NoMem);
        }
        let mut pages = (len >> 12) + (len & (PAGE - 1) != 0) as u64;
        if !access_ok(vec, pages) {
            return Err(Errno::Fault);
        }
        let mut buf: Vec<u8> = Vec::new();
        buf.try_reserve_exact(pages.min(PAGE) as usize).map_err(|_| Errno::Again)?;
        let (mut at, mut out) = (start, vec);
        while pages > 0 {
            buf.clear();
            {
                let g = self.inner.lock();
                let v = g.space.tree.find(at).ok_or(Errno::NoMem)?;
                let end = v.end.min(at + pages.min(PAGE) * PAGE);
                let mut va = at;
                while va < end {
                    buf.push(self.resident(v, va) as u8);
                    va += PAGE;
                }
            }
            self.copy_to_user(out, &buf).map_err(|_| Errno::Fault)?;
            let n = buf.len() as u64;
            pages -= n;
            out += n;
            at += n * PAGE;
        }
        Ok(())
    }

    /// Is the page at `va` of VMA `v` in memory (mincore's answer)?
    fn resident(&self, v: &Vma, va: u64) -> bool {
        if paging::translate(self.root, va).is_some() {
            return true;
        }
        let (Some(f), Some(pgoff)) = (&v.file, v.pgoff_at(va)) else { return false };
        f.device.is_none() && f.backing.file().and_then(|m| m.inode().page_cache()).is_some_and(|c| c.cached(pgoff).is_some())
    }
}
