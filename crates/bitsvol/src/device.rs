//! A volume of a pool as a stacked `BlockDevice` (B1 stack: partition -> crypt -> bitsvol -> ext4). One
//! sleeping lock per pool serialises its metadata; it is held only across bounded device calls, and a waiter
//! gives up after `LOCK_WAIT_NS` with `Timeout` (B4: no unbounded wait; block reports it as EIO). The volume's own
//! I/O waits `IO_WAIT_ROUNDS` such rounds: to a file system a timeout is a failed write and read-only for good, so
//! only a pool that really stands still may answer one (a slow one never: Linux has no such bound at all).
//! Long background work (a delete, a size walk) takes the lock in steps and behind everyone else (`lock_behind`):
//! the mutex lets a thread that just released it take it again before the sleeper it woke has run, so a loop of
//! holds would keep a waiter out for the whole loop (VS-H: one write waited for 48 of 48 deletes).
//! BL-2: a read holds the lock only to map its chunks (`read_mapped`); the data is read without it.
//! Closing a device never waits for the pool lock: the release goes to a short spin-locked queue whose room was
//! reserved at open, and the next holder of the pool lock applies it.
//! A snapshot window (`PoolHandle::window`, VS-2b) lets the flushes of a file system's freeze return at once:
//! the window's copy-on-every-write keeps every power-cut state a prefix of the writes, and the commit after
//! the window makes them durable together.

use crate::error::VolError;
use crate::pool::{Barriers, Pool};
use crate::timing::{self, CommitTime};
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use devmodel::DevError;
use crate::volume::{Piece, read_pieces, read_pieces_frames};
use devmodel::block::{BlockDevice, BlockInfo, FrameIo, IoBuf, IoPriority, Snapshots};
use ksync::{Mutex, MutexGuard, SpinLock, WaitQueue};

pub const LOCK_WAIT_NS: u64 = 30_000_000_000;
/// Rounds of `LOCK_WAIT_NS` a volume's reads, writes and flushes wait (5 minutes, each round logged).
const IO_WAIT_ROUNDS: u32 = 10;
/// The same in full, for the waits below the pool lock (`Barriers::flush`).
pub(crate) const IO_WAIT_NS: u64 = LOCK_WAIT_NS * IO_WAIT_ROUNDS as u64;
/// How long `lock_behind` lets those ahead go first before it queues with them, so a steady stream of foreground
/// I/O slows a delete but does not stop it.
const BEHIND_WAIT_NS: u64 = 1_000_000_000;
/// Tree pages one step of a delete opens with the pool locked, each at most one device read.
const DELETE_STEP_PAGES: u64 = 64;
/// BL-2: reads mapped under the pool lock and read without it before one reads under it (the chunks it mapped were
/// released and given out again during each of them: a commit beside a stream of reads).
pub(crate) const READ_TRIES: u32 = 3;

klog::counter!(READ_RETRIES, "bitsvol.read_retries");
klog::counter!(READ_LOCKED, "bitsvol.read_locked");

/// The snapshot a scheduled rollback keeps the state before it as (VS-3).
pub const BEFORE_ROLLBACK: &str = "before-rollback";

/// Wall-clock UTC ns for the "before-rollback" snapshot of a rollback carried out at an open (the kernel sets
/// it; 0 without one, as a snapshot taken without a wall clock).
static CLOCK: SpinLock<Option<fn() -> u64>> = SpinLock::named("bitsvol.clock", None);

pub fn set_clock(f: fn() -> u64) {
    *CLOCK.lock() = Some(f);
}

fn clock_ns() -> u64 {
    let f = *CLOCK.lock();
    f.map_or(0, |f| f())
}

/// Closed devices whose busy count the pool has not dropped yet. `ids` always has room for `live` entries,
/// so a close never allocates.
struct Releases {
    ids: Vec<u64>,
    live: usize,
}

/// Answers whether a flush in a snapshot window may wait for the window's commit (`PoolHandle::window`).
pub type DeferFlush = Arc<dyn Fn() -> bool + Send + Sync>;

pub struct PoolHandle {
    pool: Mutex<Pool>,
    releases: SpinLock<Releases>,
    name: String,
    wait_ns: u64,
    /// The window's volume and its answer (`window`).
    defer: SpinLock<Option<(u64, DeferFlush)>>,
    /// A window ended while the pool lock was taken elsewhere: the next holder closes the copy window.
    window_end: AtomicBool,
    /// Flushes a window let return without a commit (all time).
    deferred: AtomicU64,
    /// How many of `deferred` a successful commit has made durable since (`commit`, `settle`).
    durable: AtomicU64,
    /// Woken when a window ends (`settle` waits for the window's commit).
    window_closed: WaitQueue,
    /// The snapshot of a scheduled rollback an open could not carry out (0: none): it stays scheduled, and the
    /// snapshots tools say so instead of only the log (a system restore's generation changed without it).
    rollback_failed: AtomicU64,
    /// The pool's device, for the flush before a commit (`timed`).
    dev: Arc<dyn BlockDevice>,
    /// The pool's volume bytes not yet flushed (`Pool::owed_counter`): the pool's own count, so a commit the
    /// pool started itself (`commit_dirty_pages`, a FUA write) or one under a bare `lock` clears it too.
    owed: Arc<AtomicU64>,
    /// The pool's barrier state (`flush_volume`).
    io: Arc<Barriers>,
    /// BL-2: the pool's reuse count (`Pool::reuse_counter`), read by the lock-free reads.
    reuse: Arc<AtomicU64>,
    /// The longest commit an event reported and the last commit (VS-7, `timing::record`).
    longest: AtomicU64,
    last: SpinLock<CommitTime>,
    /// Foreground callers on their way to the pool lock; `lock_behind` lets them go first.
    ahead: AtomicU64,
    no_one_ahead: WaitQueue,
    /// A delete's rest given up because the lock did not come (`delete`): the next holder counts it in `leaks`.
    lost_deletes: AtomicU64,
    behind: AtomicU64,
    gave_way: AtomicU64,
}

impl PoolHandle {
    pub fn new(pool: Pool, name: &str) -> Arc<PoolHandle> {
        Self::with_wait(pool, name, LOCK_WAIT_NS)
    }

    /// As `new`, with another bound for lock waits (tests).
    pub fn with_wait(pool: Pool, name: &str, wait_ns: u64) -> Arc<PoolHandle> {
        let (dev, owed, io, reuse) = (pool.device().clone(), pool.owed_counter(), pool.barrier_state(), pool.reuse_counter());
        Arc::new(PoolHandle {
            dev,
            owed,
            io,
            reuse,
            pool: Mutex::named("bitsvol.pool", pool),
            releases: SpinLock::named("bitsvol.releases", Releases { ids: Vec::new(), live: 0 }),
            name: name.into(),
            wait_ns,
            defer: SpinLock::named("bitsvol.defer", None),
            window_end: AtomicBool::new(false),
            deferred: AtomicU64::new(0),
            durable: AtomicU64::new(0),
            window_closed: WaitQueue::new(),
            rollback_failed: AtomicU64::new(0),
            longest: AtomicU64::new(0),
            last: SpinLock::named("bitsvol.last_commit", CommitTime::default()),
            ahead: AtomicU64::new(0),
            no_one_ahead: WaitQueue::new(),
            lost_deletes: AtomicU64::new(0),
            behind: AtomicU64::new(0),
            gave_way: AtomicU64::new(0),
        })
    }

    /// The snapshot a scheduled rollback went to that an open of this pool could not carry out (VS-3).
    pub fn rollback_failed(&self) -> Option<u64> {
        Some(self.rollback_failed.load(Ordering::Relaxed)).filter(|&s| s != 0)
    }

    /// A snapshot window on volume `vol` (VS-2b; one at a time, `snapshots::take` serialises them): from now on
    /// no write to `vol` lands in a chunk a commit holds (`Pool::begin_copy_window`), and a flush of `vol` for
    /// which `defer` answers true returns at once (counted in `deferred_flushes`); the caller
    /// commits before or after dropping it; until that commit those flushes are not durable (a power cut leaves
    /// `vol` as of the last commit). `defer` bounds that: `snapshots::take` answers true only while its own
    /// freeze runs and commits right after the thaw.
    pub fn window(self: &Arc<Self>, vol: u64, defer: DeferFlush) -> Result<Window, DevError> {
        self.lock()?.begin_copy_window(vol).map_err(VolError::to_dev)?;
        *self.defer.lock() = Some((vol, defer));
        Ok(Window(self.clone()))
    }

    /// Flushes a window let return without a commit (all time; `owed_flushes` says how many are not durable).
    pub fn deferred_flushes(&self) -> u64 {
        self.deferred.load(Ordering::Acquire)
    }

    /// Deferred flushes no successful commit has covered yet (0 after a window's commit went through).
    pub fn owed_flushes(&self) -> u64 {
        self.deferred.load(Ordering::Acquire).saturating_sub(self.durable.load(Ordering::Acquire))
    }

    /// Whether a snapshot window is open (its deferral installed).
    pub fn window_open(&self) -> bool {
        self.defer.lock().is_some()
    }

    /// A pool commit that also marks every flush deferred before it as durable (`settle`). A take's commit
    /// after the thaw goes through here.
    pub fn commit(&self) -> Result<(), DevError> {
        self.timed("commit", |p| p.commit())
    }

    /// The last commit of this pool through its handle (flush, FUA write, `commit`) as it was measured.
    pub fn last_commit(&self) -> CommitTime {
        *self.last.lock()
    }

    /// Runs a commit under the pool lock and measures it (`timing::record`: counters, events). Six clock reads
    /// and two counter snapshots per commit, which already costs one or two device flushes.
    ///
    /// With `PREFLUSH_BYTES` or more of volume writes not yet flushed, the device is flushed first without the pool
    /// lock: on an image file that flush is the fsync that writes the volume's data back (2 s for 4 MiB on a
    /// 2 MiB/s stick), and every read of the pool would wait for it under the lock. The commit's own barrier
    /// still runs and then only finds the metadata and what was written meanwhile. dm-thin flushes its data
    /// device before each metadata commit too (linux-6.6/drivers/md/dm-thin.c:3216, called from
    /// dm-thin-metadata.c:899), under its metadata lock; here the pool lock also holds up the reads' mapping step (BL-2:
    /// their data no longer waits for it).
    fn timed(&self, why: &'static str, f: impl FnOnce(&mut Pool) -> crate::error::Result<()>) -> Result<(), DevError> {
        self.timed_by(why, Self::lock_io, 0, f)
    }

    /// `waited`: ns this commit already waited for the pool lock.
    fn timed_by(&self, why: &'static str, take: for<'a> fn(&'a Self) -> Result<MutexGuard<'a, Pool>, DevError>, waited: u64, f: impl FnOnce(&mut Pool) -> crate::error::Result<()>) -> Result<(), DevError> {
        let d = |a: Option<u64>, b: Option<u64>| a.zip(b).map_or(0, |(a, b)| b.saturating_sub(a));
        let pre_bytes = self.owed.load(Ordering::Acquire);
        let (mut pre_ns, mut pre) = (0, 0);
        if pre_bytes >= timing::PREFLUSH_BYTES {
            let a = timing::now();
            match self.dev.flush() {
                Ok(()) => pre = pre_bytes,
                Err(e) => klog::event!(Warn, "bitsvol", "commit.preflush_failed", pool = self.name.clone(), error = alloc::format!("{e:?}"); "the commit's own flush follows"),
            }
            pre_ns = d(a, timing::now());
        }
        // A flush under way (`flush_volume`) would make this commit's barrier wait for it with the pool lock held.
        self.io.wait_flush();
        let asked = timing::now();
        let mut p = take(self)?;
        let t0 = timing::now();
        p.flushed_outside(pre);
        let before = p.io_stats();
        let r = self.commit_locked(&mut p, f);
        let io = p.io_stats().since(&before);
        let t1 = timing::now();
        drop(p);
        let t = CommitTime { why, lock_wait_ns: waited.saturating_add(d(asked, t0)), ns: d(t0, t1), pre_ns, pre_bytes: pre, io };
        *self.last.lock() = t;
        timing::record(&self.name, &self.longest, &t);
        r
    }

    /// A volume's flush. When the pool has nothing to commit it is only the device's cache flush, and that runs
    /// without the pool lock: an fsync of data rewritten in place otherwise stops every read and write of the pool
    /// for the whole flush (VS-H: seven of ten slow commits under fsync load, and a snapshot's write-back behind them
    /// waited one flush per request). What this flush must cover was written before it was asked for, and with
    /// no metadata owed those writes need the device's cache flushed and nothing else; `Barriers::flush` keeps
    /// an fsync from returning on a flush still under way.
    fn flush_volume(&self) -> Result<(), DevError> {
        let d = |a: Option<u64>, b: Option<u64>| a.zip(b).map_or(0, |(a, b)| b.saturating_sub(a));
        let asked = timing::now();
        let p = self.lock_io()?;
        let t0 = timing::now();
        if p.meta_dirty() {
            drop(p);
            return self.timed_by("flush", Self::lock_io, d(asked, t0), |p| p.commit());
        }
        let deferred = self.deferred.load(Ordering::Acquire);
        drop(p);
        let before = self.io.stats();
        let r = self.io.flush(&self.dev).map_err(VolError::to_dev);
        if r.is_ok() {
            self.durable.fetch_max(deferred, Ordering::AcqRel);
        }
        let t = CommitTime { why: "flush", lock_wait_ns: d(asked, t0), ns: d(t0, timing::now()), pre_ns: 0, pre_bytes: 0, io: self.io.stats().since(&before) };
        *self.last.lock() = t;
        timing::record(&self.name, &self.longest, &t);
        r
    }

    /// Runs `f` (a commit) with the pool locked; on success the flushes deferred before the lock was taken are
    /// durable (their writes were in the pool: a write holds the pool lock, its flush comes after it).
    fn commit_locked(&self, p: &mut Pool, f: impl FnOnce(&mut Pool) -> crate::error::Result<()>) -> Result<(), DevError> {
        let owed = self.deferred.load(Ordering::Acquire);
        f(p).map_err(VolError::to_dev)?;
        self.durable.fetch_max(owed, Ordering::AcqRel);
        Ok(())
    }

    /// fsync's answer after a flush (VS-2b, exact durability in a window): returns once every flush deferred
    /// before this call is durable. While a window is open it waits for the window's own commit (at most the
    /// pool's lock bound); when that commit failed or never came, it commits itself and returns that result
    /// (EIO and the like reach the fsync that was told "done" too early). Never called from the block
    /// dispatcher or with a file system lock held: the take's freeze may wait for those.
    pub fn settle(&self) -> Result<(), DevError> {
        let want = self.deferred.load(Ordering::Acquire);
        if self.durable.load(Ordering::Acquire) >= want {
            return Ok(());
        }
        let covered = || self.durable.load(Ordering::Acquire) >= want || !self.window_open();
        if !self.window_closed.wait_until_timeout(covered, self.wait_ns) {
            klog::event!(Warn, "bitsvol", "window.settle_timeout", pool = self.name.clone(), wait_ns = self.wait_ns; "commits without the window's");
        }
        if self.durable.load(Ordering::Acquire) >= want {
            return Ok(());
        }
        klog::event!(Info, "bitsvol", "window.settle_commit", pool = self.name.clone(), owed = want.saturating_sub(self.durable.load(Ordering::Acquire)));
        let r = self.commit();
        if let Err(e) = r {
            klog::event!(Warn, "bitsvol", "window.settle_failed", pool = self.name.clone(), err = alloc::format!("{e:?}"); "deferred flushes are not durable");
        }
        r
    }

    fn defer_now(&self, vol: u64) -> bool {
        let d = self.defer.lock().clone();
        let yes = d.is_some_and(|(v, f)| v == vol && f());
        if yes {
            self.deferred.fetch_add(1, Ordering::Relaxed);
        }
        yes
    }

    /// One bounded wait for the pool lock, ahead of `lock_behind`.
    fn lock_ahead(&self) -> Option<MutexGuard<'_, Pool>> {
        self.ahead.fetch_add(1, Ordering::SeqCst);
        let g = self.pool.lock_timeout(self.wait_ns);
        if self.ahead.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.no_one_ahead.wake_all();
        }
        g
    }

    fn timed_out(&self) -> DevError {
        klog::event!(Warn, "bitsvol", "pool.lock_timeout", pool = self.name.clone(), wait_ns = self.wait_ns);
        DevError::Timeout
    }

    pub fn lock(&self) -> Result<MutexGuard<'_, Pool>, DevError> {
        let mut g = self.lock_ahead().ok_or_else(|| self.timed_out())?;
        self.apply_releases(&mut g);
        Ok(g)
    }

    /// The lock for the volume's own I/O: up to `IO_WAIT_ROUNDS` rounds of the bound, see the module text.
    /// Host tests: someone holds the pool lock now.
    #[cfg(test)]
    pub(crate) fn pool_locked(&self) -> bool {
        self.pool.try_lock().is_none()
    }

    fn lock_io(&self) -> Result<MutexGuard<'_, Pool>, DevError> {
        self.lock_rounds(Self::lock_ahead)
    }

    fn lock_rounds<'a>(&'a self, take: fn(&'a Self) -> Option<MutexGuard<'a, Pool>>) -> Result<MutexGuard<'a, Pool>, DevError> {
        for round in 1..IO_WAIT_ROUNDS {
            if let Some(mut g) = take(self) {
                self.apply_releases(&mut g);
                return Ok(g);
            }
            klog::event!(Warn, "bitsvol", "pool.lock_slow", pool = self.name.clone(), round = round, of = IO_WAIT_ROUNDS, wait_ns = self.wait_ns,
                waiters = self.pool.waiters() as u64, owner = self.pool.owner().unwrap_or(0); "the volume's I/O waits on");
        }
        let mut g = take(self).ok_or_else(|| self.timed_out())?;
        self.apply_releases(&mut g);
        Ok(g)
    }

    /// The pool lock for background work that takes it again and again: everyone on the way to `lock` goes first.
    /// Bounded twice: `BEHIND_WAIT_NS` of giving way, then the wait of the volume's I/O.
    pub fn lock_behind(&self) -> Result<MutexGuard<'_, Pool>, DevError> {
        self.behind.fetch_add(1, Ordering::Relaxed);
        if self.ahead.load(Ordering::SeqCst) != 0 {
            self.gave_way.fetch_add(1, Ordering::Relaxed);
            self.no_one_ahead.wait_until_timeout(|| self.ahead.load(Ordering::SeqCst) == 0, BEHIND_WAIT_NS);
        }
        self.lock_rounds(|h| h.pool.lock_timeout(h.wait_ns))
    }

    #[cfg(test)]
    pub(crate) fn ahead(&self) -> u64 {
        self.ahead.load(Ordering::SeqCst)
    }

    /// (holds taken by `lock_behind`, how many of them gave way to someone first), all time.
    pub fn behind_counts(&self) -> (u64, u64) {
        (self.behind.load(Ordering::Relaxed), self.gave_way.load(Ordering::Relaxed))
    }

    /// `Pool::delete` in steps: at most `DELETE_STEP_PAGES` tree pages per hold of the pool lock, and the holds
    /// behind everyone else, so a foreground write waits for one step of a delete or for its commit, not for a
    /// burst of them (dm-thin frees in small steps too, `process_delete_bio` and dm_btree_del's bounded walk). The
    /// commit may be anyone's in between: see `Pool::delete_begin`.
    pub fn delete(&self, id: u64) -> Result<(), VolError> {
        let mut p = self.lock_behind()?;
        let quiet = p.cfg.quiet;
        let mut d = p.delete_begin(id)?;
        while !p.delete_step(&mut d, DELETE_STEP_PAGES) {
            drop(p);
            p = self.lock_behind().inspect_err(|_| {
                self.lost_deletes.fetch_add(1, Ordering::Relaxed);
                klog::event!(Error, "bitsvol", "delete.leak", id = id; "the pool lock did not come: the rest of the tree is lost until the next open");
            })?;
        }
        drop(p);
        self.timed_by("delete", Self::lock_behind, 0, |p| p.commit())?;
        note!(quiet, "volume.delete", id = id);
        Ok(())
    }

    fn apply_releases(&self, p: &mut Pool) {
        if self.window_end.swap(false, Ordering::AcqRel) {
            p.end_copy_window();
        }
        let lost = self.lost_deletes.swap(0, Ordering::Relaxed);
        p.leaks += lost;
        let mut r = self.releases.lock();
        r.live -= r.ids.len();
        for id in r.ids.drain(..) {
            if let Some(n) = p.busy.get_mut(&id) {
                *n -= 1;
                if *n == 0 {
                    p.busy.remove(&id);
                }
            }
        }
    }

    /// The volume or snapshot `id` as a block device; it cannot be deleted or rolled back while open.
    ///
    /// The first open of a writable volume carries out a rollback scheduled for it (`Pool::schedule_rollback`):
    /// no file system is on it yet, so none holds the old tree in its caches. A failed one is logged, stays
    /// scheduled (`rollback_failed` names it), and the volume opens as it is.
    pub fn open_volume(self: &Arc<Self>, id: u64) -> Result<VolumeDevice, VolError> {
        let mut p = self.lock()?;
        let first = !p.busy.contains_key(&id);
        if first && !p.info(id)?.snapshot && !p.dev.info().read_only && p.pending_rollback(id).is_some() {
            match p.apply_pending_rollback(id, BEFORE_ROLLBACK, clock_ns()) {
                Ok(Some((to, kept))) => {
                    self.rollback_failed.store(0, Ordering::Relaxed);
                    klog::event!(Info, "bitsvol", "volume.rollback_applied", pool = self.name.clone(), id = id, to = to, kept_as = kept)
                }
                Ok(None) => {}
                Err(e) => {
                    self.rollback_failed.store(p.pending_rollback(id).unwrap_or(0), Ordering::Relaxed);
                    klog::event!(Error, "bitsvol", "volume.rollback_failed", pool = self.name.clone(), id = id, error = alloc::format!("{e:?}"); "the volume opens unchanged; the rollback stays scheduled")
                }
            }
        }
        let v = p.info(id)?;
        {
            let mut r = self.releases.lock();
            let need = (r.live + 1).saturating_sub(r.ids.len());
            r.ids.try_reserve(need).map_err(|_| VolError::NoMemory)?;
            r.live += 1;
        }
        *p.busy.entry(id).or_insert(0) += 1;
        let bs = p.block_size();
        let per_chunk = p.chunk_size() / bs;
        let info = BlockInfo {
            block_size: bs,
            blocks: v.size / bs as u64,
            read_only: v.read_only,
            max_blocks_per_request: per_chunk.saturating_mul(16),
            supports_discard: true,
            has_volatile_cache: true,
        };
        Ok(VolumeDevice { h: self.clone(), id, name: alloc::format!("{}.v{}", self.name, id), info })
    }
}

pub struct VolumeDevice {
    h: Arc<PoolHandle>,
    id: u64,
    name: String,
    info: BlockInfo,
}

impl VolumeDevice {
    pub fn id(&self) -> u64 {
        self.id
    }

    /// BL-2 (Linux 7.3.1 drivers/md/dm-thin.c `thin_map` :4326, `process_bio` :2006: the bio remapped and sent on with no
    /// pool-wide lock): the chunks are mapped under the pool lock, a short hold (only a tree page that is not
    /// cached is read under it), and the data is read without it, by many readers at once. A chunk is given out
    /// again only after a commit released it, and `Pool::release` bumps the reuse count before that: if the count
    /// moved during the read, the bytes may already be another volume's, so the read goes again; after
    /// `READ_TRIES` it reads under the lock, as before BL-2 (no commit can release anything then).
    fn read_mapped(&self, lba: u64, len: usize, exec: &mut dyn FnMut(&[Piece]) -> Result<(), DevError>) -> Result<(), DevError> {
        for round in 0..READ_TRIES {
            let (plan, seen) = {
                let mut g = self.h.lock_io()?;
                let plan = g.read_plan(self.id, lba, len).map_err(VolError::to_dev)?;
                (plan, self.h.reuse.load(Ordering::SeqCst))
            };
            let r = exec(&plan);
            if self.h.reuse.load(Ordering::SeqCst) == seen {
                return r;
            }
            READ_RETRIES.inc();
            klog::event!(Debug, "bitsvol", "read.retry", pool = self.h.name.clone(), vol = self.id, lba = lba, round = round + 1;
                "a commit released chunks during a read without the pool lock: read again");
        }
        READ_LOCKED.inc();
        klog::event!(Info, "bitsvol", "read.locked", pool = self.h.name.clone(), vol = self.id, lba = lba, tries = READ_TRIES;
            "chunks were released during every try: this read runs under the pool lock");
        let mut g = self.h.lock_io()?;
        let plan = g.read_plan(self.id, lba, len).map_err(VolError::to_dev)?;
        exec(&plan)
    }
    pub fn pool(&self) -> &Arc<PoolHandle> {
        &self.h
    }
}

impl Drop for VolumeDevice {
    fn drop(&mut self) {
        // Room for this push was reserved at open (`Releases`), so it never allocates.
        self.h.releases.lock().ids.push(self.id);
        if let Some(mut p) = self.h.pool.try_lock() {
            self.h.apply_releases(&mut p);
        }
    }
}

/// An open snapshot window; dropping it ends the deferral at once and the copy window with the next pool lock.
pub struct Window(Arc<PoolHandle>);

impl Drop for Window {
    fn drop(&mut self) {
        *self.0.defer.lock() = None;
        self.0.window_end.store(true, Ordering::Release);
        if let Some(mut p) = self.0.pool.try_lock() {
            self.0.apply_releases(&mut p);
        }
        self.0.window_closed.wake_all();
    }
}

impl BlockDevice for VolumeDevice {
    fn name(&self) -> &str {
        &self.name
    }
    fn info(&self) -> BlockInfo {
        self.info
    }
    fn read_blocks(&self, lba: u64, buf: &mut [u8], prio: IoPriority) -> Result<(), DevError> {
        let (dev, bs) = (self.h.dev.clone(), self.info.block_size as u64);
        self.read_mapped(lba, buf.len(), &mut |plan| read_pieces(&dev, bs, plan, buf, prio))
    }
    /// BL-2: the frames of the request go down unchanged, each mapped piece a window of them.
    fn read_frames(&self, reqs: &[FrameIo], prio: IoPriority) -> Result<(), DevError> {
        let (dev, bs) = (self.h.dev.clone(), self.info.block_size as u64);
        for r in reqs {
            let buf: &IoBuf = &r.buf;
            self.read_mapped(r.lba, buf.len(), &mut |plan| read_pieces_frames(&dev, bs, plan, buf, prio))?;
        }
        Ok(())
    }
    fn write_blocks(&self, lba: u64, buf: &[u8], fua: bool, prio: IoPriority) -> Result<(), DevError> {
        if !fua {
            return self.h.lock_io()?.write(self.id, lba, buf, false, prio).map_err(VolError::to_dev);
        }
        self.h.timed("fua", |p| p.write(self.id, lba, buf, true, prio))
    }
    fn flush(&self) -> Result<(), DevError> {
        if self.h.defer_now(self.id) {
            return Ok(());
        }
        self.h.flush_volume()
    }
    /// fsync's answer after a flush that a snapshot's freeze held back (VS-2b CR 3).
    fn settle(&self) -> Result<(), DevError> {
        self.h.settle()
    }
    fn discard(&self, lba: u64, blocks: u64) -> Result<(), DevError> {
        self.h.lock_io()?.discard(self.id, lba, blocks).map_err(VolError::to_dev)
    }
}

impl Snapshots for VolumeDevice {
    fn snapshot(&self, label: &str) -> Result<u64, DevError> {
        let now = ksync::now_ns().unwrap_or(0);
        self.h.lock()?.snapshot(self.id, label, now, false).map_err(VolError::to_dev)
    }

    /// (id, creation time in ns, label) of every snapshot taken of this volume.
    fn list(&self) -> Vec<(u64, u64, String)> {
        let Ok(p) = self.h.lock() else { return Vec::new() };
        let Ok(all) = p.list() else { return Vec::new() };
        all.into_iter().filter(|v| v.snapshot && v.origin == self.id).map(|v| (v.id, v.created_ns, v.label)).collect()
    }
}
