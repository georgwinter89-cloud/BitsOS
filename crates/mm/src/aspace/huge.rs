//! MM-13: transparent huge pages of one address space (Linux THP in `always` mode, linux-6.6/mm/huge_memory.c).
//!
//! A write fault, a user copy's write or MAP_POPULATE / MADV_POPULATE_WRITE on a missing page whose aligned
//! 2 MiB lies wholly inside a private anonymous VMA (heap, ELF data and bss, mmap; not a stack, not
//! MAP_GROWSDOWN, not MADV_NOHUGEPAGE / MAP_STACK, not after PR_SET_THP_DISABLE, by the mode of `thp`) and whose
//! page-directory slot is empty takes one zeroed 2 MiB block (buddy order 9, `alloc_zeroed_huge_light`: only
//! what is free now, never reclaim) and maps it with one 2 MiB entry (`huge_base`, `map_huge`; Linux
//! `do_huge_pmd_anonymous_page`:774 with `transhuge_vma_suitable`, huge_mm.h:100). Without a free block the fault
//! takes 4 KiB pages and the 64 KiB fault-around (Linux's VM_FAULT_FALLBACK). The fault path zeroes the block
//! without the space's lock and looks again (MM-tlb2's rule for the 4 KiB spare frame).
//!
//! Frames: a mapped huge frame is either one never-split block (only this space maps it) or 512 order-0 user
//! frames with their own share counts (`mm_core::split_user_block`): a fork splits the block's frames (not the
//! entries) before it shares them, so after a fork both spaces keep 2 MiB entries over 4 KiB-counted frames,
//! read-only (Linux `copy_huge_pmd`:1062). A write to such a read-only entry merges the frames back into one
//! block when this space is again their only holder and keeps the 2 MiB entry (the fork+exec case; Linux
//! `do_huge_pmd_wp_page` reuse), otherwise splits the entry and copies the one 4 KiB page (6.6's
//! `__split_huge_pmd`:1371 fallback). Unmapping a 2 MiB entry frees the block, or releases the 512 frames.
//!
//! Splits (`split_huge_at`, Linux `__split_huge_pmd_locked`:2066): an entry that a munmap, mprotect, madvise,
//! mremap or MAP_FIXED cuts (and every VMA split at an unaligned address) becomes 512 4 KiB entries over the same
//! frames, so the 4 KiB paths never meet a partial large leaf. The page table for it was taken when the entry
//! was made (`Inner::deposit`, Linux's `pgtable_trans_huge_deposit`; counted in VmPTE as Linux's nr_ptes), so a
//! split never fails for want of memory. V2 has no anonymous swap and reclaims only page-cache pages, so no
//! reclaim path meets a huge page; no khugepaged (no background collapse).
//!
//! Commit (MM-5b): nothing changes, as for 4 KiB pages: a VM_ACCOUNT VMA was charged for every page when
//! mapped, and a huge page lies wholly inside one VMA. TLB: one invlpg per 2 MiB entry (`tlb::flush_user_huge`).

use super::{page_flags, AsCore, Class, Fill, Inner, UNCONFIRMED};
use crate::fault::FaultReason;
use crate::vma::{Vma, VmaKind};
use crate::{share, thp, PAGE};
use alloc::vec::Vec;
use core::sync::atomic::Ordering::Relaxed;
use hal::paging::{self, tlb, Leaf, PageSize};
use mm_core::HUGE_ORDER;

/// Bytes and 4 KiB pages of a huge page.
pub(crate) const HUGE: u64 = 2 << 20;
pub(crate) const HUGE_PAGES: u64 = 512;

/// Is the frame of a 2 MiB leaf (`head`, 2 MiB aligned) the program's own: a never-split block, or 512 user frames?
pub(crate) fn own_huge(head: u64) -> bool {
    mm_core::is_user_block(head, HUGE_ORDER) || mm_core::is_user_frame(head)
}

/// This space no longer maps the 2 MiB leaf at `head` (entry gone, shootdown confirmed): a block is freed whole
/// (only this space can hold one); split frames go one by one to their last holder.
pub(crate) fn release_huge(head: u64) {
    if mm_core::is_user_block(head, HUGE_ORDER) {
        thp::block_gone();
        if let Some(fa) = mm_core::frames() {
            fa.free(head, HUGE_ORDER, mm_core::Kind::User);
        }
    } else {
        for i in 0..HUGE_PAGES {
            super::release_user(head + i * PAGE);
        }
    }
}

/// Could a VMA like `v` get huge pages at all (ignoring the mode and the space's switch)?
pub(crate) fn huge_kind(v: &Vma) -> bool {
    v.file.is_none() && v.kind != VmaKind::Stack && !v.flags.growsdown && v.prot.write
}

impl AsCore {
    /// PR_SET_THP_DISABLE of the program (Linux MMF_DISABLE_THP; fork copies it, AB sets it on the new space of
    /// an exec, which keeps it on Linux too). Huge pages already mapped stay.
    pub fn set_thp_disabled(&self, off: bool) {
        self.thp_off.store(off, Relaxed);
    }

    pub fn thp_disabled(&self) -> bool {
        self.thp_off.load(Relaxed)
    }

    /// 2 MiB entries this space maps now.
    pub fn huge_pages(&self) -> u64 {
        self.ctr.huge()
    }

    /// smaps' THPeligible of `v` (Linux `hugepage_vma_check(.., smaps)`, huge_memory.c:74: the mode, VM_NOHUGEPAGE,
    /// PR_SET_THP_DISABLE, anonymous memory, an aligned 2 MiB inside): whether a write there may take a huge page.
    /// V2 never maps one in a stack or MAP_GROWSDOWN VMA (MM-12's rule), so they answer 0.
    pub fn thp_eligible(&self, v: &Vma) -> bool {
        let base = v.start.next_multiple_of(HUGE);
        v.file.is_none()
            && v.kind != VmaKind::Stack
            && !v.flags.growsdown
            && !self.thp_disabled()
            && thp::mode_allows(v.flags.hugepage, v.flags.nohugepage)
            && base.checked_add(HUGE).is_some_and(|e| e <= v.end)
    }

    /// ktests only: no 2 MiB block is free for this space's faults (`on`), as when memory is fragmented.
    #[doc(hidden)]
    pub fn test_fail_huge(&self, on: bool) {
        self.inner.lock().huge_fail = on;
    }

    /// The 2 MiB-aligned start for a huge page at `page` of `v` when one may be made there now: the mode and the
    /// space allow it (`thp::mode_allows`, PR_SET_THP_DISABLE), `v` is private anonymous memory (`huge_kind`),
    /// the aligned 2 MiB lies inside `v` and nothing is mapped in its page-directory slot. A slot that holds a
    /// page table (left by a fallback, a partial munmap or DONTNEED) stays 4 KiB for good, as Linux 6.6's fault
    /// takes a huge page only on `pmd_none` (linux-6.6/mm/memory.c:5091) and leaves the rest to khugepaged, which
    /// V2 does not have: counted in `mm.thp.slot_busy` so the desk sees how often it costs a huge page.
    pub(crate) fn huge_base(&self, g: &Inner, v: &Vma, page: u64) -> Option<u64> {
        if g.fill != Fill::OnFault || self.thp_disabled() || !huge_kind(v) || !thp::mode_allows(v.flags.hugepage, v.flags.nohugepage) {
            return None;
        }
        let base = page & !(HUGE - 1);
        if base < v.start || base.checked_add(HUGE).is_none_or(|e| e > v.end) {
            return None;
        }
        if !paging::huge_slot_free(self.root, base) {
            thp::SLOT_BUSY.inc();
            return None;
        }
        Some(base)
    }

    /// Maps the zeroed 2 MiB block `frame` at `base` of `v` with the page table its split will need. Err(frame)
    /// when a table does not fit (the caller frees the block or keeps it).
    pub(crate) fn map_huge(&self, g: &mut Inner, base: u64, v: &Vma, frame: u64) -> Result<(), u64> {
        if g.deposit.try_reserve(1).is_err() {
            return Err(frame);
        }
        let Some(t) = self.alloc_table() else { return Err(frame) };
        if paging::map_user_2m(self.root, base, frame, page_flags(v.prot), &mut || self.alloc_table()).is_err() {
            self.free_table_now(t);
            return Err(frame);
        }
        g.deposit.push(t);
        self.ctr.add(Class::Anon, HUGE_PAGES);
        self.ctr.huge_add(1);
        thp::block_mapped();
        thp::FAULT_ALLOC.inc();
        self.anon_hint.store(true, Relaxed);
        Ok(())
    }

    /// A huge page at `base` of `v` made under the lock (user copies, MAP_POPULATE, MADV_POPULATE_WRITE: the
    /// call's own thread waits for it anyway). False: no free block (or the hook says so), the caller fills 4 KiB.
    pub(crate) fn fill_huge_now(&self, g: &mut Inner, base: u64, v: &Vma) -> bool {
        let f = if g.huge_fail { None } else { mm_core::alloc_zeroed_huge_light(mm_core::Kind::User) };
        let Some(f) = f else {
            thp::FAULT_FALLBACK.inc();
            return false;
        };
        match self.map_huge(g, base, v, f) {
            Ok(()) => true,
            Err(f) => {
                free_block(f);
                thp::FAULT_FALLBACK.inc();
                false
            }
        }
    }

    /// A page table that never was reachable from the root (a deposit): freed at once.
    pub(crate) fn free_table_now(&self, t: u64) {
        self.ctr.tables_freed(1);
        if let Some(fa) = mm_core::frames() {
            fa.free(t, 0, mm_core::Kind::PageTable);
        }
    }

    /// Splits the 2 MiB leaf that maps `va` (if one does) into 512 4 KiB entries over the same frames, which become
    /// 512 order-0 user frames. False only when no page table was at hand (an event; the leaf stays whole).
    pub(crate) fn split_huge_at(&self, g: &mut Inner, va: u64, cause: tlb::Cause) -> bool {
        if va >= hal::USER_TOP {
            return true;
        }
        let Some(l) = paging::translate_leaf(self.root, va).filter(|l| l.size == PageSize::Size2M) else { return true };
        let (base, head) = (l.page_virt, l.phys & !(HUGE - 1));
        let Some(pt) = g.deposit.pop().or_else(|| self.alloc_table()) else {
            klog::event!(Error, "mm", "thp.split_no_table", addr = klog::Hex(base); "no page table to split a huge page: it stays whole");
            return false;
        };
        if mm_core::split_user_block(head, HUGE_ORDER) {
            thp::block_gone();
            thp::SPLIT_PAGE.inc();
        }
        match paging::split_user_2m(self.root, base, pt, cause) {
            Ok(o) => {
                self.ctr.huge_sub(1);
                thp::SPLIT_PMD.inc();
                if !o.confirmed() {
                    UNCONFIRMED.inc();
                    klog::event!(Warn, "mm", "as.shootdown_unconfirmed", root = klog::Hex(self.root), start = klog::Hex(base),
                        end = klog::Hex(base + HUGE), cpus = o.unanswered, resends = o.resends, not_ready = o.not_ready;
                        "a CPU did not answer the TLB shootdown of a huge page split: its pending IPI flushes before it runs user code again");
                }
                true
            }
            Err(e) => {
                // Cannot happen under the lock (the leaf was just found): the table goes back.
                g.deposit.push(pt);
                klog::event!(Error, "mm", "thp.split_failed", addr = klog::Hex(base), error = alloc::format!("{e:?}"));
                false
            }
        }
    }

    /// Splits the 2 MiB leaves that [start, end) cuts (at most the one around each unaligned end), before a
    /// change of only that range (Linux's `split_huge_pmd` in zap, mprotect, madvise, mremap and `__split_vma`).
    pub(crate) fn split_edges(&self, g: &mut Inner, start: u64, end: u64, cause: tlb::Cause) {
        if self.ctr.huge() == 0 || start >= end {
            return;
        }
        if !start.is_multiple_of(HUGE) {
            self.split_huge_at(g, start, cause);
        }
        if !end.is_multiple_of(HUGE) {
            self.split_huge_at(g, end, cause);
        }
    }

    /// Splits every 2 MiB leaf in [start, end) (an mremap to an address with another offset in 2 MiB). The leaves
    /// are found by a walk that skips empty directories, so a huge, sparse range costs its tables, not one look per
    /// 2 MiB slot (B4: the space lock is held). Without heap for the list, it goes in rounds of what fitted.
    pub(crate) fn split_range(&self, g: &mut Inner, start: u64, end: u64, cause: tlb::Cause) {
        let mut from = start & !(HUGE - 1);
        while from < end && self.ctr.huge() > 0 {
            let (mut found, mut full) = (Vec::new(), false);
            paging::for_each_leaf(self.root, from, end, &mut |u| {
                if u.size == PageSize::Size2M && !full {
                    if found.try_reserve(1).is_ok() {
                        found.push(u.virt);
                    } else {
                        full = true;
                    }
                }
            });
            let Some(&last) = found.last() else {
                if full {
                    klog::event!(Warn, "mm", "thp.split_range_slow", start = klog::Hex(from), end = klog::Hex(end);
                        "no heap for the list of huge pages: splitting slot by slot");
                    self.split_slots(g, from.max(start), end, cause);
                }
                return;
            };
            for va in found {
                self.split_huge_at(g, va.max(start), cause);
            }
            if !full {
                return;
            }
            // `last` + 2 MiB <= `end` <= USER_TOP: no overflow.
            from = last + HUGE;
        }
    }

    /// The fallback of `split_range` without heap: one look per 2 MiB slot, while huge pages are left.
    fn split_slots(&self, g: &mut Inner, start: u64, end: u64, cause: tlb::Cause) {
        let mut va = start & !(HUGE - 1);
        while va < end && self.ctr.huge() > 0 {
            self.split_huge_at(g, va.max(start), cause);
            let Some(n) = va.checked_add(HUGE) else { return };
            va = n;
        }
    }

    /// A write to the read-only 2 MiB leaf `l` (at `va`) of `v`, which allows writes. Ok(true): done, the entry is
    /// writable and whole again (a block this space alone holds; or 512 frames it alone holds after a fork,
    /// merged back into a block). Ok(false): the entry was split, the caller handles the 4 KiB page.
    pub(crate) fn huge_write(&self, g: &mut Inner, va: u64, v: &Vma, l: Leaf) -> Result<bool, FaultReason> {
        let (base, head) = (l.page_virt, l.phys & !(HUGE - 1));
        let alone = mm_core::is_user_block(head, HUGE_ORDER)
            || ((0..HUGE_PAGES).all(|i| share::exclusive(head + i * PAGE)) && mm_core::merge_user_block(head, HUGE_ORDER) && {
                thp::block_mapped();
                thp::MERGED.inc();
                true
            });
        if alone && v.file.is_none() {
            let fl = page_flags(v.prot);
            paging::grant_user(self.root, base, HUGE, &mut |_| Some(fl)).map_err(|_| FaultReason::Repeated)?;
            thp::COW_REUSE.inc();
            return Ok(true);
        }
        if !self.split_huge_at(g, va, tlb::Cause::Cow) {
            return Err(FaultReason::NoMemory);
        }
        thp::COW_SPLIT.inc();
        Ok(false)
    }
}

/// A 2 MiB block nobody mapped (made for a fault another thread served first).
pub(crate) fn free_block(f: u64) {
    if let Some(fa) = mm_core::frames() {
        fa.free(f, HUGE_ORDER, mm_core::Kind::User);
    }
}

/// Is the present leaf (phys, size) a frame only this space holds, which may simply become writable again?
pub(crate) fn own_exclusive(phys: u64, size: PageSize) -> bool {
    if size == PageSize::Size2M {
        mm_core::is_user_block(phys & !(HUGE - 1), HUGE_ORDER)
    } else {
        mm_core::is_user_frame(phys) && share::exclusive(phys)
    }
}

