//! W35 MM-2: page migration (Linux 7.x mm/migrate.c `migrate_pages`), the real `mm_core::compact::Migrate`
//! behind compaction (MM-3's scanners hand over one order-0 frame of a Movable pageblock at a time with a target
//! from the free scanner, Linux `compaction_alloc`).
//!
//! - Page-cache frames (`Kind::PageCache`): vfs `reclaim::migrate_frame` (CONTRACTS BL-3/MM-2): a clean page only
//!   the index holds gets a new `CachePage` on the target, which replaces the index entry under the file's lock;
//!   the old page leaves through PF-5's grace queue (Linux `filemap_migrate_folio` / `folio_migrate_mapping`).
//!   Dirty pages, pages under I/O, held by a read or a write-back batch, and pages a mapping pins stay (counted).
//! - Anonymous frames (`Kind::User`): `aspace::migrate` (owner word reverse map, migration entries, one
//!   confirmed shootdown per holder, copy, the leaves back over the target; Linux `try_to_migrate_one`,
//!   `remove_migration_ptes`).
//! - Huge pages: never handed over by compaction (Linux `skip_isolation_on_order`: a block at pageblock order);
//!   `migrate_huge` moves one whole into a free 2 MiB block or splits it and moves its pages.
//! - Nothing else moves: kernel kinds are in Unmovable pageblocks, `Kind::Dma` (MM-14's pinned buffers) is
//!   Unmovable, a drm userptr's pages or any reference beyond the holders' (a share count no entry explains) keep
//!   the frame where it is.
//!
//! Counters (`mm.migrate.*`) count per frame: migration is compaction's slow path (each move costs a copy and a
//! shootdown), never the allocation fast path.

use crate::aspace::migrate::{self as am, Anon, Huge};
use mm_core::compact::{Migrate, Moved};
use core::sync::atomic::{AtomicBool, Ordering::Relaxed};
use mm_core::{Kind, Target};
use vfs::reclaim::{self as vr, Migrated};

klog::counter!(pub CACHE_MOVED, "mm.migrate.cache_moved");
klog::counter!(pub CACHE_DIRTY, "mm.migrate.cache_dirty");
klog::counter!(pub CACHE_HELD, "mm.migrate.cache_held");
klog::counter!(pub CACHE_BUSY, "mm.migrate.cache_busy");
klog::counter!(pub CACHE_STALE, "mm.migrate.cache_stale");
klog::counter!(pub ANON_MOVED, "mm.migrate.anon_moved");
klog::counter!(pub ANON_BUSY, "mm.migrate.anon_busy");
klog::counter!(pub ANON_ELSEWHERE, "mm.migrate.anon_elsewhere");
klog::counter!(pub ANON_LOCKED, "mm.migrate.anon_mlocked");
klog::counter!(pub ANON_NO_OWNER, "mm.migrate.anon_no_owner");
klog::counter!(pub UNCONFIRMED, "mm.migrate.unconfirmed");
klog::counter!(pub NO_TARGET, "mm.migrate.no_target");
klog::counter!(pub HUGE_WHOLE, "mm.migrate.huge_whole");
klog::counter!(pub HUGE_SPLIT, "mm.migrate.huge_split");
klog::counter!(pub HUGE_FAIL, "mm.migrate.huge_fail");

/// Once per episode (B6): the first page-cache page without a target is told, until one moves again (a run that
/// ran out of targets meets every following page the same way).
static NO_TARGET_TOLD: AtomicBool = AtomicBool::new(false);

/// The migrator compaction uses (installed on the boot allocator at start, `install`).
pub struct Migrator;

pub static MIGRATOR: Migrator = Migrator;

impl Migrate for Migrator {
    fn migrate<'b>(&self, phys: u64, kind: Kind, owner: u64, dest: &mut dyn FnMut() -> Option<Target<'b>>) -> Moved {
        match kind {
            Kind::PageCache if owner != 0 => cache(phys, owner, dest),
            Kind::PageCache => Moved::Stays,
            Kind::User => anon(phys, owner, dest),
            _ => Moved::Unsupported,
        }
    }
}

fn cache<'b>(phys: u64, owner: u64, dest: &mut dyn FnMut() -> Option<Target<'b>>) -> Moved {
    match vr::migrate_frame(phys, owner, dest) {
        Migrated::Moved => {
            CACHE_MOVED.inc();
            if NO_TARGET_TOLD.load(Relaxed) {
                NO_TARGET_TOLD.store(false, Relaxed);
            }
            return Moved::Moved;
        }
        Migrated::Dirty => CACHE_DIRTY.inc(),
        Migrated::Held => CACHE_HELD.inc(),
        Migrated::Busy => CACHE_BUSY.inc(),
        Migrated::Stale => CACHE_STALE.inc(),
        Migrated::NoTarget => {
            NO_TARGET.inc();
            if !NO_TARGET_TOLD.swap(true, Relaxed) {
                klog::event!(Info, "mm", "migrate.cache_no_target", phys = klog::Hex(phys);
                    "a page-cache page found no target frame: it stays (told once until a page moves again)");
            }
        }
    }
    Moved::Stays
}

/// An anonymous frame that cannot move for what holds it (`Unsupported`: the scanner counts it in
/// `mm.compact.skipped_anon`) or for now (`Stays`: a lock, a target, a CPU that did not answer).
fn anon<'b>(phys: u64, owner: u64, dest: &mut dyn FnMut() -> Option<Target<'b>>) -> Moved {
    match am::migrate_anon(phys, owner, dest) {
        Anon::Moved => {
            ANON_MOVED.inc();
            return Moved::Moved;
        }
        Anon::Busy => {
            ANON_BUSY.inc();
            return Moved::Stays;
        }
        Anon::NoTarget => {
            NO_TARGET.inc();
            return Moved::Stays;
        }
        Anon::Unconfirmed => {
            UNCONFIRMED.inc();
            return Moved::Stays;
        }
        Anon::Elsewhere => ANON_ELSEWHERE.inc(),
        Anon::Locked => ANON_LOCKED.inc(),
        Anon::NoOwner => ANON_NO_OWNER.inc(),
    }
    Moved::Unsupported
}

/// Moves the order-0 frame `phys` of `kind` (page cache or anonymous) into a new frame from the allocator (not
/// compaction's free scanner): the general `migrate_pages` for callers outside compaction (tests). Thread context
/// with no lock held (the spaces the move let go of may end here).
pub fn migrate_frame(phys: u64, kind: Kind) -> Moved {
    let Some(b) = mm_core::boot_buddy() else { return Moved::Unsupported };
    let mut dest = || b.alloc_target(0, mm_core::Zone::Normal, kind);
    let r = MIGRATOR.migrate(phys, kind, b.owner(phys), &mut dest);
    crate::aspace::migrate::drop_dying();
    r
}

/// What `migrate_huge` did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HugeMoved {
    Whole,
    /// Split; how many of its 512 pages moved.
    Split(u64),
    Stays,
}

/// Moves the huge page at `head` (a never-split 2 MiB user block): whole into a free 2 MiB block, else split and
/// its pages moved one by one into frames from the allocator (Linux `migrate_pages_batch` splits a large folio
/// whose target allocation fails).
pub fn migrate_huge(head: u64) -> HugeMoved {
    let Some(b) = mm_core::boot_buddy() else { return HugeMoved::Stays };
    let mut dest = || b.alloc_target(0, mm_core::Zone::Normal, Kind::User);
    let r = am::migrate_huge(head, b.owner(head), true, &mut dest);
    crate::aspace::migrate::drop_dying();
    match r {
        Huge::Whole => {
            HUGE_WHOLE.inc();
            HugeMoved::Whole
        }
        Huge::Split(n) => {
            HUGE_SPLIT.inc();
            ANON_MOVED.add(n);
            HugeMoved::Split(n)
        }
        Huge::Stays(_) => {
            HUGE_FAIL.inc();
            HugeMoved::Stays
        }
    }
}

/// The boot allocator's compaction moves pages from now on (MM-3's drop-if-clean is gone).
pub(crate) fn install() -> bool {
    mm_core::boot_buddy().is_some_and(|b| b.set_migrator(&MIGRATOR))
}
