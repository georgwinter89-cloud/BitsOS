//! Strict commit (MM-5b, V2-PLAN R2, Linux `overcommit_memory = 2`): memory a program may write is charged when
//! it is promised, not when it is touched, so a page fault never finds the pool empty and no running program is
//! ever killed for memory; the asking call gets ENOMEM instead.
//!
//! What is charged, as Linux's `accountable_mapping` (linux-6.6/mm/mmap.c:2699, `VM_ACCOUNT`) with mode 2's rule
//! that `MAP_NORESERVE` is ignored (mm/mmap.c:1342): every private writable mapping at `mmap` (anonymous or a
//! file's private copy), a private mapping that `mprotect` makes writable for the first time
//! (mm/mprotect.c:633), `brk` growth (mm/mmap.c `do_brk_flags`), `mremap` growth, every accounted mapping again
//! for a fork's child (kernel/fork.c `dup_mmap`), and tmpfs/`/dev/shm` pages through FS-5's pool
//! (`vfs::pool::set_commit`): a file's pages when they are allocated (FS-14, as Linux's shmem), shared anonymous
//! memory's object by its size at creation. PROT_NONE reservations and shared
//! mappings of files cost nothing. A charge stays until the mapping goes (an `mprotect` back to read-only keeps
//! it, as on Linux). The limit is all the frame allocator's memory minus `POOL_KEEP` (512 MiB the kernel keeps
//! for itself: heap, page tables, DMA, page cache of disks); small machines keep a quarter instead.
//!
//! All counters are atomics; nothing here sleeps or allocates, so charging may happen under any MM lock.

use core::sync::atomic::{AtomicU64, Ordering};

pub const PAGE: u64 = 4096;
/// What the system keeps for itself (R2).
pub const POOL_KEEP: u64 = 512 << 20;

/// A committed-pages counter with a limit (pure; the system's is `GLOBAL`).
pub struct Budget {
    committed: AtomicU64,
}

impl Budget {
    pub const fn new() -> Budget {
        Budget { committed: AtomicU64::new(0) }
    }

    pub fn committed(&self) -> u64 {
        self.committed.load(Ordering::Relaxed)
    }

    /// Adds `n` pages if the total stays at or below `limit` (all or nothing).
    pub fn charge(&self, n: u64, limit: u64) -> bool {
        self.committed.try_update(Ordering::AcqRel, Ordering::Relaxed, |c| c.checked_add(n).filter(|&a| a <= limit)).is_ok()
    }

    /// Adds `n` pages whatever the limit (taking over charges made elsewhere).
    pub fn force(&self, n: u64) {
        self.committed.fetch_add(n, Ordering::AcqRel);
    }

    /// Gives `n` pages back. False: more than was charged (a bookkeeping bug; the counter stops at 0).
    pub fn uncharge(&self, n: u64) -> bool {
        let r = self.committed.try_update(Ordering::AcqRel, Ordering::Relaxed, |c| Some(c.saturating_sub(n)));
        r.is_ok_and(|before| before >= n)
    }
}

impl Default for Budget {
    fn default() -> Self {
        Self::new()
    }
}

/// The limit in pages for `total` bytes of managed memory.
pub fn limit_for(total: u64) -> u64 {
    let keep = POOL_KEEP.min(total / 4);
    (total - keep) / PAGE
}

static GLOBAL: Budget = Budget::new();
/// The measured limit (0: not measured yet).
static LIMIT: AtomicU64 = AtomicU64::new(0);
/// A limit a test set (0: none); wins over the measured one.
static TEST_LIMIT: AtomicU64 = AtomicU64::new(0);

klog::counter!(REFUSED, "mm.commit.refused");
klog::counter!(UNDERFLOWS, "mm.commit.underflows");
/// The size of the last refused request and the total then (for the report).
static LAST_REFUSED: AtomicU64 = AtomicU64::new(0);
static REPORTED: AtomicU64 = AtomicU64::new(0);

fn measure() -> u64 {
    let total = mm_core::frames().map(|f| f.stats().total_bytes).unwrap_or(0);
    if total == 0 {
        // No frame allocator (host tests): only the per-space limits apply.
        return u64::MAX;
    }
    let l = limit_for(total);
    LIMIT.store(l, Ordering::Relaxed);
    l
}

/// The system's commit limit in pages (Linux `CommitLimit`).
pub fn limit() -> u64 {
    match TEST_LIMIT.load(Ordering::Relaxed) {
        0 => match LIMIT.load(Ordering::Relaxed) {
            0 => measure(),
            l => l,
        },
        t => t,
    }
}

/// Pages committed system-wide (Linux `Committed_AS`).
pub fn committed() -> u64 {
    GLOBAL.committed()
}

/// Charges `n` pages system-wide, all or nothing. A refusal is measured once more first: the managed memory
/// grows after boot (the released initramfs), so a cached limit can only be too small.
pub fn charge(n: u64) -> bool {
    if n == 0 || GLOBAL.charge(n, limit()) {
        return true;
    }
    if TEST_LIMIT.load(Ordering::Relaxed) == 0 && GLOBAL.charge(n, measure()) {
        return true;
    }
    REFUSED.inc();
    LAST_REFUSED.store(n, Ordering::Relaxed);
    false
}

/// A space's own limit refused `n` pages (counted with the system's refusals).
pub fn refused_by_space(n: u64) {
    REFUSED.inc();
    LAST_REFUSED.store(n, Ordering::Relaxed);
}

/// Pages a change took beyond what was charged for it (a bookkeeping bug): added to the total anyway, so the
/// later uncharge does not underflow, and counted.
pub fn force(n: u64) {
    GLOBAL.force(n);
    UNPLANNED.inc();
}
klog::counter!(UNPLANNED, "mm.commit.unplanned");

pub fn uncharge(n: u64) {
    if n > 0 && !GLOBAL.uncharge(n) {
        UNDERFLOWS.inc();
    }
}

/// Tests (QemuOnly): a small system limit in pages, None = measured again.
pub fn set_test_limit(pages: Option<u64>) {
    TEST_LIMIT.store(pages.unwrap_or(0), Ordering::Relaxed);
}

/// tmpfs and `/dev/shm` files are charged here once installed (FS-5 change request 4). vfs's own system-pool
/// limit (`vfs::pool::set_system_limit`, FS-1's tmpfs page limit; tests set it) keeps working as an extra ceiling
/// for tmpfs alone: from the moment it changes, tmpfs may grow by what that limit leaves above what vfs reported
/// in use then (`system_used`), as the built-in pool would allow. The commit itself binds first in practice.
struct TmpfsCommit {
    /// Tmpfs pages charged through this hook now.
    pages: AtomicU64,
    /// (vfs limit last seen, `pages` then, vfs's `system_used()` then).
    seen: ksync::SpinLock<(u64, u64, u64)>,
}

impl vfs::pool::Commit for TmpfsCommit {
    fn charge(&self, n: u64) -> bool {
        let limit = vfs::pool::system_limit();
        let mut s = self.seen.lock();
        let now = self.pages.load(Ordering::Relaxed);
        if s.0 != limit {
            *s = (limit, now, vfs::pool::system_used());
        }
        let grown = now.saturating_sub(s.1);
        if s.2.saturating_add(grown).saturating_add(n) > limit {
            REFUSED.inc();
            LAST_REFUSED.store(n, Ordering::Relaxed);
            return false;
        }
        if !charge(n) {
            return false;
        }
        self.pages.fetch_add(n, Ordering::Relaxed);
        true
    }
    fn uncharge(&self, n: u64) {
        let _ = self.pages.try_update(Ordering::AcqRel, Ordering::Relaxed, |p| Some(p.saturating_sub(n)));
        uncharge(n)
    }
}

static TMPFS: TmpfsCommit = TmpfsCommit { pages: AtomicU64::new(0), seen: ksync::SpinLock::named("mm.commit.tmpfs", (0, 0, 0)) };

/// Pages tmpfs (incl. shared anonymous memory) holds of the commit.
pub fn tmpfs_pages() -> u64 {
    TMPFS.pages.load(Ordering::Relaxed)
}

/// Core initcall (before `vfs::init` copies the initramfs into tmpfs): tmpfs charges become part of the system's
/// commit. Whatever the built-in pool already holds is taken over first.
fn install() -> Result<(), &'static str> {
    let before = vfs::pool::system_used();
    GLOBAL.force(before);
    TMPFS.pages.store(before, Ordering::Relaxed);
    vfs::pool::set_commit(&TMPFS);
    let l = limit();
    klog::event!(Info, "mm", "commit.ready", limit_pages = l, limit_mb = l.saturating_mul(PAGE) >> 20, keep_mb = POOL_KEEP >> 20,
        committed_pages = committed(); "strict commit (overcommit mode 2): private writable memory and tmpfs are charged when promised");
    Ok(())
}
kreg::initcall!(kreg::Level::Core, "mm.commit", install);

/// From the `mm.report` thread: one event per period in which a charge was refused.
pub(crate) fn report() {
    let n = REFUSED.get();
    let seen = REPORTED.swap(n, Ordering::Relaxed);
    if n > seen {
        klog::event!(Warn, "mm", "commit.refused", refusals = n - seen, last_pages = LAST_REFUSED.load(Ordering::Relaxed),
            committed_pages = committed(), limit_pages = limit(); "memory requests answered ENOMEM: the commit limit is reached (R2, nothing was killed)");
    }
    if UNDERFLOWS.get() > 0 && !REPORTED_UNDERFLOW.swap(true, Ordering::Relaxed) {
        klog::event!(Error, "mm", "commit.underflow", count = UNDERFLOWS.get(); "more commit given back than charged (bug)");
    }
}
static REPORTED_UNDERFLOW: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_all_or_nothing() {
        let b = Budget::new();
        assert!(b.charge(10, 15));
        assert!(!b.charge(6, 15), "over the limit: nothing taken");
        assert_eq!(b.committed(), 10);
        assert!(b.charge(5, 15));
        assert!(b.uncharge(15));
        assert!(!b.uncharge(1), "underflow is reported");
        assert_eq!(b.committed(), 0);
        assert!(!b.charge(u64::MAX, u64::MAX) || b.uncharge(u64::MAX));
    }

    #[test]
    fn limit_keeps_pool_keep() {
        assert_eq!(limit_for(32 << 30), ((32u64 << 30) - (512 << 20)) / PAGE);
        assert_eq!(limit_for(1 << 30), (3u64 << 28) / PAGE, "a small machine keeps a quarter");
    }
}
