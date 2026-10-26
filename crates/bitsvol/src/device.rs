//! A volume of a pool as a stacked `BlockDevice` (B1 stack: partition -> crypt -> bitsvol -> ext4). One
//! sleeping lock per pool serialises its metadata; it is held only across bounded device calls, and a waiter
//! gives up after `LOCK_WAIT_NS` with `Timeout` (B4: no unbounded wait; block reports it as EIO).
//! Closing a device never waits for the pool lock: the release goes to a short spin-locked queue whose room was
//! reserved at open, and the next holder of the pool lock applies it.
//! A snapshot window (`PoolHandle::window`, VS-2b) lets the flushes of a file system's freeze return at once:
//! the window's copy-on-every-write keeps every power-cut state a prefix of the writes, and the commit after
//! the window makes them durable together.

use crate::error::VolError;
use crate::pool::Pool;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use devmodel::DevError;
use devmodel::block::{BlockDevice, BlockInfo, IoPriority, Snapshots};
use ksync::{Mutex, MutexGuard, SpinLock, WaitQueue};

pub const LOCK_WAIT_NS: u64 = 30_000_000_000;
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

/// A flush that holds the pool lock longer than this is logged (commit time = what other I/O waits).
const SLOW_COMMIT_NS: u64 = 1_000_000_000;

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
}

impl PoolHandle {
    pub fn new(pool: Pool, name: &str) -> Arc<PoolHandle> {
        Self::with_wait(pool, name, LOCK_WAIT_NS)
    }

    /// As `new`, with another bound for lock waits (tests).
    pub fn with_wait(pool: Pool, name: &str, wait_ns: u64) -> Arc<PoolHandle> {
        Arc::new(PoolHandle {
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
        let mut p = self.lock()?;
        self.commit_locked(&mut p, |p| p.commit())
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

    pub fn lock(&self) -> Result<MutexGuard<'_, Pool>, DevError> {
        let mut g = self.pool.lock_timeout(self.wait_ns).ok_or_else(|| {
            klog::event!(Warn, "bitsvol", "pool.lock_timeout", pool = self.name.clone(), wait_ns = self.wait_ns);
            DevError::Timeout
        })?;
        self.apply_releases(&mut g);
        Ok(g)
    }

    fn apply_releases(&self, p: &mut Pool) {
        if self.window_end.swap(false, Ordering::AcqRel) {
            p.end_copy_window();
        }
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

impl VolumeDevice {
    /// Runs a commit under the pool lock; one that holds the lock longer than `SLOW_COMMIT_NS` is logged
    /// (two clock reads per commit, which already costs two device flushes).
    fn timed_commit(&self, f: impl FnOnce(&mut Pool) -> crate::error::Result<()>) -> Result<(), DevError> {
        let mut p = self.h.lock()?;
        let t0 = ksync::now_ns();
        let r = self.h.commit_locked(&mut p, f);
        if let (Some(a), Some(b)) = (t0, ksync::now_ns()) {
            let ns = b.saturating_sub(a);
            if ns > SLOW_COMMIT_NS {
                klog::event!(Warn, "bitsvol", "commit.slow", pool = self.h.name.clone(), ns = ns);
            }
        }
        r
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
        self.h.lock()?.read(self.id, lba, buf, prio).map_err(VolError::to_dev)
    }
    fn write_blocks(&self, lba: u64, buf: &[u8], fua: bool, prio: IoPriority) -> Result<(), DevError> {
        if !fua {
            return self.h.lock()?.write(self.id, lba, buf, false, prio).map_err(VolError::to_dev);
        }
        self.timed_commit(|p| p.write(self.id, lba, buf, true, prio))
    }
    fn flush(&self) -> Result<(), DevError> {
        if self.h.defer_now(self.id) {
            return Ok(());
        }
        self.timed_commit(|p| p.commit())
    }
    /// fsync's answer after a flush that a snapshot's freeze held back (VS-2b CR 3).
    fn settle(&self) -> Result<(), DevError> {
        self.h.settle()
    }
    fn discard(&self, lba: u64, blocks: u64) -> Result<(), DevError> {
        self.h.lock()?.discard(self.id, lba, blocks).map_err(VolError::to_dev)
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
