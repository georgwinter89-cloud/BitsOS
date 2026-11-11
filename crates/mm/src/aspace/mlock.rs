//! GM-2a CR 4: mlock, munlock, mlockall, munlockall and mlock2 (linux-6.6/mm/mlock.c). V2 has no swap and never takes a
//! mapped page away, so a locked page stays by itself: what is left of the calls is Linux's answers, the limit, the
//! population and VM_LOCKED's effects on other calls (madvise, msync, fork, VmLck).
//!
//! The locked pages are not counted along: `Space::locked_pages` walks the VMAs at each call, which only programs that
//! lock pay for, and munmap, mremap, fork and exec cannot make a counter wrong.

use super::{AsCore, Inner};
use crate::PAGE;
use crate::linux::Errno;
use crate::vma::Vma;
use alloc::vec::Vec;
use core::sync::atomic::Ordering::Relaxed;
use hal::paging::tlb;

const MLOCK_ONFAULT: u64 = 1;
const MCL_CURRENT: u64 = 1;
const MCL_FUTURE: u64 = 2;
const MCL_ONFAULT: u64 = 4;

/// What the process layer knows about a caller: RLIMIT_MEMLOCK's soft value in bytes and CAP_IPC_LOCK.
#[derive(Clone, Copy, Debug)]
pub struct LockLimit {
    pub bytes: u64,
    pub capable: bool,
}

impl LockLimit {
    /// `can_do_mlock` (mlock.c:40).
    fn may_lock(self) -> bool {
        self.bytes != 0 || self.capable
    }
    fn pages(self) -> u64 {
        self.bytes >> 12
    }
}

/// How a VMA is locked: (VM_LOCKED, VM_LOCKONFAULT).
type Lock = (bool, bool);

fn is_device(v: &Vma) -> bool {
    v.file.as_ref().is_some_and(|f| f.device.is_some())
}

/// `__mlock_posix_error_return` (mlock.c:568).
fn posix_error(e: Errno) -> Errno {
    match e {
        Errno::Fault => Errno::NoMem,
        Errno::NoMem => Errno::Again,
        o => o,
    }
}

impl AsCore {
    /// Pages of locked VMAs (Linux `locked_vm`, VmLck / 4 KiB).
    pub fn locked_pages(&self) -> u64 {
        self.inner.lock().space.locked_pages()
    }

    /// `mlock_future_ok` asks for the caller's RLIMIT_MEMLOCK and CAP_IPC_LOCK at each mmap and brk: the syscall layer
    /// calls this first. Only a program under MCL_FUTURE pays for more than one load. Two threads of one program with
    /// different capabilities can see each other's value between this and their mapping.
    pub fn refresh_future_limit(&self, limit: impl FnOnce() -> LockLimit) {
        if self.future_lock.load(Relaxed) {
            let limit = limit();
            let mut g = self.inner.lock();
            if g.space.def_lock.0 {
                g.space.def_lock_limit = (!limit.capable).then(|| limit.pages());
            }
        }
    }

    /// mlock (flags 0) and mlock2 (mlock.c:583 `do_mlock`, :622, :632).
    pub fn sys_mlock(&self, start: u64, len: u64, flags: u64, limit: LockLimit) -> Result<(), Errno> {
        if flags & !MLOCK_ONFAULT != 0 {
            return Err(Errno::Inval);
        }
        if !limit.may_lock() {
            return Err(Errno::Perm);
        }
        let (start, len) = page_range(start, len);
        let mut g = self.inner.lock();
        if !limit.capable {
            let locked = (len >> 12).wrapping_add(g.space.locked_pages());
            // A range locked already does not count twice (`count_mm_mlocked_page_nr`).
            if locked > limit.pages() && locked.wrapping_sub(g.space.locked_in(start, len)) > limit.pages() {
                return Err(Errno::NoMem);
            }
        }
        self.apply_lock(&mut g, start, len, (true, flags & MLOCK_ONFAULT != 0))?;
        self.populate_locked(&mut g, start, start + len, false).map_err(posix_error)
    }

    /// munlock (mlock.c:651): no limit, no capability.
    pub fn sys_munlock(&self, start: u64, len: u64) -> Result<(), Errno> {
        let (start, len) = page_range(start, len);
        let mut g = self.inner.lock();
        self.apply_lock(&mut g, start, len, (false, false))
    }

    /// mlockall (mlock.c:707): MCL_CURRENT above the limit is ENOMEM, MCL_ONFAULT alone EINVAL, population errors of a
    /// single VMA do not count (`mm_populate` ignores them).
    pub fn sys_mlockall(&self, flags: u64, limit: LockLimit) -> Result<(), Errno> {
        if flags == 0 || flags & !(MCL_CURRENT | MCL_FUTURE | MCL_ONFAULT) != 0 || flags == MCL_ONFAULT {
            return Err(Errno::Inval);
        }
        if !limit.may_lock() {
            return Err(Errno::Perm);
        }
        let mut g = self.inner.lock();
        if flags & MCL_CURRENT != 0 && g.space.tree.pages() > limit.pages() && !limit.capable {
            return Err(Errno::NoMem);
        }
        let onfault = flags & MCL_ONFAULT != 0;
        g.space.def_lock = (flags & MCL_FUTURE != 0, flags & MCL_FUTURE != 0 && onfault);
        g.space.def_lock_limit = (!limit.capable).then(|| limit.pages());
        self.future_lock.store(g.space.def_lock.0, Relaxed);
        if flags & MCL_CURRENT != 0 {
            self.lock_every_vma(&mut g, (true, onfault))?;
            self.populate_locked(&mut g, 0, u64::MAX, true)?;
        }
        Ok(())
    }

    /// munlockall (mlock.c:737): always 0 unless the VMA list runs out of room.
    pub fn sys_munlockall(&self) -> Result<(), Errno> {
        let mut g = self.inner.lock();
        g.space.def_lock = (false, false);
        g.space.def_lock_limit = None;
        self.future_lock.store(false, Relaxed);
        self.lock_every_vma(&mut g, (false, false))
    }

    /// `apply_vma_lock_flags` (mlock.c:480): a hole ends the walk with ENOMEM, what lies before it stays changed.
    fn apply_lock(&self, g: &mut Inner, start: u64, len: u64, lock: Lock) -> Result<(), Errno> {
        let Some(end) = start.checked_add(len) else { return Err(Errno::Inval) };
        let mut at = start;
        while at < end {
            let Some(v) = g.space.tree.next_from(at).cloned() else { return Err(Errno::NoMem) };
            if at < v.start {
                return Err(Errno::NoMem);
            }
            let to = v.end.min(end);
            self.set_lock(g, &v, at, to, lock)?;
            at = to;
        }
        Ok(())
    }

    fn lock_every_vma(&self, g: &mut Inner, lock: Lock) -> Result<(), Errno> {
        let mut all: Vec<Vma> = Vec::new();
        all.try_reserve(g.space.tree.len()).map_err(|_| Errno::Again)?;
        all.extend(g.space.tree.iter().cloned());
        for v in &all {
            self.set_lock(g, v, v.start, v.end, lock)?;
        }
        Ok(())
    }

    /// `mlock_fixup` (mlock.c:424): device mappings (VM_SPECIAL) never take the flag; no room for a split is EAGAIN.
    fn set_lock(&self, g: &mut Inner, v: &Vma, s: u64, e: u64, lock: Lock) -> Result<(), Errno> {
        if is_device(v) || (v.flags.locked, v.flags.lock_onfault) == lock {
            return Ok(());
        }
        self.split_edges(g, s, e, tlb::Cause::Madvise);
        g.space.tree.update(s, e, &mut |x| (x.flags.locked, x.flags.lock_onfault) = lock).map_err(|_| Errno::Again)
    }

    /// `__mm_populate` (gup.c:1608): the locked VMAs of the range, a written private mapping for writing; `ignore`
    /// goes on after a VMA's error. PROT_NONE cannot be touched: EFAULT, which the caller turns into ENOMEM.
    fn populate_locked(&self, g: &mut Inner, start: u64, end: u64, ignore: bool) -> Result<(), Errno> {
        let mut at = start;
        while at < end {
            let Some(v) = g.space.tree.next_from(at).cloned() else { return Ok(()) };
            let (s, e) = (at.max(v.start), v.end.min(end));
            at = v.end;
            if s >= e {
                return Ok(());
            }
            if is_device(&v) || v.flags.lock_onfault || !v.flags.locked {
                continue;
            }
            let write = v.prot.write && v.private();
            let r = if !v.prot.accessible() {
                Err(Errno::Fault)
            } else if write || v.prot.read {
                self.populate_advice(g, &v, s, e, write)
            } else {
                // Only Linux's FOLL_FORCE reads an unreadable mapping: such pages come at their first use here.
                Ok(())
            };
            if !ignore {
                r?;
            }
        }
        Ok(())
    }
}

/// `do_mlock`'s `PAGE_ALIGN(len + offset_in_page(start))`, which wraps as in Linux.
fn page_range(start: u64, len: u64) -> (u64, u64) {
    let off = start & (PAGE - 1);
    (start - off, len.wrapping_add(off).wrapping_add(PAGE - 1) & !(PAGE - 1))
}
