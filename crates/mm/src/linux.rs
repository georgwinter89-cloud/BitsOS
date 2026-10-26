//! Linux semantics of `mmap`/`munmap`/`mprotect`/`brk` for anonymous memory (pure, host-tested): argument
//! checks in Linux's order (`arch/x86/kernel/sys_x86_64.c`, `mm/mmap.c` `do_mmap`/`__get_unmapped_area`/`brk`,
//! `mm/mprotect.c`, `mm/vma.c` `do_vmi_munmap`), placement (hint, top-down search) and the VMA tree changes.
//! Each call is split into a check (`plan_*`, no change) and a commit, so the address space can do its
//! page-table work in between and never leaves the tree half-changed when memory runs out.

use crate::vma::{FileRef, Prot, Vma, VmaFlags, VmaKind, VmaTree};
use uapi::errno;

pub const PAGE: u64 = 4096;
/// Linux `TASK_SIZE` on x86-64 with 4-level paging.
pub const TASK_SIZE: u64 = hal::USER_TOP;
/// `vm.mmap_min_addr` as CachyOS sets it (no mapping below it for a program without CAP_SYS_RAWIO).
pub const MMAP_MIN_ADDR: u64 = 0x1_0000;

pub const PROT_READ: u64 = 1;
pub const PROT_WRITE: u64 = 2;
pub const PROT_EXEC: u64 = 4;
pub const PROT_SEM: u64 = 8;
pub const PROT_GROWSDOWN: u64 = 0x0100_0000;
pub const PROT_GROWSUP: u64 = 0x0200_0000;

pub const MAP_SHARED: u64 = 0x01;
pub const MAP_PRIVATE: u64 = 0x02;
pub const MAP_TYPE: u64 = 0x0f;
pub const MAP_FIXED: u64 = 0x10;
pub const MAP_ANONYMOUS: u64 = 0x20;
pub const MAP_32BIT: u64 = 0x40;
pub const MAP_GROWSDOWN: u64 = 0x100;
pub const MAP_NORESERVE: u64 = 0x4000;
pub const MAP_POPULATE: u64 = 0x8000;
pub const MAP_NONBLOCK: u64 = 0x1_0000;
pub const MAP_STACK: u64 = 0x2_0000;
pub const MAP_HUGETLB: u64 = 0x4_0000;
pub const MAP_FIXED_NOREPLACE: u64 = 0x10_0000;

pub const MREMAP_MAYMOVE: u64 = 1;
pub const MREMAP_FIXED: u64 = 2;
pub const MREMAP_DONTUNMAP: u64 = 4;

/// madvise advice values (include/uapi/asm-generic/mman-common.h).
pub mod madv {
    pub const NORMAL: u64 = 0;
    pub const RANDOM: u64 = 1;
    pub const SEQUENTIAL: u64 = 2;
    pub const WILLNEED: u64 = 3;
    pub const DONTNEED: u64 = 4;
    pub const FREE: u64 = 8;
    pub const REMOVE: u64 = 9;
    pub const DONTFORK: u64 = 10;
    pub const DOFORK: u64 = 11;
    pub const MERGEABLE: u64 = 12;
    pub const UNMERGEABLE: u64 = 13;
    pub const HUGEPAGE: u64 = 14;
    pub const NOHUGEPAGE: u64 = 15;
    pub const DONTDUMP: u64 = 16;
    pub const DODUMP: u64 = 17;
    pub const WIPEONFORK: u64 = 18;
    pub const KEEPONFORK: u64 = 19;
    pub const COLD: u64 = 20;
    pub const PAGEOUT: u64 = 21;
    pub const POPULATE_READ: u64 = 22;
    pub const POPULATE_WRITE: u64 = 23;
    pub const DONTNEED_LOCKED: u64 = 24;
    pub const COLLAPSE: u64 = 25;
    pub const HWPOISON: u64 = 100;
    pub const SOFT_OFFLINE: u64 = 101;

    /// `madvise_behavior_valid` (linux-6.6/mm/madvise.c:1166) with CachyOS's config (KSM, THP and memory failure
    /// built in).
    pub fn valid(b: u64) -> bool {
        matches!(b, 0..=4 | 8..=25 | HWPOISON | SOFT_OFFLINE)
    }
}

/// The checks of `do_madvise` (linux-6.6/mm/madvise.c:1407) before the VMA walk: `Ok(None)` = nothing to do
/// (an empty range), else the page-aligned range. `behavior` is the `int` argument.
pub fn plan_madvise(start: u64, len_in: u64, behavior: u64) -> Result<Option<(u64, u64)>, Errno> {
    if !madv::valid(behavior) || start & (PAGE - 1) != 0 {
        return Err(Errno::Inval);
    }
    let len = len_in.wrapping_add(PAGE - 1) & !(PAGE - 1);
    if len_in != 0 && len == 0 {
        return Err(Errno::Inval);
    }
    let end = start.wrapping_add(len);
    if end < start {
        return Err(Errno::Inval);
    }
    if end == start {
        return Ok(None);
    }
    if behavior == madv::HWPOISON || behavior == madv::SOFT_OFFLINE {
        // CAP_SYS_ADMIN only; programs never have it here.
        return Err(Errno::Perm);
    }
    Ok(Some((start, end)))
}

/// Linux's window for `MAP_32BIT` (x86-64: the second GiB).
const MAP_32BIT_LO: u64 = 0x4000_0000;
const MAP_32BIT_HI: u64 = 0x8000_0000;

/// The errno a memory call returns (positive; the syscall layer negates it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Errno {
    Perm,
    NoMem,
    Inval,
    Exist,
    /// A file mapping (not anonymous): not this layer's (FS-5/MM-5b).
    NoDev,
    Overflow,
    /// mprotect adding PROT_WRITE to a shared mapping of a read-only fd (FS-5, Linux VM_MAYWRITE).
    Acces,
    /// madvise could not split a VMA (Linux turns ENOMEM into EAGAIN there).
    Again,
    /// mremap of an address without a mapping; madvise(POPULATE) of a page that would be SIGBUS.
    Fault,
    OpNotSupp,
}

impl Errno {
    pub fn code(self) -> i64 {
        match self {
            Errno::Perm => errno::EPERM,
            Errno::NoMem => errno::ENOMEM,
            Errno::Inval => errno::EINVAL,
            Errno::Exist => errno::EEXIST,
            Errno::NoDev => errno::ENODEV,
            Errno::Overflow => errno::EOVERFLOW,
            Errno::Acces => errno::EACCES,
            Errno::Again => errno::EAGAIN,
            Errno::Fault => errno::EFAULT,
            Errno::OpNotSupp => errno::EOPNOTSUPP,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Errno::Perm => "EPERM",
            Errno::NoMem => "ENOMEM",
            Errno::Inval => "EINVAL",
            Errno::Exist => "EEXIST",
            Errno::NoDev => "ENODEV",
            Errno::Overflow => "EOVERFLOW",
            Errno::Acces => "EACCES",
            Errno::Again => "EAGAIN",
            Errno::Fault => "EFAULT",
            Errno::OpNotSupp => "EOPNOTSUPP",
        }
    }
}

pub(crate) fn page_align(len: u64) -> Option<u64> {
    len.checked_add(PAGE - 1).map(|l| l & !(PAGE - 1))
}

/// What `mmap` will do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MmapPlan {
    pub addr: u64,
    pub len: u64,
    pub prot: Prot,
    /// MAP_FIXED over existing mappings: they go first.
    pub replaces: bool,
    /// MAP_POPULATE (without MAP_NONBLOCK): fill the pages now.
    pub populate: bool,
    /// MAP_SHARED (anonymous: backed by a nameless tmpfs file, MM-5b).
    pub shared: bool,
    /// Linux VM_ACCOUNT: a private writable mapping, charged to the commit (MAP_NORESERVE ignored, mode 2).
    /// File mappings: decided after the file check (`MAP_SHARED_VALIDATE` is shared too).
    pub account: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProtectPlan {
    pub start: u64,
    /// [start, applied_end) changes: Linux changes the VMAs up to the first hole and then fails.
    pub applied_end: u64,
    pub prot: Prot,
    /// The error returned after the change (ENOMEM for a hole), if any.
    pub error: Option<Errno>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BrkPlan {
    /// The break stays; `brk` returns it.
    Keep(u64),
    /// The break moves to `brk`; pages in `unmap` go, `map` is new heap.
    Move { brk: u64, unmap: Option<(u64, u64)>, map: Option<(u64, u64)> },
}

/// The VMA tree with the program break and where top-down placement starts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Space {
    pub tree: VmaTree,
    pub brk_start: u64,
    pub brk: u64,
    /// Highest end of a placed mapping (Linux's `mmap_base`, below the stack gap).
    pub mmap_top: u64,
    /// Pages of VM_ACCOUNT VMAs (what this space holds of the commit; kept by every commit step below).
    pub committed: u64,
}

impl Space {
    pub fn new(mmap_top: u64) -> Self {
        Space { tree: VmaTree::new(), brk_start: 0, brk: 0, mmap_top, committed: 0 }
    }

    /// `mmap(addr, len, prot, flags, -1, off)` for anonymous memory: the checks and the placement.
    pub fn plan_mmap(&self, addr: u64, len: u64, prot: u64, flags: u64, off: u64) -> Result<MmapPlan, Errno> {
        if off & (PAGE - 1) != 0 {
            return Err(Errno::Inval);
        }
        if flags & MAP_ANONYMOUS == 0 {
            return Err(Errno::NoDev);
        }
        self.plan(addr, len, prot, flags, off, false)
    }

    /// FS-5: the address part of `do_mmap` for a file mapping (the syscall layer checked the offset, the fd
    /// and MAP_HUGETLB before; the file's own checks, map type included, come after this, in `vfs::mmap::check`).
    pub fn plan_mmap_file(&self, addr: u64, len: u64, prot: u64, flags: u64, off: u64) -> Result<MmapPlan, Errno> {
        self.plan(addr, len, prot, flags, off, true)
    }

    fn plan(&self, addr: u64, len: u64, prot: u64, flags: u64, off: u64, file: bool) -> Result<MmapPlan, Errno> {
        if !file && flags & MAP_HUGETLB != 0 {
            // No huge-page pool (CachyOS default `nr_hugepages = 0` gives the same answer).
            return Err(Errno::NoMem);
        }
        if len == 0 {
            return Err(Errno::Inval);
        }
        let fixed = flags & (MAP_FIXED | MAP_FIXED_NOREPLACE) != 0;
        let mut hint = addr;
        if !fixed {
            hint &= !(PAGE - 1);
            if hint != 0 && hint < MMAP_MIN_ADDR {
                hint = MMAP_MIN_ADDR;
            }
        }
        let len = page_align(len).filter(|&l| l != 0).ok_or(Errno::NoMem)?;
        if (off >> 12).checked_add(len >> 12).is_none() {
            return Err(Errno::Overflow);
        }
        let start = self.place(hint, len, fixed, flags & MAP_32BIT != 0)?;
        if start > TASK_SIZE - len {
            return Err(Errno::NoMem);
        }
        if start & (PAGE - 1) != 0 {
            return Err(Errno::Inval);
        }
        if start < MMAP_MIN_ADDR {
            return Err(Errno::Perm);
        }
        let overlaps = !self.tree.is_free(start, start + len);
        if flags & MAP_FIXED_NOREPLACE != 0 && overlaps {
            return Err(Errno::Exist);
        }
        let shared = match flags & MAP_TYPE {
            MAP_SHARED => true,
            MAP_PRIVATE => false,
            // A file's map type (incl. MAP_SHARED_VALIDATE) is checked with the file, after this.
            _ if file => false,
            _ => return Err(Errno::Inval),
        };
        let populate = flags & (MAP_POPULATE | MAP_NONBLOCK) == MAP_POPULATE;
        let prot = Prot::from_linux(prot);
        Ok(MmapPlan { addr: start, len, prot, replaces: overlaps, populate, shared, account: !shared && prot.write })
    }

    /// `__get_unmapped_area` + `arch_get_unmapped_area_topdown`.
    pub(crate) fn place(&self, hint: u64, len: u64, fixed: bool, low32: bool) -> Result<u64, Errno> {
        if len > TASK_SIZE - MMAP_MIN_ADDR {
            return Err(Errno::NoMem);
        }
        if fixed {
            return Ok(hint);
        }
        if low32 {
            // x86 sends MAP_32BIT to the legacy bottom-up search, which takes a free hint whose end stays
            // below 2 GiB (wherever it starts) and otherwise searches [1 GiB, 2 GiB). Checked on CachyOS.
            if hint != 0 && hint <= MAP_32BIT_HI.saturating_sub(len) && self.tree.is_free(hint, hint + len) {
                return Ok(hint);
            }
            return self.tree.gap_bottomup(len, MAP_32BIT_LO, MAP_32BIT_HI).ok_or(Errno::NoMem);
        }
        if hint != 0 && hint <= TASK_SIZE - len && self.tree.is_free(hint, hint + len) {
            return Ok(hint);
        }
        self.tree.gap_topdown(len, MMAP_MIN_ADDR, self.mmap_top).ok_or(Errno::NoMem)
    }

    /// Puts the planned mapping into the tree (after the caller removed what it replaces). Room for the
    /// entries is reserved before anything changes.
    pub fn commit_mmap(&mut self, plan: &MmapPlan, kind: VmaKind) -> Result<(), Errno> {
        self.commit_mmap_with(plan, kind, None)
    }

    /// `commit_mmap` of a file mapping (FS-5).
    pub fn commit_mmap_with(&mut self, plan: &MmapPlan, kind: VmaKind, file: Option<FileRef>) -> Result<(), Errno> {
        self.tree.reserve(3).map_err(|_| Errno::NoMem)?;
        let end = plan.addr + plan.len;
        if plan.replaces {
            self.commit_munmap(plan.addr, end)?;
        }
        let flags = VmaFlags { account: plan.account, ..VmaFlags::default() };
        self.tree.insert(Vma { start: plan.addr, end, prot: plan.prot, kind, file, flags }).map_err(|_| Errno::NoMem)?;
        if plan.account {
            self.committed += plan.len / PAGE;
        }
        Ok(())
    }

    /// Pages of VM_ACCOUNT VMAs inside [start, end).
    pub fn accounted(&self, start: u64, end: u64) -> u64 {
        self.tree.overlapping(start, end).iter().filter(|v| v.flags.account).map(|v| (v.end.min(end) - v.start.max(start)) / PAGE).sum()
    }

    /// Whether `mprotect` to `prot` charges VMA `v` (linux-6.6/mm/mprotect.c:633: a private mapping that becomes
    /// writable and was neither writable nor accounted before).
    pub fn protect_charges(v: &Vma, prot: Prot) -> bool {
        prot.write && !v.prot.write && !v.flags.account && v.private()
    }

    /// `munmap(start, len)`: the range to remove.
    pub fn plan_munmap(&self, start: u64, len: u64) -> Result<(u64, u64), Errno> {
        if start & (PAGE - 1) != 0 || start > TASK_SIZE || len > TASK_SIZE - start {
            return Err(Errno::Inval);
        }
        let end = start + page_align(len).ok_or(Errno::Inval)?;
        if end == start {
            return Err(Errno::Inval);
        }
        Ok((start, end))
    }

    pub fn commit_munmap(&mut self, start: u64, end: u64) -> Result<bool, Errno> {
        let gone = self.accounted(start, end);
        let r = self.tree.remove(start, end).map_err(|_| Errno::NoMem)?;
        self.committed -= gone;
        Ok(r)
    }

    /// `mprotect(start, len, prot)`. `Ok(None)`: nothing to do (len 0).
    pub fn plan_mprotect(&self, start: u64, len: u64, prot: u64) -> Result<Option<ProtectPlan>, Errno> {
        let grows = prot & (PROT_GROWSDOWN | PROT_GROWSUP);
        let prot = prot & !(PROT_GROWSDOWN | PROT_GROWSUP);
        if grows == PROT_GROWSDOWN | PROT_GROWSUP || start & (PAGE - 1) != 0 {
            return Err(Errno::Inval);
        }
        if len == 0 {
            return Ok(None);
        }
        let end = page_align(len).and_then(|l| start.checked_add(l)).filter(|&e| e > start).ok_or(Errno::NoMem)?;
        if prot & !(PROT_READ | PROT_WRITE | PROT_EXEC | PROT_SEM) != 0 {
            return Err(Errno::Inval);
        }
        let first = self.tree.next_from(start).filter(|v| v.start < end).ok_or(Errno::NoMem)?;
        if grows & PROT_GROWSDOWN != 0 {
            // Only a MAP_GROWSDOWN (MAP_GROWSUP below) mapping may take it, and this layer makes none.
            return Err(Errno::Inval);
        }
        if first.start > start {
            return Err(Errno::NoMem);
        }
        if grows & PROT_GROWSUP != 0 {
            return Err(Errno::Inval);
        }
        let mut applied_end = self.tree.covered_from(start, end);
        let mut error = (applied_end < end).then_some(Errno::NoMem);
        // Linux's mprotect stops at the first VMA that may not get the new rights (VM_MAYWRITE of a shared
        // mapping of a read-only fd, VM_MAYEXEC of a file on a noexec mount: EACCES, linux-6.6/mm/mprotect.c:789),
        // after changing the ones before it.
        let refused = |f: &FileRef| (prot & PROT_WRITE != 0 && !f.may_write) || (prot & PROT_EXEC != 0 && !f.may_exec);
        if prot & (PROT_WRITE | PROT_EXEC) != 0
            && let Some(v) = self.tree.overlapping(start, applied_end).iter().find(|v| v.file.as_ref().is_some_and(refused))
        {
            applied_end = v.start.max(start);
            error = Some(Errno::Acces);
        }
        Ok(Some(ProtectPlan { start, applied_end, prot: Prot::from_linux(prot), error }))
    }

    /// Applies the plan; VMAs that `protect_charges` become VM_ACCOUNT (the caller charged them).
    pub fn commit_mprotect(&mut self, plan: &ProtectPlan) -> Result<(), Errno> {
        let prot = plan.prot;
        let mut added = 0;
        self.tree
            .update(plan.start, plan.applied_end, &mut |v| {
                if Self::protect_charges(v, prot) {
                    v.flags.account = true;
                    added += v.pages();
                }
                v.prot = prot;
            })
            .map_err(|_| Errno::NoMem)?;
        self.committed += added;
        Ok(())
    }

    /// Sets where the heap starts (after the program image).
    pub fn init_brk(&mut self, start: u64) {
        let s = page_align(start).unwrap_or(start);
        self.brk_start = s;
        self.brk = s;
    }

    /// `brk(new)` as Linux's `mm/mmap.c`.
    pub fn plan_brk(&self, new: u64) -> BrkPlan {
        if new < self.brk_start || self.brk_start == 0 {
            return BrkPlan::Keep(self.brk);
        }
        let (Some(newbrk), Some(oldbrk)) = (page_align(new), page_align(self.brk)) else { return BrkPlan::Keep(self.brk) };
        if newbrk == oldbrk {
            return BrkPlan::Move { brk: new, unmap: None, map: None };
        }
        if new <= self.brk {
            return BrkPlan::Move { brk: new, unmap: Some((newbrk, oldbrk)), map: None };
        }
        // One free page must stay between the heap and the next mapping.
        let next_ok = match (self.tree.next_from(oldbrk), newbrk.checked_add(PAGE)) {
            (_, None) => false,
            (Some(n), Some(limit)) => n.start >= limit,
            (None, Some(_)) => true,
        };
        if !next_ok || newbrk > TASK_SIZE {
            return BrkPlan::Keep(self.brk);
        }
        BrkPlan::Move { brk: new, unmap: None, map: Some((oldbrk, newbrk)) }
    }

    /// Applies a `Move` to the tree and the break. On `Err` nothing changed.
    pub fn commit_brk(&mut self, plan: &BrkPlan) -> Result<u64, Errno> {
        match *plan {
            BrkPlan::Keep(b) => Ok(b),
            BrkPlan::Move { brk, unmap, map } => {
                self.tree.reserve(3).map_err(|_| Errno::NoMem)?;
                if let Some((s, e)) = unmap {
                    self.commit_munmap(s, e)?;
                }
                if let Some((s, e)) = map {
                    let mut v = Vma::anon(s, e, Prot::rw(), VmaKind::Heap);
                    v.flags.account = true;
                    self.tree.insert(v).map_err(|_| Errno::NoMem)?;
                    self.committed += (e - s) / PAGE;
                }
                self.brk = brk;
                Ok(brk)
            }
        }
    }
}

#[cfg(test)]
#[path = "linux_tests.rs"]
mod tests;
