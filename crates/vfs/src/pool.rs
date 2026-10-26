//! tmpfs memory pools (FS-5, V2-PLAN R2). FS-14: a tmpfs file or memfd is charged page by page when a page
//! is allocated (write, fault, fallocate), as Linux charges its VM_NORESERVE shmem files (`shmem_inode_acct_block`
//! from `shmem_alloc_and_add_folio`, linux-6.6/mm/shmem.c:210): a growing ftruncate costs nothing, holes are
//! free, and a page given back (truncate, hole punch, the file's end) returns its charge. A refusal is ENOSPC
//! to a write or fallocate and SIGBUS to a fault, as on Linux. Only the kernel's nameless objects (MM's shared
//! anonymous memory: Linux `shmem_kernel_file_setup` without VM_NORESERVE under overcommit mode 2) are still
//! charged by size when it grows, so a fault inside them never meets an empty pool.
//!
//! Two levels, in Linux's order (linux-6.6/mm/shmem.c `shmem_inode_acct_block`: the memory account first,
//! then the mount's `max_blocks`; `Pool::charge` names which said no, ENOMEM or ENOSPC, and the page cache
//! answers ENOSPC for a page as shmem does; a size-charged object's growth keeps the two answers):
//! - the **system pool**: what all tmpfs files together may hold, the frame allocator's memory minus
//!   `POOL_KEEP` (512 MiB, R2) measured at first use; MM-5b's strict commit replaces it through
//!   `set_commit` (then tmpfs charges are part of the system-wide commit). Refused: **ENOMEM**.
//! - a **mount's own size** (`size=` of Linux's tmpfs), none by default: `/tmp` and `/dev/shm` are bounded only
//!   by the pool (V2-PLAN "Shared memory"). Refused: **ENOSPC**.
//!
//! - **metadata** (review INT-A1d fs 1): every inode (`INODE_BYTES`, Linux's `BOGO_INODE_SIZE`, plus a symlink's
//!   target) and every name (`DENTRY_BYTES` plus the name) is charged to the system pool too, in whole pages as a
//!   mount's metadata bytes grow (`charge_meta`, held by a `MetaCharge` until the inode or name goes). Not to the
//!   mount's own size: Linux's `size=` bounds blocks only. Refused: **ENOSPC** (Linux `shmem_reserve_inode`,
//!   linux-6.6/mm/shmem.c:330). Without it an empty file or a symlink cost kernel heap no pool saw, and a program
//!   filling the 1777 `/tmp` with them exhausted the heap (a panic) instead of getting ENOSPC.
//!
//! All counters are atomics (no lock), so charging may happen under any file lock.

use crate::Errno;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use uapi::errno::*;

pub const PAGE: u64 = 4096;
/// What the system keeps for itself (R2's POOL_KEEP).
pub const POOL_KEEP_BYTES: u64 = 512 << 20;

/// The system-wide commit MM-5b installs: `charge(pages)` true = granted (all or nothing).
pub trait Commit: Sync {
    fn charge(&self, pages: u64) -> bool;
    fn uncharge(&self, pages: u64);
}

static COMMIT: ksync::OnceRef<dyn Commit> = ksync::OnceRef::new();

/// MM-5b: from now on tmpfs charges go to the system-wide commit (once; the built-in pool is retired).
pub fn set_commit(c: &'static dyn Commit) {
    COMMIT.set(c);
    klog::event!(Info, "vfs", "pool.commit_hook"; "tmpfs charges now go to the system-wide commit");
}

/// The built-in system pool (until MM-5b's commit): pages charged by all tmpfs files' data and metadata (the
/// one counter the limit applies to)...
static SYS_PAGES: AtomicU64 = AtomicU64::new(0);
/// ... of them for data ...
static CHARGED: AtomicU64 = AtomicU64::new(0);
/// ... and for metadata.
static META_PAGES: AtomicU64 = AtomicU64::new(0);

/// What one inode costs besides its data (Linux `BOGO_INODE_SIZE`; V2's inode, page cache head and locks are
/// about this size).
pub const INODE_BYTES: u64 = 1024;
/// What one name costs besides its bytes (the dentry and its two entries in the parent's maps).
pub const DENTRY_BYTES: u64 = 512;
/// Its limit in pages; 0 = not measured yet.
static LIMIT: AtomicU64 = AtomicU64::new(0);
static POOL_FULL: AtomicBool = AtomicBool::new(false);

fn measured_limit() -> u64 {
    let l = LIMIT.load(Ordering::Relaxed);
    if l != 0 {
        return l;
    }
    match mm_core::frames().map(|f| f.stats().total_bytes) {
        Some(total) if total > 0 => {
            let keep = POOL_KEEP_BYTES.min(total / 4);
            let n = limit_for(total);
            if LIMIT.compare_exchange(0, n, Ordering::AcqRel, Ordering::Relaxed).is_ok() {
                MEASURED.store(n, Ordering::Relaxed);
                klog::event!(Info, "vfs", "tmpfs.budget", pages = n, bytes = n * PAGE, keep_bytes = keep;
                    "tmpfs pages are charged against this pool when allocated (FS-14); refused beyond it");
            }
            LIMIT.load(Ordering::Relaxed)
        }
        // No frame allocator (host tests, early boot): only the heap's own refusal bounds it.
        _ => u64::MAX,
    }
}

/// The frame allocator gained memory (the kernel calls it after releasing the initramfs module, which vfs::init
/// unpacked, so the first measurement came before it): the next charge measures the pool again, so it stays
/// RAM minus POOL_KEEP (R2; D4 of INT-A1d: the Mini-PC's pool was 6.5 MiB short). A limit a test set stays.
pub fn frames_grew() {
    let _ = LIMIT.compare_exchange(MEASURED.load(Ordering::Relaxed), 0, Ordering::AcqRel, Ordering::Relaxed);
}

/// The last measured limit (0: none), so `frames_grew` leaves a test's own limit alone.
static MEASURED: AtomicU64 = AtomicU64::new(0);

/// The pool's limit for `total` bytes of frames: all of it minus POOL_KEEP; small machines (tests with 128
/// MiB) keep a quarter instead of 512 MiB.
pub fn limit_for(total: u64) -> u64 {
    let keep = POOL_KEEP_BYTES.min(total / 4);
    ((total - keep) / PAGE).max(1)
}

/// Tests and FS-1's hook: the system pool's limit in pages (None: measure again).
pub fn set_system_limit(pages: Option<u64>) {
    LIMIT.store(pages.unwrap_or(0), Ordering::Relaxed);
    POOL_FULL.store(false, Ordering::Relaxed);
}

/// The system pool's limit in pages (u64::MAX without a frame allocator).
pub fn system_limit() -> u64 {
    measured_limit()
}

/// Pages charged to the system pool by all tmpfs files' data.
pub fn system_charged() -> u64 {
    CHARGED.load(Ordering::Relaxed)
}

/// Pages charged to the system pool for tmpfs metadata (inodes, names).
pub fn system_meta() -> u64 {
    META_PAGES.load(Ordering::Relaxed)
}

/// Pages of the system pool in use, data and metadata (what its limit applies to). Once MM's commit took the
/// charges over (`set_commit`) the built-in counter stands still, so it is what tmpfs holds now (data plus
/// metadata, the same sum the built-in pool counted): FS-15, statfs's f_bfree moved with no write before
/// (nplb PosixStatvfsTest.UsageChanges).
pub fn system_used() -> u64 {
    if COMMIT.get().is_some() {
        return CHARGED.load(Ordering::Relaxed).saturating_add(META_PAGES.load(Ordering::Relaxed));
    }
    SYS_PAGES.load(Ordering::Relaxed)
}

/// Takes `n` from `counter`, never below zero; an attempt to go below is a bookkeeping bug, counted and logged
/// once instead of wrapping (a wrapped counter would refuse every later charge).
fn give(counter: &AtomicU64, n: u64) -> u64 {
    let mut used = counter.load(Ordering::Relaxed);
    loop {
        let after = used.saturating_sub(n);
        match counter.compare_exchange_weak(used, after, Ordering::AcqRel, Ordering::Relaxed) {
            Ok(_) => {
                if used < n {
                    UNDERFLOWS.inc();
                    if !UNDERFLOW_LOGGED.swap(true, Ordering::Relaxed) {
                        klog::event!(Error, "vfs", "pool.underflow", had = used, gave = n; "a tmpfs pool was given back more than it held (bug)");
                    }
                }
                return used;
            }
            Err(now) => used = now,
        }
    }
}

klog::counter!(UNDERFLOWS, "vfs.pool.underflows");
static UNDERFLOW_LOGGED: AtomicBool = AtomicBool::new(false);

/// Adds `n` to `counter` if the result stays at or below `limit`.
fn take(counter: &AtomicU64, n: u64, limit: u64) -> bool {
    let mut used = counter.load(Ordering::Relaxed);
    loop {
        let Some(after) = used.checked_add(n).filter(|&a| a <= limit) else { return false };
        match counter.compare_exchange_weak(used, after, Ordering::AcqRel, Ordering::Relaxed) {
            Ok(_) => return true,
            Err(now) => used = now,
        }
    }
}

fn system_charge(n: u64) -> bool {
    if !system_take(n) {
        return false;
    }
    CHARGED.fetch_add(n, Ordering::Relaxed);
    true
}

fn system_uncharge(n: u64) {
    give(&CHARGED, n);
    system_give(n);
}

/// `n` pages of the system pool (data or metadata), all or nothing.
fn system_take(n: u64) -> bool {
    if let Some(c) = COMMIT.get() {
        return c.charge(n);
    }
    let limit = measured_limit();
    if take(&SYS_PAGES, n, limit) {
        return true;
    }
    if !POOL_FULL.swap(true, Ordering::AcqRel) {
        klog::event!(Warn, "vfs", "tmpfs.full", pages = limit, bytes = limit.saturating_mul(PAGE), asked = n;
            "the tmpfs pool is used up: new tmpfs pages are refused until pages are given back");
    }
    false
}

fn system_give(n: u64) {
    if let Some(c) = COMMIT.get() {
        return c.uncharge(n);
    }
    let before = give(&SYS_PAGES, n);
    if POOL_FULL.load(Ordering::Relaxed) && before.saturating_sub(n) < measured_limit() {
        POOL_FULL.store(false, Ordering::Release);
    }
}

/// One file system's pool (a tmpfs instance): its own size bound (0 = none) and what its files hold.
pub struct Pool {
    pub name: &'static str,
    limit: AtomicU64,
    used: AtomicU64,
    full: AtomicBool,
    /// Metadata bytes of its inodes and names; `pages_for(meta)` pages of the system pool are charged for them.
    meta: AtomicU64,
    /// FS-15: inode slots in use (Linux `max_inodes - free_ispace / BOGO_INODE_SIZE`): one per live inode, one
    /// more per extra hard link of a non-directory (`shmem_link`/`shmem_unlink`). Counted for statfs only.
    inodes: AtomicU64,
}

klog::counter!(REFUSED_NOSPC, "vfs.pool.enospc");
klog::counter!(REFUSED_NOMEM, "vfs.pool.enomem");
klog::counter!(REFUSED_META, "vfs.pool.meta_refused");

/// Metadata charged to a pool (an inode, a name): given back when it is dropped.
pub struct MetaCharge {
    pool: Arc<Pool>,
    bytes: u64,
    /// Inode slots it holds (an inode's charge: 1 plus its extra hard links; a name's: 0), given back with it.
    slots: AtomicU64,
}

impl MetaCharge {
    pub fn bytes(&self) -> u64 {
        self.bytes
    }
    /// One more inode slot (a hard link to a named non-directory, `shmem_reserve_inode(sb, NULL)`).
    pub fn add_slot(&self) {
        self.slots.fetch_add(1, Ordering::Relaxed);
        self.pool.inodes.fetch_add(1, Ordering::Relaxed);
    }
    /// One slot back (a name of a non-directory with more than one link went, `shmem_free_inode`); never below
    /// the inode's own slot. `Inode::add_link`/`drop_link` order their steps so a refusal here is a real
    /// accounting error (counted as an underflow), not a race.
    pub fn drop_slot(&self) {
        if self.slots.try_update(Ordering::Relaxed, Ordering::Relaxed, |n| (n > 1).then(|| n - 1)).is_ok() {
            take_sat(&self.pool.inodes, 1);
        } else {
            UNDERFLOWS.inc();
        }
    }
}

impl Drop for MetaCharge {
    fn drop(&mut self) {
        take_sat(&self.pool.inodes, self.slots.load(Ordering::Relaxed));
        self.pool.uncharge_meta(self.bytes);
    }
}

/// Subtracts `n` from `a`, saturating at 0.
fn take_sat(a: &AtomicU64, n: u64) {
    if n > 0 {
        let _ = a.try_update(Ordering::Relaxed, Ordering::Relaxed, |v| Some(v.saturating_sub(n)));
    }
}

/// RAM in pages (Linux `totalram_pages`): what the frame allocator manages; None without one (host tests).
pub fn ram_pages() -> Option<u64> {
    mm_core::frames().map(|f| f.stats().total_bytes / PAGE).filter(|&p| p > 0)
}

impl Pool {
    pub const fn new(name: &'static str) -> Pool {
        Pool { name, limit: AtomicU64::new(0), used: AtomicU64::new(0), full: AtomicBool::new(false), meta: AtomicU64::new(0), inodes: AtomicU64::new(0) }
    }

    /// Metadata bytes its inodes and names hold now.
    pub fn meta_bytes(&self) -> u64 {
        self.meta.load(Ordering::Relaxed)
    }

    /// FS-15: inode slots in use (its charged inodes plus their extra hard links).
    pub fn inodes(&self) -> u64 {
        self.inodes.load(Ordering::Relaxed)
    }

    /// Charges a new inode (`bytes` of metadata) and takes its inode slot.
    pub fn charge_inode(self: &Arc<Self>, bytes: u64) -> Result<MetaCharge, Errno> {
        let c = self.charge_meta(bytes)?;
        c.add_slot();
        Ok(c)
    }

    /// Charges `bytes` of metadata (a new inode or name): the system pool gives the pages the mount's metadata
    /// grows into; ENOSPC when it refuses. Each step charges exactly the pages its own change of the byte count
    /// needs (taken before the exchange, given back if the exchange lost), so the pages charged always equal
    /// `pages_for(meta)`.
    pub fn charge_meta(self: &Arc<Self>, bytes: u64) -> Result<MetaCharge, Errno> {
        let mut old = self.meta.load(Ordering::Relaxed);
        loop {
            let new = old.checked_add(bytes).ok_or(ENOSPC)?;
            let need = pages_for(new) - pages_for(old);
            if need > 0 && !system_take(need) {
                REFUSED_META.inc();
                return Err(ENOSPC);
            }
            match self.meta.compare_exchange_weak(old, new, Ordering::AcqRel, Ordering::Relaxed) {
                Ok(_) => {
                    META_PAGES.fetch_add(need, Ordering::Relaxed);
                    return Ok(MetaCharge { pool: self.clone(), bytes, slots: AtomicU64::new(0) });
                }
                Err(now) => {
                    if need > 0 {
                        system_give(need);
                    }
                    old = now;
                }
            }
        }
    }

    fn uncharge_meta(&self, bytes: u64) {
        let mut old = self.meta.load(Ordering::Relaxed);
        loop {
            let new = old.saturating_sub(bytes);
            match self.meta.compare_exchange_weak(old, new, Ordering::AcqRel, Ordering::Relaxed) {
                Ok(_) => {
                    if old < bytes {
                        UNDERFLOWS.inc();
                    }
                    let back = pages_for(old) - pages_for(new);
                    if back > 0 {
                        give(&META_PAGES, back);
                        system_give(back);
                    }
                    return;
                }
                Err(now) => old = now,
            }
        }
    }
    /// The mount's own size in pages (`size=`; None = only the system pool bounds it).
    pub fn set_size(&self, pages: Option<u64>) {
        self.limit.store(pages.unwrap_or(0), Ordering::Relaxed);
        self.full.store(false, Ordering::Relaxed);
    }
    pub fn size(&self) -> Option<u64> {
        match self.limit.load(Ordering::Relaxed) {
            0 => None,
            n => Some(n),
        }
    }
    /// Pages its files are charged for now.
    pub fn used(&self) -> u64 {
        self.used.load(Ordering::Relaxed)
    }

    /// Charges `n` pages, all or nothing: ENOMEM when the system pool refuses, ENOSPC when this mount is full.
    pub fn charge(&self, n: u64) -> Result<(), Errno> {
        if n == 0 {
            return Ok(());
        }
        if !system_charge(n) {
            REFUSED_NOMEM.inc();
            return Err(ENOMEM);
        }
        let limit = self.size().unwrap_or(u64::MAX);
        if !take(&self.used, n, limit) {
            system_uncharge(n);
            REFUSED_NOSPC.inc();
            if !self.full.swap(true, Ordering::AcqRel) {
                klog::event!(Warn, "vfs", "tmpfs.mount_full", fs = self.name, pages = limit, asked = n;
                    "a tmpfs mount reached its size: growing a file there answers ENOSPC");
            }
            return Err(ENOSPC);
        }
        Ok(())
    }

    /// Charges as many of `n` pages as both levels allow, but at least `least` (all or nothing; nothing at all
    /// when even those do not fit): for writes, which write what fits (Linux writes page by page and returns
    /// the bytes so far) yet need every page up to the first byte's. After a refusal of the whole the rest is
    /// taken in halving chunks (O(log n) tries, never a page-by-page walk that would drain the pool for others
    /// meanwhile). Returns the pages granted and, when that is less than `n`, the error of the last refusal.
    pub fn charge_upto(&self, least: u64, n: u64) -> (u64, Option<Errno>) {
        let least = least.min(n);
        let mut refused = match self.charge(n) {
            Ok(()) => return (n, None),
            Err(e) => e,
        };
        if least == n {
            return (0, Some(refused));
        }
        if let Err(e) = self.charge(least) {
            return (0, Some(e));
        }
        let mut got = least;
        let mut chunk = (n - got) / 2;
        while chunk > 0 {
            match self.charge(chunk) {
                Ok(()) => {
                    got += chunk;
                    chunk = chunk.min(n - got);
                }
                Err(e) => {
                    refused = e;
                    chunk /= 2;
                }
            }
        }
        (got, (got < n).then_some(refused))
    }

    pub fn uncharge(&self, n: u64) {
        if n == 0 {
            return;
        }
        let before = give(&self.used, n);
        if self.full.load(Ordering::Relaxed) && before.saturating_sub(n) < self.size().unwrap_or(u64::MAX) {
            self.full.store(false, Ordering::Release);
        }
        system_uncharge(n);
    }
}

/// Pages a file of `size` bytes is charged for.
pub fn pages_for(size: u64) -> u64 {
    size.div_ceil(PAGE)
}
