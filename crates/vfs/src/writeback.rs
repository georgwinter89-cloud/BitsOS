//! Background write-back (FS-5): files whose page cache has a backend register here; `flush_all` writes
//! their dirty pages back through the backend's ordering hooks (`pagecache::Backend`). The flusher thread
//! (class Background, B4/FS-12) lives in `vfs-sys` and calls `flush_all` every few seconds; `sync(2)` and
//! `syncfs(2)` (FS-1b) will call it too. tmpfs files never register: they have nothing to write.
//!
//! The list holds weak references, so a registered file ends normally; dead entries are pruned on each pass.

use crate::pagecache::SyncKind;
use crate::Errno;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use ksync::{Spare, SpinLock, WaitQueue};
use uapi::errno::*;

/// Something with dirty data to write back (a file with a backend, later a whole file system).
pub trait Flush: Send + Sync {
    fn flush(&self, why: SyncKind) -> Result<(), Errno>;
    /// The last write-back before the devices stop (Reload, reboot, power-off), after every `flush`: a file
    /// system with a journal empties it and marks itself clean (ext4: `ext4_put_super`'s part, FS-8c), so the
    /// next kernel mounts without a replay. `Ok(true)` when it did something (counted in `Pass::flushed`).
    /// Default: nothing more than `flush`. It must not wait without a bound (a frozen volume stays as it is).
    fn stop(&self) -> Result<bool, Errno> {
        Ok(false)
    }
}

impl Flush for crate::inode::Inode {
    fn flush(&self, why: SyncKind) -> Result<(), Errno> {
        self.sync(why)
    }
}

static LIST: SpinLock<Vec<Weak<dyn Flush>>> = SpinLock::named("vfs.writeback.list", Vec::new());
/// Whole file systems (FS-7's FAT volume): their metadata and the device's cache, after the files.
static FS_LIST: SpinLock<Vec<Weak<dyn Flush>>> = SpinLock::named("vfs.writeback.fs", Vec::new());

klog::counter!(FLUSH_PASSES, "vfs.writeback.passes");
klog::counter!(FLUSH_ERRORS, "vfs.writeback.errors");

/// Registers `w` for background write-back (ENOMEM when the list cannot grow).
pub fn register(w: Weak<dyn Flush>) -> Result<(), Errno> {
    push_grown(&LIST, w)
}

/// `w` onto a list behind a spin lock. The list grows with the lock dropped (`ksync::Spare`): under it the heap can
/// neither reclaim nor use single frames, and with 260 000 files registered (Mini-PC 11.11.2026, after Steam had
/// checked a game) the next doubling was 8 MiB in one piece.
fn push_grown(list: &SpinLock<Vec<Weak<dyn Flush>>>, w: Weak<dyn Flush>) -> Result<(), Errno> {
    let mut spare = Spare::new();
    loop {
        let mut l = list.lock();
        if !spare.fits(&mut *l, 1) {
            drop(l);
            spare.grow().map_err(|()| ENOMEM)?;
            continue;
        }
        l.push(w);
        return Ok(());
    }
}

/// Registers a whole file system (ENOMEM when the list cannot grow): every pass flushes it after the files.
pub fn register_fs(w: Weak<dyn Flush>) -> Result<(), Errno> {
    push_grown(&FS_LIST, w)
}

/// Everything durable and every file system clean, before the devices stop: every file's dirty pages, every
/// registered file system (its metadata, then its device's cache flush), then each file system's `stop`
/// (FS-8c: ext4 checkpoints its journal and marks itself clean). Thread context, before a reboot or a Reload
/// stops the devices (FS-7 change request: RB's Reload, MC's `reboot`, CP's console reboot/poweroff call it
/// before `quiesce_all`). The kernel goes on normally afterwards if the stop is called off (a file system is
/// dirty again at its next change).
pub fn sync_all() -> Pass {
    let mut p = flush_all(SyncKind::Fsync);
    for f in live_fs() {
        match f.stop() {
            Ok(true) => p.flushed += 1,
            Ok(false) => {}
            Err(e) => {
                p.failed += 1;
                FLUSH_ERRORS.inc();
                klog::event!(Warn, "vfs", "writeback.stop_failed", errno = e; "a file system could not be made clean before the stop");
            }
        }
    }
    p
}

/// `sync(2)`/`syncfs(2)`: everything durable (`ksys_sync`), journals committed but not emptied (Linux's
/// `ext4_sync_fs` commits; only an unmount or a freeze checkpoints).
pub fn sync_durable() -> Pass {
    flush_all(SyncKind::Fsync)
}

fn live_fs() -> Vec<Arc<dyn Flush>> {
    live_of(&FS_LIST).unwrap_or_default()
}

/// The live objects of a list. The answer's room is made with the lock dropped (as `push_grown`: one pass of the
/// flusher asked for 4 MiB in one piece under the spin lock, was refused and wrote nothing); the list may grow
/// meanwhile, then the room is made again. None: no memory.
fn live_of(list: &SpinLock<Vec<Weak<dyn Flush>>>) -> Option<Vec<Arc<dyn Flush>>> {
    let mut v: Vec<Arc<dyn Flush>> = Vec::new();
    loop {
        let need = {
            let mut l = list.lock();
            l.retain(|w| w.strong_count() > 0);
            if l.len() <= v.capacity() {
                v.extend(l.iter().filter_map(Weak::upgrade));
                return Some(v);
            }
            l.len()
        };
        v.try_reserve_exact(need).ok()?;
    }
}

/// Registered objects still alive (files and file systems).
pub fn registered() -> usize {
    LIST.lock().iter().filter(|w| w.strong_count() > 0).count() + FS_LIST.lock().iter().filter(|w| w.strong_count() > 0).count()
}

/// What one pass did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pass {
    pub flushed: usize,
    pub failed: usize,
}

/// FS-STALL round 5: the flusher (`vfs-sys`) waits here between passes: its period, or sooner when a writer over the
/// dirty limit asks for a pass (`balance_dirty`).
pub fn wait_for_work(period_ns: u64) {
    WORK.wait_until_timeout(|| KICKED.load(Ordering::Acquire), period_ns);
    KICKED.store(false, Ordering::Release);
}

/// After a pass of the flusher: writers waiting for one go on.
pub fn pass_done() {
    PASSES.fetch_add(1, Ordering::AcqRel);
    DONE.wake_all();
}

/// Asks the flusher for a pass now.
pub fn kick() {
    if !KICKED.swap(true, Ordering::AcqRel) {
        WORK.wake_all();
    }
}

static KICKED: AtomicBool = AtomicBool::new(false);
static PASSES: AtomicU64 = AtomicU64::new(0);
static WORK: WaitQueue = WaitQueue::new();
static DONE: WaitQueue = WaitQueue::new();

/// What a writer over the dirty limit does (`balance_dirty`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Balance {
    /// Under the limit: nothing.
    Go,
    /// Its own file holds a large share of the dirty pages: it writes that file back itself.
    WriteOwn,
    /// It waited for the flusher (kicked), at most a pause in proportion to what it dirtied.
    Waited,
}

/// The longest a writer waits for the flusher per write (Linux's `MAX_PAUSE`, 200 ms).
const MAX_PAUSE_NS: u64 = 200_000_000;
/// Pause per page the write dirtied (about 1 GB/s of write-back).
const PAUSE_PER_PAGE_NS: u64 = 4_000;

/// `balance_dirty_pages`, small: under the limit nothing; over it, a writer whose file holds a quarter or more of
/// the dirty pages writes it back itself (its own pace is the disk's), any other writer kicks the flusher and waits
/// for its pass, at most in proportion to the pages it dirtied (a 100-byte write waits microseconds, not a commit).
/// Never a commit or a device flush per write. `own`: the writer's file's dirty pages (`PageCache::own_dirty`).
pub fn balance_dirty(own: u64, dirtied: u64) -> Balance {
    if !crate::pagecache::dirty_over_limit() {
        return Balance::Go;
    }
    let total = crate::pagecache::dirty_pages_total();
    if own.saturating_mul(4) >= total {
        return Balance::WriteOwn;
    }
    let seen = PASSES.load(Ordering::Acquire);
    kick();
    let pause = dirtied.max(1).saturating_mul(PAUSE_PER_PAGE_NS).min(MAX_PAUSE_NS);
    DONE.wait_until_timeout(|| PASSES.load(Ordering::Acquire) != seen || !crate::pagecache::dirty_over_limit(), pause);
    BALANCE_WAITS.inc();
    Balance::Waited
}

klog::counter!(BALANCE_WAITS, "vfs.dirty_balance_waits");

/// Writes back every registered object (outside the list lock; the backends may sleep on their devices).
pub fn flush_all(why: SyncKind) -> Pass {
    let Some(live) = live_of(&LIST) else {
        return Pass { flushed: 0, failed: 1 };
    };
    // Whole file systems after their files (FS-7: FAT's entries, FSInfo and the stick's cache flush).
    let fs = live_fs();
    let mut p = Pass::default();
    for f in live.iter().chain(fs.iter()) {
        match f.flush(why) {
            Ok(()) => p.flushed += 1,
            Err(e) => {
                p.failed += 1;
                FLUSH_ERRORS.inc();
                klog::event!(Warn, "vfs", "writeback.failed", errno = e; "a background write-back failed; its pages stay dirty");
            }
        }
    }
    FLUSH_PASSES.inc();
    p
}
