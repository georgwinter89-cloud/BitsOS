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
use ksync::SpinLock;
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
    let mut l = LIST.lock();
    l.try_reserve(1).map_err(|_| ENOMEM)?;
    l.push(w);
    Ok(())
}

/// Registers a whole file system (ENOMEM when the list cannot grow): every pass flushes it after the files.
pub fn register_fs(w: Weak<dyn Flush>) -> Result<(), Errno> {
    let mut l = FS_LIST.lock();
    l.try_reserve(1).map_err(|_| ENOMEM)?;
    l.push(w);
    Ok(())
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
    let mut l = FS_LIST.lock();
    l.retain(|w| w.strong_count() > 0);
    l.iter().filter_map(Weak::upgrade).collect()
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

/// Writes back every registered object (outside the list lock; the backends may sleep on their devices).
pub fn flush_all(why: SyncKind) -> Pass {
    let live: Vec<Arc<dyn Flush>> = {
        let mut l = LIST.lock();
        l.retain(|w| w.strong_count() > 0);
        let mut v = Vec::new();
        if v.try_reserve(l.len()).is_err() {
            return Pass { flushed: 0, failed: 1 };
        }
        v.extend(l.iter().filter_map(Weak::upgrade));
        v
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
