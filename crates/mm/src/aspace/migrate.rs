//! W35 MM-2: migration of anonymous frames (`Kind::User`: anonymous memory and private copies), Linux 7.x
//! mm/migrate.c `migrate_folio_unmap` / `migrate_folio_move` with mm/rmap.c `try_to_migrate_one` and
//! `remove_migration_ptes`.
//!
//! Reverse map: the frame's owner word holds (space id, page address) of the space that mapped it last
//! (`note_anon`; `spaces`), so a frame only one space maps (share count 0, the usual case) is found with one look.
//! A frame fork shares (`share::others` > 0), or one whose owner went (fork, then the parent ended), is found by a
//! walk over the live spaces at the same address (fork keeps addresses; Linux's anon_vma chain is the reference:
//! the walk costs one try-lock and one page-table look per live space and runs only for those frames). Every
//! holder must be found and locked, else nothing moves.
//!
//! Protocol, with every holder's `mm.as` held (try-locks only: the caller is compaction, maybe inside an
//! allocation): each holder's 4 KiB leaf becomes a migration entry (`paging::freeze_user_leaf`, a 2 MiB leaf over
//! split frames is split first, MM-13), the shootdowns must be confirmed (else the leaves go back and nothing
//! moves), the frame is copied into the target, the leaves come back over the target with their rights and
//! Accessed/Dirty bits (`thaw_user_leaf`), the share count moves to the target and the old frame is freed (no CPU
//! can reach it any more). A fault on the page meanwhile waits for `mm.as` (bounded: one copy and one shootdown);
//! once a thread sleeps for a holder's `mm.as` no further migration takes it (`waited_for`): a program's fault,
//! mmap or munmap waits for the migration in flight at most, never for the scanner's whole run through its memory.
//! mlocked pages migrate unless `vm.compact_unevictable_allowed` is 0 (Linux's default 1).

use super::spaces;
use super::{AsCore, Inner};
use crate::share;
use alloc::sync::Arc;
use alloc::vec::Vec;
use hal::paging::{self, tlb, PageSize};
use ksync::MutexGuard;
use core::sync::atomic::{AtomicBool, Ordering::Relaxed};
use mm_core::{FrameAlloc, Kind, Target, HUGE_ORDER};

const PAGE: u64 = 4096;
const HUGE: u64 = 2 << 20;

/// What became of one anonymous frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Anon {
    /// Copied into the target; the old frame is free.
    Moved,
    /// No owner word (never noted, or its space had no id).
    NoOwner,
    /// A holder's lock was taken (never waited for), or a thread of the holder sleeps for it (`YIELDED`).
    Busy,
    /// Not every holder was found (the frame moved, was freed or is held where no entry maps it).
    Elsewhere,
    /// In an mlocked VMA while `vm.compact_unevictable_allowed` is 0.
    Locked,
    /// A shootdown was not confirmed: the leaves went back.
    Unconfirmed,
    /// No target frame, or no room for the target's share count or the holder list.
    NoTarget,
}

/// One space that maps the frame, locked.
struct Holder<'a> {
    a: &'a AsCore,
    g: MutexGuard<'a, Inner>,
}

impl AsCore {
    /// This space maps the anonymous frame `phys` at `va` now: its owner word says so (W35 MM-2). Only for
    /// `Kind::User` frames (a page-cache frame's word is the page cache's).
    pub(crate) fn note_anon(&self, phys: u64, va: u64) {
        mm_core::reclaim::set_frame_owner(phys, self.anon_word(va));
    }

    /// The owner word `note_anon` gives a frame mapped at `va` here.
    pub(crate) fn anon_word(&self, va: u64) -> u64 {
        spaces::word(self.id, va)
    }

    /// `note_anon` for every program frame mapped in [lo, hi) (after mremap moved them).
    pub(crate) fn note_anon_range(&self, lo: u64, hi: u64) {
        paging::for_each_leaf(self.root, lo, hi, &mut |u| {
            if u.size == PageSize::Size2M && mm_core::is_user_block(u.phys, HUGE_ORDER) {
                self.note_anon(u.phys, u.virt);
            } else if u.size == PageSize::Size4K && mm_core::is_user_frame(u.phys) {
                self.note_anon(u.phys, u.virt);
            }
        });
    }

    /// Whether the 4 KiB page `va` maps frame `phys` (a 2 MiB leaf over split frames counts: it is split before
    /// the move).
    fn maps(&self, va: u64, phys: u64) -> bool {
        paging::translate(self.root, va).is_some_and(|(p, _)| p & !(PAGE - 1) == phys)
    }

    /// The mlock rule for page `va` (`vm.compact_unevictable_allowed`).
    fn may_move(g: &Inner, va: u64) -> bool {
        mm_core::compact::unevictable_allowed() != 0 || g.space.tree.find(va).is_none_or(|v| !v.flags.locked && !v.flags.lock_onfault)
    }
}

/// Moves the anonymous order-0 frame `phys` with owner word `owner` into a target from `dest` (see the module
/// comment). The target is this call's from the moment `dest` hands it out: it becomes the page's or drops.
pub(crate) fn migrate_anon<'b>(phys: u64, owner: u64, dest: &mut dyn FnMut() -> Option<Target<'b>>) -> Anon {
    let (id, va) = spaces::unword(owner);
    if id == 0 {
        return Anon::NoOwner;
    }
    let mut cands: Vec<Arc<AsCore>> = Vec::new();
    let r = find_and_move(&mut cands, owner, va, phys, dest);
    cands.into_iter().for_each(release);
    r
}

/// The references to address spaces migrations let go of: dropped by kswapd every period, by kcompactd and
/// compact_memory after their runs and by the test entry points (`drop_dying`, thread context), never inside the
/// allocation that compacts, whose locks the last reference's teardown may need (a file mapping's pins). One slot
/// per space id: a reference let go while its space's slot is filled is never the last (the slot keeps one), so it
/// just goes; else it fills the slot. Whatever the counts say at the moment (two holders letting go at once cannot
/// both see themselves as not the last), no migration ever ends a space. Slots are made before the reference is
/// taken (`reserve`), so `release` never allocates; R1: as many slots as the space table has ids.
struct Dying {
    slots: Vec<Option<Arc<AsCore>>>,
    filled: usize,
}

static DYING: ksync::SpinLock<Dying> = ksync::SpinLock::named("mm.migrate.dying", Dying { slots: Vec::new(), filled: 0 });

/// Slots for every space id up to `top` (the space table's length); false: no heap for them (take no reference).
/// The list grows with its lock dropped (`ksync::Spare`; desk review of MM-2 x FS-W2, #4).
fn reserve(top: usize) -> bool {
    let mut spare = ksync::Spare::new();
    loop {
        let mut d = DYING.lock();
        let have = d.slots.len();
        if have >= top {
            return true;
        }
        if !spare.fits(&mut d.slots, top - have) {
            drop(d);
            if spare.grow().is_err() {
                return false;
            }
            continue;
        }
        d.slots.resize_with(top, || None);
        return true;
    }
}

/// Lets go of a space a migration upgraded (its slot exists: `reserve` came first).
fn release(a: Arc<AsCore>) {
    let i = (a.id as usize).wrapping_sub(1);
    let mut d = DYING.lock();
    let Some(slot) = d.slots.get_mut(i) else {
        // Unreachable (a space without id is never upgraded by a migration): keep the rule, never drop here.
        core::mem::forget(a);
        return;
    };
    if slot.is_none() {
        *slot = Some(a);
        d.filled += 1;
    } else {
        // The slot holds the same space: this reference is not the last.
        drop(a);
    }
}

/// The references `release` kept go now. Only from thread context with no lock held (kswapd, kcompactd,
/// compact_memory, the `mm::migrate` test entry points).
pub(crate) fn drop_dying() {
    if DYING.lock().filled == 0 {
        return;
    }
    let mut i = 0;
    loop {
        let a = {
            let mut d = DYING.lock();
            if i >= d.slots.len() || d.filled == 0 {
                return;
            }
            let a = d.slots[i].take();
            if a.is_some() {
                d.filled -= 1;
            }
            a
        };
        drop(a);
        i += 1;
    }
}

fn find_and_move<'b>(cands: &mut Vec<Arc<AsCore>>, owner: u64, va: u64, phys: u64, dest: &mut dyn FnMut() -> Option<Target<'b>>) -> Anon {
    let id = spaces::unword(owner).0;
    if cands.try_reserve(1).is_err() || !reserve(id as usize) {
        return Anon::NoTarget;
    }
    cands.extend(spaces::get(id));
    match lock_holders(cands, va, phys, owner) {
        Ok((mut hs, others)) => return move_locked(&mut hs, va, phys, others, dest),
        Err(Anon::Elsewhere) if still_anon(phys, owner) => {}
        Err(r) => return r,
    }
    // The owner alone did not hold it (fork-shared, the owner gone or the page moved in it): every other live space
    // at the same address (fork keeps addresses).
    let Some(rest) = spaces::all_but(id) else { return Anon::NoTarget };
    let top = rest.iter().map(|(i, _)| *i as usize).max().unwrap_or(0);
    if cands.try_reserve(rest.len()).is_err() || !reserve(top) {
        return Anon::NoTarget;
    }
    cands.extend(rest.iter().filter_map(|(_, w)| w.upgrade()));
    WALKS.inc();
    match lock_holders(cands, va, phys, owner) {
        Ok((mut hs, others)) => move_locked(&mut hs, va, phys, others, dest),
        Err(r) => r,
    }
}

/// Whether a thread sleeps for `a`'s lock (a fault, an mmap, an munmap of the program): then the migration stands
/// back, as if the lock were taken. The scanner moves frame after frame of one space, taking and releasing its
/// lock each time; a sleeper woken by a release needs longer to run than the scanner needs to take the lock for
/// the next frame, so without this a program's thread waited until the scanner left its memory (measured: 1613
/// migrations in front of one mprotect call, 106 ms). Linux's async compaction stops after the block when one of
/// its locks is contended (mm/compaction.c `compact_lock_irqsave` :506, `cc->contended` :2386); its migration
/// never holds the mmap lock at all.
fn waited_for(a: &AsCore) -> bool {
    let w = a.inner.contended();
    if w {
        YIELDED.inc();
    }
    w
}

/// Locks the candidates that map `phys` at `va` (try-locks; a space that maps it elsewhere or not at all is let go at
/// once) until they are all of the frame's holders: as many as its share count says (`share::others` + 1), read
/// with every holder found so far locked, so it cannot change (a new holder needs a fork of a locked one, a holder
/// that leaves needs its own lock); a reference no mapping explains keeps the count above and the frame where it is.
/// A taken lock of a space that turns out not to hold the frame does not stop the move. Ok: the holders, locked,
/// and the share count.
///
/// The scanner read `phys`'s kind and `owner` without a lock and nothing pins the frame (Linux isolates a folio
/// with a reference first): meanwhile it may have been freed and handed out again, as a page-cache or device frame
/// the same space maps at the same address. So with the first holder locked the frame must still be a
/// `Kind::User` frame with that owner word; a user frame a locked holder maps cannot be freed or change its kind
/// before that lock goes, so one look is enough. Without it only the target's copy (`Target::copy_from` checks the
/// source's kind) kept such a frame where it was, after its leaf had been frozen and shot down for nothing.
fn lock_holders<'a>(cands: &'a [Arc<AsCore>], va: u64, phys: u64, owner: u64) -> Result<(Vec<Holder<'a>>, usize), Anon> {
    let mut hs = Vec::new();
    let mut busy = false;
    for a in cands {
        if waited_for(a) {
            busy = true;
            continue;
        }
        let Some(g) = a.inner.try_lock() else {
            busy = true;
            continue;
        };
        if !a.maps(va, phys) {
            continue;
        }
        if hs.is_empty() && !still_anon(phys, owner) {
            return Err(Anon::Elsewhere);
        }
        if !AsCore::may_move(&g, va) {
            return Err(Anon::Locked);
        }
        if hs.try_reserve(1).is_err() {
            return Err(Anon::NoTarget);
        }
        hs.push(Holder { a, g });
        let others = share::others(phys) as usize;
        if hs.len() == others + 1 {
            return Ok((hs, others));
        }
        if hs.len() > others + 1 {
            return Err(Anon::Elsewhere);
        }
    }
    Err(if busy { Anon::Busy } else { Anon::Elsewhere })
}

/// Whether `phys` is still the anonymous frame the scanner saw (`lock_holders`).
fn still_anon(phys: u64, owner: u64) -> bool {
    mm_core::is_user_frame(phys) && mm_core::reclaim::frame_owner(phys) == owner
}

/// The move itself, every holder of `phys` (mapped at `va`, `others` share counts) locked.
fn move_locked<'b>(hs: &mut [Holder<'_>], va: u64, phys: u64, others: usize, dest: &mut dyn FnMut() -> Option<Target<'b>>) -> Anon {
    // A 2 MiB leaf over split frames (a fork's) becomes 512 4 KiB leaves first (MM-13's split).
    for h in hs.iter_mut() {
        if paging::translate_leaf(h.a.root, va).is_some_and(|l| l.size == PageSize::Size2M) && !h.a.split_huge_at(&mut h.g, va, tlb::Cause::Migrate) {
            return Anon::NoTarget;
        }
    }
    let Some(t) = dest() else { return Anon::NoTarget };
    let new = t.phys();
    let undo_shares = |n: usize| {
        (0..n).for_each(|_| {
            let _ = share::release(new);
        })
    };
    // The target's share counts first: a failure there changes nothing a program sees (the target drops).
    let shared = (0..others).take_while(|_| share::share(new)).count();
    if shared < others {
        undo_shares(shared);
        return Anon::NoTarget;
    }
    let mut frozen = 0usize;
    let mut failed = None;
    for h in hs.iter() {
        match paging::freeze_user_leaf(h.a.root, va, PageSize::Size4K, phys) {
            Ok((_, o)) => {
                frozen += 1;
                if !o.confirmed() {
                    super::UNCONFIRMED.inc();
                    klog::event!(Warn, "mm", "migrate.shootdown_unconfirmed", root = klog::Hex(h.a.root), addr = klog::Hex(va), cpus = o.unanswered;
                        "a CPU did not answer the shootdown of a migration: the page stays where it is");
                    failed = Some(Anon::Unconfirmed);
                    break;
                }
            }
            // Cannot happen (the leaf was checked under the lock): the frame is not where the holder maps it.
            Err(_) => {
                failed = Some(Anon::Elsewhere);
                break;
            }
        }
    }
    let copied = failed.is_none() && t.copy_from(phys);
    let to = if copied { new } else { phys };
    for h in hs.iter().take(frozen) {
        // Cannot fail: the leaf was frozen under the lock just now.
        let _ = paging::thaw_user_leaf(h.a.root, va, PageSize::Size4K, to);
    }
    if !copied {
        undo_shares(others);
        return failed.unwrap_or(Anon::NoTarget);
    }
    // The page tables hold the target now.
    let new = t.into_phys();
    for _ in 0..others {
        let _ = share::release(phys);
    }
    hs[0].a.note_anon(new, va);
    super::free_user(phys);
    Anon::Moved
}

/// What became of a huge page (`migrate_huge`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Huge {
    /// Copied whole into a free 2 MiB block.
    Whole,
    /// No free 2 MiB block: split (MM-13) and its 4 KiB pages moved one by one; how many moved.
    Split(u64),
    /// Not moved (why, as for a 4 KiB page).
    Stays(Anon),
}

/// Moves the huge page (a never-split 2 MiB user block at `head`, owner word set when it was mapped) of the space
/// that maps it: whole into a free 2 MiB block when one is free now, else split and moved page by page with
/// targets from `dest` (Linux `migrate_pages_batch`: a large folio whose target allocation fails is split and its
/// pages migrated). Compaction never hands one over (a block at pageblock order, Linux `skip_isolation_on_order`);
/// this is for callers that must empty a range. `whole`: false splits even when a 2 MiB block is free (tests).
pub(crate) fn migrate_huge<'b>(head: u64, owner: u64, whole: bool, dest: &mut dyn FnMut() -> Option<Target<'b>>) -> Huge {
    let (id, base) = spaces::unword(owner);
    if id == 0 || !reserve(id as usize) {
        return Huge::Stays(Anon::NoOwner);
    }
    let Some(a) = spaces::get(id) else { return Huge::Stays(Anon::NoOwner) };
    let r = move_huge(&a, head, base, whole, dest);
    release(a);
    r
}

/// Once per episode (B6): the first unconfirmed shootdown of a whole huge page is told, until one moves again.
static HUGE_TOLD: AtomicBool = AtomicBool::new(false);

fn move_huge<'b>(a: &AsCore, head: u64, base: u64, whole: bool, dest: &mut dyn FnMut() -> Option<Target<'b>>) -> Huge {
    if waited_for(a) {
        return Huge::Stays(Anon::Busy);
    }
    let Some(mut g) = a.inner.try_lock() else { return Huge::Stays(Anon::Busy) };
    let mapped = paging::translate_leaf(a.root, base).is_some_and(|l| l.size == PageSize::Size2M && l.phys == head);
    if !mapped || !base.is_multiple_of(HUGE) || !mm_core::is_user_block(head, HUGE_ORDER) {
        return Huge::Stays(Anon::Elsewhere);
    }
    if !AsCore::may_move(&g, base) {
        return Huge::Stays(Anon::Locked);
    }
    let Some(b) = mm_core::boot_buddy() else { return Huge::Stays(Anon::NoTarget) };
    if let Some(t) = if whole { b.alloc_target_light(HUGE_ORDER, mm_core::Zone::Normal, Kind::User) } else { None } {
        let Ok((_, o)) = paging::freeze_user_leaf(a.root, base, PageSize::Size2M, head) else { return Huge::Stays(Anon::Elsewhere) };
        let copied = o.confirmed() && t.copy_from(head);
        let _ = paging::thaw_user_leaf(a.root, base, PageSize::Size2M, if copied { t.phys() } else { head });
        if copied {
            let nb = t.into_phys();
            a.note_anon(nb, base);
            // The huge page lives on in `nb` (the THP count stays): the old block goes back as it is.
            mm_core::reclaim::set_frame_owner(head, 0);
            FrameAlloc::free(b, head, HUGE_ORDER, Kind::User);
            if HUGE_TOLD.load(Relaxed) {
                HUGE_TOLD.store(false, Relaxed);
            }
            return Huge::Whole;
        }
        if !o.confirmed() {
            super::UNCONFIRMED.inc();
            if !HUGE_TOLD.swap(true, Relaxed) {
                klog::event!(Warn, "mm", "migrate.huge_unconfirmed", root = klog::Hex(a.root), addr = klog::Hex(base), cpus = o.unanswered;
                    "a CPU did not answer the shootdown of a huge page's migration: it stays where it is (told once until one moves)");
            }
            return Huge::Stays(Anon::Unconfirmed);
        }
        return Huge::Stays(Anon::NoTarget);
    }
    if !a.split_huge_at(&mut g, base, tlb::Cause::Migrate) {
        return Huge::Stays(Anon::NoTarget);
    }
    let mut moved = 0;
    for i in 0..super::huge::HUGE_PAGES {
        let (va, phys) = (base + i * PAGE, head + i * PAGE);
        let mut hs = [Holder { a, g }];
        let r = move_locked(&mut hs, va, phys, 0, dest);
        let [Holder { g: back, .. }] = hs;
        g = back;
        if r != Anon::Moved {
            break;
        }
        moved += 1;
    }
    drop(g);
    Huge::Split(moved)
}

klog::counter!(pub(crate) WALKS, "mm.migrate.anon_walks");
klog::counter!(pub(crate) YIELDED, "mm.migrate.anon_yielded");
