//! `mremap` (MM-5b) with Linux 6.6's semantics (mm/mremap.c:908 `SYSCALL_DEFINE5(mremap)`, `mremap_to` :800,
//! `vma_to_resize` :746, `move_vma` :583, `vma_expandable` :887), in Linux's order, side effects included (a
//! MREMAP_FIXED call that fails late has already unmapped its target, as on Linux).
//!
//! Moving keeps the frames: the page entries go from the old range to the new one (frames, page-cache pins and
//! device pages stay what they are, nothing is copied); the old entries are shot down. Commit: growth is charged
//! for accounted VMAs; a move charges the difference (MREMAP_DONTUNMAP the whole new length, the old VMA keeps
//! its charge). Device VMAs (Linux VM_DONTEXPAND|VM_PFNMAP) move but do not grow (EFAULT) and refuse
//! MREMAP_DONTUNMAP (EINVAL).

use super::{AsCore, Inner};
use crate::linux::{Errno, Space, MMAP_MIN_ADDR, MREMAP_DONTUNMAP, MREMAP_FIXED, MREMAP_MAYMOVE, TASK_SIZE};
use crate::vma::Vma;
use crate::{commit, PAGE};
use hal::paging::{self, Released};

klog::counter!(REMAP_MOVES, "mm.as.remap_moves");
klog::counter!(REMAP_MOVED_PAGES, "mm.as.remap_moved_pages");

fn align(len: u64) -> u64 {
    // PAGE_ALIGN wraps on Linux too; a wrapped length meets the checks below.
    len.wrapping_add(PAGE - 1) & !(PAGE - 1)
}

fn is_device(v: &Vma) -> bool {
    v.file.as_ref().is_some_and(|f| f.device.is_some())
}

/// Linux VM_MAYSHARE: a MAP_SHARED mapping (of any fd, or shared anonymous memory).
fn may_share(v: &Vma) -> bool {
    v.file.as_ref().is_some_and(|f| f.shared || f.backing.file().is_some_and(|m| m.may_share()))
}

/// The file page (Linux `vm_pgoff`) of `addr` inside `v`; anonymous VMAs count from address 0, as Linux's.
fn pgoff_of(v: &Vma, addr: u64) -> u64 {
    v.pgoff_at(addr).unwrap_or(addr / PAGE)
}

impl AsCore {
    /// `mremap(addr, old_len, new_len, flags, new_addr)`: the new address.
    pub fn sys_mremap(&self, addr: u64, old_len: u64, new_len: u64, flags: u64, new_addr: u64) -> Result<u64, Errno> {
        if flags & !(MREMAP_FIXED | MREMAP_MAYMOVE | MREMAP_DONTUNMAP) != 0
            || (flags & MREMAP_FIXED != 0 && flags & MREMAP_MAYMOVE == 0)
            || (flags & MREMAP_DONTUNMAP != 0 && (flags & MREMAP_MAYMOVE == 0 || old_len != new_len))
            || addr & (PAGE - 1) != 0
        {
            return Err(Errno::Inval);
        }
        let (old_len, new_len) = (align(old_len), align(new_len));
        // CachyOS 7.2 (`check_mremap_params`: "Is the new length silly?") refuses a new length past TASK_SIZE
        // before looking at the mapping; 6.6 got there later or not at all (the ABI diff is the oracle).
        if new_len == 0 || new_len > TASK_SIZE {
            return Err(Errno::Inval);
        }
        let mut g = self.inner.lock();
        if g.space.tree.find(addr).is_none() {
            return Err(Errno::Fault);
        }
        if flags & (MREMAP_FIXED | MREMAP_DONTUNMAP) != 0 {
            return self.mremap_to(&mut g, addr, old_len, new_addr, new_len, flags);
        }
        if old_len >= new_len {
            if old_len != new_len {
                // new_len <= TASK_SIZE and addr is in a VMA, so this cannot wrap; wrapping_add keeps a hostile
                // length from ever overflow-panicking the kernel should the checks above change (Linux wraps too).
                let (s, e) = g.space.plan_munmap(addr.wrapping_add(new_len), old_len - new_len)?;
                self.remove_range(&mut g, s, e)?;
            }
            return Ok(addr);
        }
        let v = resize_check(&g.space, addr, old_len, new_len, flags)?;
        if old_len == v.end - addr && expandable(&g.space, &v, new_len - old_len) {
            self.expand(&mut g, &v, addr + new_len)?;
            return Ok(addr);
        }
        if flags & MREMAP_MAYMOVE == 0 {
            return Err(Errno::NoMem);
        }
        let to = g.space.place(0, new_len, false, false)?;
        self.move_vma(&mut g, &v, addr, old_len, to, new_len, false)
    }

    fn mremap_to(&self, g: &mut Inner, addr: u64, old_len: u64, new_addr: u64, new_len: u64, flags: u64) -> Result<u64, Errno> {
        if new_addr & (PAGE - 1) != 0 || new_len > TASK_SIZE || new_addr > TASK_SIZE - new_len {
            return Err(Errno::Inval);
        }
        if addr.wrapping_add(old_len) > new_addr && new_addr + new_len > addr {
            return Err(Errno::Inval);
        }
        if flags & MREMAP_FIXED != 0 {
            let (s, e) = g.space.plan_munmap(new_addr, new_len)?;
            self.remove_range(g, s, e)?;
        }
        let mut old_len = old_len;
        if old_len > new_len {
            let (s, e) = g.space.plan_munmap(addr.wrapping_add(new_len), old_len - new_len)?;
            self.remove_range(g, s, e)?;
            old_len = new_len;
        }
        let v = resize_check(&g.space, addr, old_len, new_len, flags)?;
        // get_unmapped_area: MAP_FIXED takes the address (its range was checked above);
        // DONTUNMAP alone treats it as a hint.
        // DONTUNMAP alone treats it as a hint; `security_mmap_addr` then refuses an address below mmap_min_addr
        // either way.
        let to = if flags & MREMAP_FIXED != 0 { new_addr } else { g.space.place(new_addr, new_len, false, false)? };
        if to < MMAP_MIN_ADDR {
            return Err(Errno::Perm);
        }
        self.move_vma(g, &v, addr, old_len, to, new_len, flags & MREMAP_DONTUNMAP != 0)
    }

    /// Grows `v` in place up to `new_end` (the range above is free): charged like the VMA, merged into it.
    fn expand(&self, g: &mut Inner, v: &Vma, new_end: u64) -> Result<(), Errno> {
        let pages = (new_end - v.end) / PAGE;
        let need = if v.flags.account { pages } else { 0 };
        g.charge(need, 0)?;
        let mut ext = v.clone();
        ext.start = v.end;
        ext.end = new_end;
        if let Some(f) = ext.file.as_mut() {
            f.pgoff += (v.end - v.start) / PAGE;
        }
        let before = g.space.committed;
        if g.space.tree.insert(ext).is_err() {
            commit::uncharge(need);
            return Err(Errno::NoMem);
        }
        g.space.committed += need;
        g.settle(before, need);
        Ok(())
    }

    /// Moves [old, old + old_len) of `v` to `to` with `new_len` (old_len <= new_len), Linux `move_vma`.
    #[allow(clippy::too_many_arguments)]
    fn move_vma(&self, g: &mut Inner, v: &Vma, old: u64, old_len: u64, to: u64, new_len: u64, dontunmap: bool) -> Result<u64, Errno> {
        let delta = if dontunmap { new_len } else { new_len - old_len };
        let need = if v.flags.account { delta / PAGE } else { 0 };
        g.space.tree.reserve(4).map_err(|_| Errno::NoMem)?;
        g.charge(need, 0)?;
        let before = g.space.committed;
        // Linux move_vma keeps hiwater_vm as it was: the VMA counted twice for the move is no peak.
        let peak = g.space.tree.peak_pages();
        let mut nv = v.clone();
        nv.start = to;
        nv.end = to + new_len;
        if let Some(f) = nv.file.as_mut() {
            f.pgoff = pgoff_of(v, old);
        }
        let account = nv.flags.account;
        if g.space.tree.insert(nv).is_err() {
            commit::uncharge(need);
            return Err(Errno::NoMem);
        }
        if account {
            g.space.committed += new_len / PAGE;
        }
        if let Err(e) = self.move_pages(g, old, to, old_len) {
            // Nothing moved (move_pages undoes itself): the new VMA goes again.
            let _ = g.space.commit_munmap(to, to + new_len);
            g.space.tree.restore_peak(peak);
            g.settle(before, need);
            return Err(e);
        }
        if !dontunmap && old_len > 0 {
            // The entries are gone from the old range already; only the VMA (and its charge) goes. (An old length
            // of 0 duplicates a shared mapping and keeps the old one, as Linux's failed munmap of 0 bytes does.)
            let _ = g.space.commit_munmap(old, old + old_len);
        }
        g.space.tree.restore_peak(peak);
        g.settle(before, need);
        REMAP_MOVES.inc();
        Ok(to)
    }

    /// Moves the page entries of [old, old + len) to [to, to + len): frames keep their owners (nothing copied or
    /// freed), the old entries are shot down and emptied tables freed. On a missing page table it puts back what
    /// it moved and answers ENOMEM.
    fn move_pages(&self, g: &mut Inner, old: u64, to: u64, len: u64) -> Result<(), Errno> {
        if len == 0 {
            return Ok(());
        }
        let mut failed = false;
        let mut moved = 0u64;
        paging::for_each_leaf(self.root, old, old + len, &mut |u| {
            if failed {
                return;
            }
            if paging::map_user_4k(self.root, to + (u.virt - old), u.phys, u.flags, &mut || self.alloc_table()).is_err() {
                failed = true;
            } else {
                moved += 1;
            }
        });
        // Take out one side's entries without freeing the frames (they live on at the other side).
        let from = if failed { to } else { old };
        let (d, l, ctr) = (&mut g.deferred, &mut g.lost, &self.ctr);
        let r = paging::unmap_user(self.root, from, len, &mut |r: Released| {
            if r.table {
                ctr.tables_freed(1);
                Self::give_back(&r, &super::Owner::Device, d, l);
            }
        });
        if r.as_ref().is_ok_and(|u| !u.outcome.confirmed()) {
            super::UNCONFIRMED.inc();
        }
        if failed {
            return Err(Errno::NoMem);
        }
        REMAP_MOVED_PAGES.add(moved);
        Ok(())
    }
}

/// Linux `vma_to_resize`: the VMA at `addr` may become `new_len` long (EFAULT, EINVAL as Linux).
fn resize_check(space: &Space, addr: u64, old_len: u64, new_len: u64, flags: u64) -> Result<Vma, Errno> {
    let v = space.tree.find(addr).ok_or(Errno::Fault)?.clone();
    if old_len == 0 && !may_share(&v) {
        return Err(Errno::Inval);
    }
    if flags & MREMAP_DONTUNMAP != 0 && is_device(&v) {
        return Err(Errno::Inval);
    }
    if old_len > v.end - addr {
        return Err(Errno::Fault);
    }
    if new_len == old_len {
        return Ok(v);
    }
    if pgoff_of(&v, addr).checked_add(new_len / PAGE).is_none() {
        return Err(Errno::Inval);
    }
    if is_device(&v) {
        return Err(Errno::Fault);
    }
    Ok(v)
}

/// Linux `vma_expandable`: nothing mapped above the VMA up to the new end, which stays in user space.
fn expandable(space: &Space, v: &Vma, delta: u64) -> bool {
    match v.end.checked_add(delta) {
        Some(end) => space.tree.is_free(v.end, end) && end <= TASK_SIZE,
        None => false,
    }
}
