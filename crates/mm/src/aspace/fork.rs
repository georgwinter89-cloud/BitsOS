//! The child's address space for fork (MM-5b, AB-4 calls `fork`; Linux `dup_mmap` + `copy_page_range`).
//!
//! The child gets every VMA except MADV_DONTFORK ones (MADV_WIPEONFORK ones empty) and, for the private ones,
//! the parent's present frames shared copy-on-write: one shootdown makes the parent's writable program frames
//! read-only, then each frame's share count goes up and the child maps it read-only too; the first write in
//! either space copies it (or takes it back once that space is the last holder, `fault.rs`). Page-cache frames
//! of file mappings and device pages are not copied: the child faults them in from the file or the device
//! (Linux skips such VMAs too, `vma_needs_copy`). An `Fill::Eager` space (no fault path) gets copies instead.
//!
//! Commit: the child is charged every accounted VMA it gets (Linux charges `vma_pages` per VM_ACCOUNT VMA in
//! `dup_mmap`), all before anything is copied: ENOMEM refuses the fork and the parent is unchanged.
//!
//! Huge pages: MM maps program memory only in 4 KiB pages (no transparent huge pages), so there is nothing to
//! split; a large user leaf would be a bug and refuses the fork (ENOMEM, Error event) instead of sharing part of it.

use super::{AddressSpace, AsCore, Class, Counters, Fill, Inner};
use crate::vma::Vma;
use crate::{commit, share, MmError, PAGE};
use alloc::vec::Vec;
use hal::paging::{self, PageFlags, PageSize};

klog::counter!(FORKS, "mm.as.forks");
klog::counter!(FORK_SHARED, "mm.as.fork_shared_pages");
klog::counter!(FORK_COPIED, "mm.as.fork_copied_pages");

/// Does the child get the parent's frames of `v` (shared copy-on-write, or copied)?
fn copies_pages(v: &Vma) -> bool {
    !v.flags.dontfork && !v.flags.wipeonfork && v.private()
}

impl AsCore {
    /// A copy of this address space for a fork's child (see the module docs). Fails with `NoMemory` when the
    /// commit, a page table or a copy does not fit; the parent is then unchanged except that some of its
    /// writable pages became read-only (their next write simply takes them back).
    pub fn fork(&self) -> Result<AddressSpace, MmError> {
        let g = self.inner.lock();
        let kept: Vec<&Vma> = {
            let mut v = Vec::new();
            v.try_reserve(g.space.tree.len()).map_err(|_| MmError::NoMemory)?;
            v.extend(g.space.tree.iter().filter(|x| !x.flags.dontfork));
            v
        };
        let charge: u64 = kept.iter().filter(|v| v.flags.account).map(|v| v.pages()).sum();
        if g.limit.is_some_and(|l| charge > l) {
            commit::refused_by_space(charge);
            return Err(MmError::NoMemory);
        }
        if !commit::charge(charge) {
            klog::event!(Info, "mm", "as.fork_refused", pages = charge; "fork refused: the child's private writable memory does not fit the commit");
            return Err(MmError::NoMemory);
        }
        let root = match paging::new_user_root(crate::kernel_root(), &mut super::alloc_root_block) {
            Some(r) => r,
            None => {
                commit::uncharge(charge);
                return Err(MmError::NoMemory);
            }
        };
        let mut c = Inner::new(g.fill);
        c.limit = g.limit;
        c.space.brk_start = g.space.brk_start;
        c.space.brk = g.space.brk;
        c.space.mmap_top = g.space.mmap_top;
        c.space.committed = charge;
        let mut built = c.space.tree.reserve(kept.len()).is_ok();
        for v in &kept {
            if built {
                built = c.space.tree.insert((*v).clone()).is_ok();
            }
        }
        // From here the child cleans up after itself (its drop gives the charge, shared frames and tables back).
        // Its counters hold what was mapped, also after a failure half-way (Linux dup_mm: hiwater_rss = rss and
        // hiwater_vm = total_vm of the child; the tree's peak is its size, built by inserts only).
        let ctr = Counters::default();
        let mut done = built && self.copy_pages(&g, &kept, root, &ctr);
        let child = AddressSpace::from_inner(root, c, ctr);
        for v in &kept {
            if let Some(d) = v.file.as_ref().and_then(|f| f.device.as_ref()) {
                done &= d.add_space(&child.me);
            }
            // MM-rmap: the child maps the parent's files too (Linux dup_mmap links each VMA into the file's
            // i_mmap). Without taking the child's lock: the parent's `mm.as` is held here.
            if let Some(k) = v.file.as_ref().and_then(super::rmap::key_of) {
                done &= super::rmap::add(k, &child.me);
            }
        }
        if !done {
            klog::event!(Warn, "mm", "as.fork_no_memory", vmas = kept.len(); "fork refused: no memory for the child's tables or copies");
            return Err(MmError::NoMemory);
        }
        FORKS.inc();
        Ok(child)
    }

    /// The child's page tables under `root` for the private VMAs in `kept` (shared copy-on-write, or copied for an
    /// eager space). False: out of memory (or a large user leaf); what was mapped stays for the child's drop.
    fn copy_pages(&self, g: &Inner, kept: &[&Vma], root: u64, ctr: &Counters) -> bool {
        let Some((lo, hi)) = kept.iter().filter(|v| copies_pages(v)).fold(None, |r: Option<(u64, u64)>, v| Some(r.map_or((v.start, v.end), |(a, b)| (a.min(v.start), b.max(v.end))))) else {
            return true;
        };
        let tree = &g.space.tree;
        let cow = g.fill == Fill::OnFault;
        let own = |phys: u64| mm_core::is_user_frame(phys);
        if cow {
            // The parent's writable program frames become read-only everywhere first (one shootdown for all), so
            // no store lands in a frame after the child shares it. A CPU that does not answer is logged; its
            // pending IPI still flushes before it runs the parent's code again.
            let r = paging::protect_user(self.root, lo, hi - lo, &mut |u| {
                let shareable = u.flags.contains(PageFlags::WRITABLE) && own(u.phys) && tree.find(u.virt).is_some_and(copies_pages);
                shareable.then_some(PageFlags(u.flags.0 & !PageFlags::WRITABLE))
            });
            match r {
                Err(e) => {
                    klog::event!(Error, "mm", "as.fork_protect_failed", error = alloc::format!("{e:?}"));
                    return false;
                }
                Ok((_, o)) if !o.confirmed() => {
                    super::UNCONFIRMED.inc();
                    klog::event!(Warn, "mm", "as.shootdown_unconfirmed", root = klog::Hex(self.root), start = klog::Hex(lo), end = klog::Hex(hi),
                        cpus = o.unanswered, resends = o.resends, not_ready = o.not_ready;
                        "a CPU did not answer the TLB shootdown of a fork: until it does, the parent's stores there could reach the child");
                }
                Ok(_) => {}
            }
        }
        let (mut ok, mut shared, mut copied) = (true, 0u64, 0u64);
        let fail_after = g.fork_fail_after;
        paging::for_each_leaf(self.root, lo, hi, &mut |u| {
            if !ok || !own(u.phys) || !tree.find(u.virt).is_some_and(copies_pages) {
                return;
            }
            if u.size != PageSize::Size4K {
                klog::event!(Error, "mm", "as.fork_large_leaf", virt = klog::Hex(u.virt); "a large user page: MM maps program memory in 4 KiB pages only");
                ok = false;
                return;
            }
            let (frame, fl) = if cow && share::share(u.phys) {
                shared += 1;
                (u.phys, PageFlags(u.flags.0 & !PageFlags::WRITABLE))
            } else {
                // Eager space, or no room for the count: the child gets its own copy (writable as the parent's).
                match mm_core::alloc_copy(u.phys, mm_core::Kind::User) {
                    Some(f) => {
                        copied += 1;
                        (f, u.flags)
                    }
                    None => {
                        ok = false;
                        return;
                    }
                }
            };
            let injected = fail_after.is_some_and(|n| shared + copied > n);
            let mut alloc_table = || {
                let t = mm_core::alloc_table_frame();
                ctr.tables_added(t.is_some() as u64);
                t
            };
            if injected || paging::map_user_4k(root, u.virt, frame, fl, &mut alloc_table).is_err() {
                // Not mapped in the child: give the share (or the copy) back right away.
                if frame == u.phys {
                    let _ = share::release(frame);
                } else {
                    super::free_user(frame);
                }
                ok = false;
                return;
            }
            ctr.add(Class::Anon, 1);
        });
        FORK_SHARED.add(shared);
        FORK_COPIED.add(copied);
        klog::event!(Debug, "mm", "as.forked", shared = shared, copied = copied, pages = (hi - lo) / PAGE, ok = ok);
        ok
    }
}
