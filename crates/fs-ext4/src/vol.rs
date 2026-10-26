//! One mounted ext4 volume behind its lock (`ext4.vol`, a Mutex: every change is serialised; lock order
//! `vfs.sb.ns` > `vfs.cache.sync` > `ext4.vol` > the device below (for a loop image: its file's page cache and
//! its file system's locks). The volume never calls back into the VFS while it holds `ext4.vol`, and the
//! device flush of `sync` runs after the lock is released.
//!
//! Mount checks the superblock and descriptors, refuses what V2 cannot read and mounts read-only what it
//! cannot write (`sb::parse`), replays a journal that needs recovery (`recover`; FS-8b) and finishes the
//! orphan list a cut left (`orphan_cleanup`). A writable mount clears the "cleanly unmounted" state bit (and
//! sets needs_recovery with a journal) before its first change and undoes both at `sync_fs`/freeze/unmount
//! after the log is empty. fsync commits the running journal transaction (`flush`).

use crate::dev::Dev;
use crate::gd::{self, Table};
use crate::inode::DiskInode;
use crate::le::*;
use crate::sb::{self, Access, Geo};
use crate::state::{Node, State};
use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};
use ksync::{Mutex, SpinLock, WaitQueue};
use crate::errno::*;
use vfs::abi::Time;

#[derive(Clone, Copy)]
pub struct Options {
    /// Mount writable when the volume allows it.
    pub writable: bool,
    /// The clock for file times.
    pub now: fn() -> Time,
}

impl Default for Options {
    fn default() -> Options {
        Options { writable: true, now: vfs::inode::now }
    }
}

pub struct Volume {
    pub geo: Geo,
    pub access: Access,
    pub st: Mutex<State>,
    pub dev: Arc<dyn Dev>,
    /// Inodes the VFS let go of (`forget`): freed under the lock at the next call (a drop may come under
    /// other locks).
    dropped: SpinLock<Vec<u32>>,
    pub label: [u8; 16],
    /// Frozen for a snapshot (`freeze`); changes wait in `lock_rw` until `thaw`.
    frozen: AtomicBool,
    thawed: WaitQueue,
    /// A write-back waited `WRITEBACK_WAIT` for this freeze's thaw: further write-backs fail at once.
    wb_gave_up: AtomicBool,
}

/// How long a write-back (`lock_wb`) waits for a thaw, in 100 ms slices: as long as the VFS waits for the
/// writers to drain before a freeze (10 s), so a freeze holder that never thaws cannot hang the flusher, a
/// sync or a Reload.
const WRITEBACK_WAIT: u64 = 100;

/// What `stat` needs of a node.
pub type Stat = DiskInode;

/// The volume lock; while held, the holder's disk requests are lifted (`block::prio::Lift`).
pub struct VolGuard<'a> {
    _held: Option<block::prio::Held<'a>>,
    g: ksync::MutexGuard<'a, State>,
}

impl core::ops::Deref for VolGuard<'_> {
    type Target = State;
    fn deref(&self) -> &State {
        &self.g
    }
}

impl core::ops::DerefMut for VolGuard<'_> {
    fn deref_mut(&mut self) -> &mut State {
        &mut self.g
    }
}

/// Superblock, geometry, access and group descriptors as the device holds them.
fn read_meta(dev: &dyn Dev) -> Result<(Geo, Access, Vec<u8>, Table), i64> {
    let mut raw = vec![0u8; sb::SIZE];
    dev.read(sb::OFFSET, &mut raw)?;
    let (geo, access) = sb::parse(&raw)?;
    if !geo.fits(dev.size()) {
        klog::event!(Warn, "ext4", "mount.short_device", blocks = geo.blocks, bytes = dev.size());
        return Err(EINVAL);
    }
    let gdb = geo.gdt_blocks();
    let mut table = Vec::new();
    table.try_reserve_exact((gdb * geo.bs) as usize).map_err(|_| ENOMEM)?;
    for nr in 0..gdb {
        let mut b = vec![0u8; geo.bs as usize];
        let at = gd::desc_block(&geo, nr);
        if at >= geo.blocks {
            return Err(EUCLEAN);
        }
        dev.read(at * geo.bs, &mut b)?;
        table.extend_from_slice(&b);
    }
    table.truncate(geo.groups as usize * geo.desc_size);
    let gd = Table { raw: table, ds: geo.desc_size, is64: geo.is64() };
    gd::check(&gd, &geo)?;
    Ok((geo, access, raw, gd))
}

impl Volume {
    pub fn mount(dev: Arc<dyn Dev>, opts: Options) -> Result<Arc<Volume>, i64> {
        Volume::mount_stage(dev, opts, None)
    }

    /// `replayed`: the journal was replayed already (Some(reason) for a read-only mount over a memory replay).
    fn mount_stage(dev: Arc<dyn Dev>, opts: Options, replayed: Option<&'static str>) -> Result<Arc<Volume>, i64> {
        let (geo, mut access, raw, gd) = read_meta(&*dev)?;
        if !opts.writable && access == Access::ReadWrite {
            access = Access::ReadOnly(replayed.unwrap_or("mounted read-only"));
        }
        let mut label = [0u8; 16];
        label.copy_from_slice(&raw[0x78..0x88]);
        let seed = g32(&raw, sb::WTIME) ^ geo.uuid.iter().fold(0u32, |a, &b| a.rotate_left(5) ^ b as u32);
        let recover = g32(&raw, sb::FEATURE_INCOMPAT) & sb::incompat::RECOVER != 0;
        let mut st = State { dev: dev.clone(), geo: geo.clone(), sb: raw, gd, nodes: BTreeMap::new(), reserved: 0, now: opts.now, next_gen: seed, writable: access == Access::ReadWrite, written: 0, jnl: None, dirty: false, frozen: false };
        if geo.has_compat(sb::compat::HAS_JOURNAL) {
            match crate::journal::load(&st, st.writable)? {
                Ok((j, disk)) if recover && disk.start != 0 && replayed.is_none() => return Volume::recover(dev, opts, &st, j, disk),
                Ok((j, _)) => {
                    if st.writable {
                        st.jnl = Some(j);
                    }
                }
                Err(why) => {
                    klog::event!(Warn, "ext4", "journal.unusable", why = why, recover = recover; "the journal is not used: read-only");
                    if st.writable {
                        access = Access::ReadOnly(why);
                        st.writable = false;
                    }
                }
            }
        } else if recover {
            access = Access::ReadOnly("needs_recovery without a journal");
            st.writable = false;
        }
        let writable = st.writable;
        if writable {
            st.counts_from_groups();
        }
        let lift_dev = dev.clone();
        let vol = Arc::new(Volume { geo, access, st: Mutex::named("ext4.vol", st), dev, dropped: SpinLock::named("ext4.dropped", Vec::new()), label, frozen: AtomicBool::new(false), thawed: WaitQueue::new(), wb_gave_up: AtomicBool::new(false) });
        let _ = lift_dev;
        if writable {
            let mut st = vol.st.lock();
            st.ensure_dirty()?;
            if let Err(e) = st.orphan_cleanup() {
                // `ext4_orphan_cleanup` (linux-6.6/fs/ext4/orphan.c): an orphan it cannot read ends the list
                // (s_last_orphan = 0) and the mount goes on; what is left over is e2fsck's.
                klog::event!(Warn, "ext4", "orphan.cleanup_failed", errno = e; "orphan list dropped, mount goes on");
                if st.writable {
                    let _ = st.with_tx(|st, tx| {
                        st.save_sb(tx);
                        p32(&mut st.sb, sb::LAST_ORPHAN, 0);
                        tx.sb_dirty = true;
                        Ok(())
                    });
                }
            }
            if st.jnl.is_some() {
                drop(st);
                let w: Weak<dyn vfs::writeback::Flush> = Arc::downgrade(&vol) as Weak<dyn vfs::writeback::Flush>;
                if vfs::writeback::register_fs(w).is_err() {
                    klog::event!(Warn, "ext4", "journal.no_writeback"; "the 5-second commit will not run; fsync and sync_fs still commit");
                }
            }
        }
        klog::event!(Info, "ext4", "mounted", blocks = vol.geo.blocks, bs = vol.geo.bs, groups = vol.geo.groups, rw = writable, journal = vol.st.lock().jnl.is_some(), why = match access { Access::ReadOnly(w) => w, Access::ReadWrite => "" });
        Ok(vol)
    }

    /// `jbd2_journal_recover` at mount: a writable mount replays to the home blocks, empties the log and
    /// clears needs_recovery (then mounts again from the replayed disk); a read-only one replays into memory
    /// (`OverlayDev`) and writes nothing.
    fn recover(dev: Arc<dyn Dev>, opts: Options, st: &State, j: crate::journal::Journal, disk: crate::jbd2::Jsb) -> Result<Arc<Volume>, i64> {
        let bs = st.geo.bs;
        let lay = crate::jbd2::Layout { bs: bs as usize, csum: disk.csum(), is64: disk.is64(), seed: disk.seed() };
        let log = crate::recovery::Log { dev: &*dev, map: &j.map, jsb: &disk, lay };
        crate::journal::REPLAYS.inc();
        if !st.writable {
            let ov = Arc::new(crate::dev::OverlayDev::new(dev.clone(), bs));
            let r = crate::recovery::replay(&log, st.geo.blocks, &mut |b, d| ov.put(b, d))?;
            klog::event!(Info, "ext4", "journal.replayed_in_memory", transactions = r.transactions, blocks = r.blocks, revoked = r.revoke_hits, bad = r.bad);
            return Volume::mount_stage(ov, Options { writable: false, ..opts }, Some("journal replayed in memory (read-only mount)"));
        }
        let r = crate::recovery::replay(&log, st.geo.blocks, &mut |b, d| dev.write(b * bs, &d))?;
        dev.flush()?;
        klog::event!(Info, "ext4", "journal.replayed", transactions = r.transactions, blocks = r.blocks, revoked = r.revoke_hits, bad = r.bad, next = r.end);
        if r.bad > 0 {
            // Linux fails the mount here; V2 mounts read-only and leaves the log for e2fsck.
            return Volume::mount_stage(dev, Options { writable: false, ..opts }, Some("journal replay found damaged blocks: e2fsck"));
        }
        let p0 = j.map.phys(0).ok_or(EUCLEAN)?;
        let mut b0 = vec![0u8; bs as usize];
        dev.read(p0 * bs, &mut b0)?;
        let mut empty = disk.clone();
        let s = empty.serialise(0, r.end.wrapping_add(1), disk.first);
        b0[..crate::jbd2::SB_SIZE].copy_from_slice(&s);
        dev.write(p0 * bs, &b0)?;
        dev.flush()?;
        let mut raw = vec![0u8; sb::SIZE];
        dev.read(sb::OFFSET, &mut raw)?;
        let (geo, _) = sb::parse(&raw)?;
        let f = g32(&raw, sb::FEATURE_INCOMPAT) & !sb::incompat::RECOVER;
        p32(&mut raw, sb::FEATURE_INCOMPAT, f);
        sb::set_checksum(&mut raw, &geo);
        dev.write(sb::OFFSET, &raw)?;
        dev.flush()?;
        Volume::mount_stage(dev, opts, Some("replayed"))
    }

    pub fn writable(&self) -> bool {
        self.st.lock().writable
    }

    /// The volume lock, after the inodes the VFS dropped are dealt with (not while frozen: freeing them
    /// waits for the thaw). An Interactive thread that has to wait lifts the holder's disk requests.
    pub fn lock(&self) -> VolGuard<'_> {
        let lift = self.dev.lift();
        let g = match self.st.try_lock() {
            Some(g) => g,
            None => {
                let _w = lift.and_then(|l| l.wait());
                self.st.lock()
            }
        };
        let mut st = VolGuard { _held: lift.map(|l| l.hold()), g };
        if !st.frozen {
            let gone: Vec<u32> = core::mem::take(&mut *self.dropped.lock());
            for ino in gone {
                self.reap(&mut st, ino);
            }
        }
        st
    }

    /// The lock for a change: EROFS on a read-only volume; waits while frozen; a running journal
    /// transaction grown past a quarter of the log is committed first (so one operation's transactions
    /// land in one journal transaction).
    pub fn lock_rw(&self) -> Result<VolGuard<'_>, i64> {
        self.lock_change(false)
    }

    /// `lock_rw` for a page write-back (`write_run`, the inode's times after it): while frozen it waits at
    /// most `WRITEBACK_WAIT`, then EBUSY (the pages stay dirty for the next pass). Linux never meets this
    /// (a frozen file system has no dirty pages); in V2 a mapping or a time update can still dirty one.
    pub fn lock_wb(&self) -> Result<VolGuard<'_>, i64> {
        self.lock_change(true)
    }

    fn lock_change(&self, writeback: bool) -> Result<VolGuard<'_>, i64> {
        let mut st = loop {
            self.wait_thawed(writeback)?;
            let st = self.lock();
            if !st.writable {
                return Err(EROFS);
            }
            if !st.frozen {
                break st;
            }
        };
        st.ensure_dirty()?;
        if st.journal_need_commit() {
            st.jcommit()?;
        }
        Ok(st)
    }

    fn wait_thawed(&self, writeback: bool) -> Result<(), i64> {
        let mut slices = 0u64;
        while self.frozen.load(Ordering::Acquire) {
            if writeback && (slices >= WRITEBACK_WAIT || self.wb_gave_up.load(Ordering::Acquire)) {
                if !self.wb_gave_up.swap(true, Ordering::AcqRel) {
                    klog::event!(Warn, "ext4", "freeze.writeback_gave_up", waited_ms = slices * 100;
                        "a write-back waited 10 s for the thaw: write-backs answer EBUSY until the thaw, the pages stay dirty");
                }
                return Err(EBUSY);
            }
            if vfs::dentry::interrupted() {
                return Err(EINTR);
            }
            if !self.thawed.wait_until_timeout(|| !self.frozen.load(Ordering::Acquire), 100_000_000) {
                slices += 1;
                if slices == 100 {
                    klog::event!(Warn, "ext4", "freeze.change_waits", waited_ms = slices * 100; "a change waits for the thaw");
                }
            }
        }
        Ok(())
    }

    fn reap(&self, st: &mut State, ino: u32) {
        let Some(n) = st.nodes.get_mut(&ino) else { return };
        n.holders = n.holders.saturating_sub(1);
        if n.holders > 0 {
            return;
        }
        let blocks: u64 = n.reserved.values().map(|&b| b as u64).sum();
        n.reserved.clear();
        st.reserved = st.reserved.saturating_sub(blocks);
        let links = n.di.links();
        if links == 0 && st.writable {
            if let Err(e) = st.evict(ino, true) {
                klog::event!(Warn, "ext4", "evict.failed", ino = ino, errno = e);
            }
        }
        st.nodes.remove(&ino);
    }

    /// The VFS holds inode `ino` (a regular file's ops object exists).
    pub fn hold(&self, ino: u32) -> Result<DiskInode, i64> {
        let mut st = self.lock();
        let di = st.inode(None, ino)?;
        let n = st.nodes.entry(ino).or_insert(Node { di: di.clone(), holders: 0, reserved: BTreeMap::new() });
        n.holders += 1;
        Ok(n.di.clone())
    }

    /// The VFS let go of inode `ino` (the last reference to its ops object went). Only noted: the drop may
    /// come under any lock, freeing needs the volume lock and the device (done by the next `lock`; a cut
    /// before leaves it on the orphan list for the next mount).
    pub fn forget(&self, ino: u32) {
        self.dropped.lock().push(ino);
    }

    pub fn stat(&self, ino: u32) -> Result<DiskInode, i64> {
        self.lock().inode(None, ino)
    }

    pub fn list(&self, dir: u32) -> Result<Vec<(Vec<u8>, u32, u8)>, i64> {
        let st = self.lock();
        let di = st.inode(None, dir)?;
        if !di.is_dir() {
            return Err(ENOTDIR);
        }
        st.list_dir(&di)
    }

    pub fn readlink(&self, ino: u32) -> Result<Vec<u8>, i64> {
        let st = self.lock();
        let di = st.inode(None, ino)?;
        if !di.is_symlink() {
            return Err(EINVAL);
        }
        let size = di.size();
        if size > st.geo.bs || size == 0 {
            return Err(EUCLEAN);
        }
        let mut v = vec![0u8; size as usize];
        if crate::extent::uses_extents(&di) || di.flags() & crate::inode::FL_INLINE_DATA != 0 || di.blocks512(&st.geo) != 0 {
            let n = st.read_file(&di, 0, &mut v)?;
            v.truncate(n);
        } else {
            v.copy_from_slice(&di.i_block()[..size.min(60) as usize]);
        }
        Ok(v)
    }

    pub fn read(&self, ino: u32, off: u64, buf: &mut [u8]) -> Result<usize, i64> {
        let st = self.lock();
        let di = st.inode(None, ino)?;
        st.read_file(&di, off, buf)
    }

    pub fn xattrs(&self, ino: u32) -> Result<Vec<(Vec<u8>, Vec<u8>)>, i64> {
        let st = self.lock();
        let di = st.inode(None, ino)?;
        st.xattrs(&di)
    }

    /// Writes a run of a file's pages (`write_run`): the disk size follows up to `size`.
    pub fn write(&self, ino: u32, off: u64, data: &[u8], size: u64, pages: core::ops::Range<u64>) -> Result<(), i64> {
        let mut st = self.lock_wb()?;
        // This run's promises are used now; the others' stay out of reach.
        // A run may use as much of the root reserve as it was promised (root's writes reserve from it).
        let promised = st.release_reserved(ino, pages);
        let keep = st.reserved + sb::reserved_blocks(&st.sb, &st.geo).saturating_sub(promised);
        st.write_file(ino, off, data, size, keep)
    }

    /// Promises blocks for the file pages [first, last] that `write_at` makes dirty and the disk does not
    /// map yet; ENOSPC now instead of at write-back.
    pub fn reserve(&self, ino: u32, first: u64, last: u64) -> Result<(), i64> {
        let mut st = self.lock();
        if !st.writable {
            return Err(EROFS);
        }
        st.reserve_pages(ino, first, last)
    }

    /// fallocate's blocks for [off, off + len): unwritten extents now (`State::fallocate`).
    pub fn fallocate(&self, ino: u32, off: u64, len: u64) -> Result<(), i64> {
        let mut st = self.lock_rw()?;
        let keep = st.keep_for(None);
        st.fallocate(ino, off, len, keep)
    }

    pub fn set_size(&self, ino: u32, size: u64) -> Result<(), i64> {
        let mut st = self.lock_rw()?;
        st.release_reserved(ino, size.div_ceil(4096)..u64::MAX);
        st.set_file_size(ino, size)
    }

    /// Stores what the VFS holds of an inode: mode, owner and times (`setattr`, a file's write-back).
    /// Nothing is written when the disk inode already holds them (the write-back calls this for every
    /// file it writes; an unchanged inode must not make a journal transaction every 5 seconds).
    pub fn store_meta(&self, ino: u32, m: Meta) -> Result<(), i64> {
        self.store_meta_as(ino, m, false)
    }

    /// `store_meta` from a file's write-back (`lock_wb`).
    pub fn store_meta_wb(&self, ino: u32, m: Meta) -> Result<(), i64> {
        self.store_meta_as(ino, m, true)
    }

    fn store_meta_as(&self, ino: u32, m: Meta, writeback: bool) -> Result<(), i64> {
        {
            let st = self.lock();
            let mut di = st.inode(None, ino)?;
            let before = di.raw.clone();
            m.apply(&mut di);
            if di.raw == before {
                return Ok(());
            }
        }
        let mut st = self.lock_change(writeback)?;
        st.with_tx(|st, tx| {
            let mut di = st.inode(Some(tx), ino)?;
            m.apply(&mut di);
            st.put_inode(tx, &mut di)
        })
    }

    /// `ext4_statfs` (super.c:6793): blocks less the overhead, free less the root reserve for `bavail`.
    pub fn statfs(&self) -> Result<vfs::statfs::StatFs, i64> {
        let st = self.lock();
        let geo = &st.geo;
        let itb = gd::itb_per_group(geo);
        let mut overhead = geo.first_data_block;
        for g in 0..geo.groups {
            overhead += gd::base_meta_blocks(geo, g) + 2 + itb;
        }
        if geo.has_compat(sb::compat::HAS_JOURNAL) {
            let j = g32(&st.sb, 0xE0);
            if let Ok(di) = st.inode(None, j) {
                overhead += di.blocks512(geo) / (geo.bs / 512);
            }
        }
        let free = st.free_blocks().saturating_sub(st.reserved);
        let r = sb::reserved_blocks(&st.sb, geo);
        let u = &geo.uuid;
        let fsid = u64::from_le_bytes([u[0], u[1], u[2], u[3], u[4], u[5], u[6], u[7]]) ^ u64::from_le_bytes([u[8], u[9], u[10], u[11], u[12], u[13], u[14], u[15]]);
        Ok(vfs::statfs::StatFs {
            ftype: EXT4_SUPER_MAGIC,
            bsize: geo.bs as i64,
            blocks: geo.blocks.saturating_sub(overhead),
            bfree: free,
            bavail: free.saturating_sub(r),
            files: geo.inodes as u64,
            ffree: st.free_inodes() as u64,
            fsid,
            namelen: 255,
            frsize: geo.bs as i64,
            flags: 0,
        })
    }

    /// fsync / fdatasync: the running journal transaction is committed (`ext4_sync_file` ->
    /// `ext4_force_commit`, fsync.c:109); without one, the device's cache is flushed (the data went to the
    /// device before).
    /// The device's `settle` comes last, with the volume lock dropped (VS-2b CR 3): a flush that a snapshot's
    /// freeze held back is durable when fsync returns, as on a frozen Linux file system.
    pub fn flush(&self) -> Result<(), i64> {
        let committed = {
            let mut st = self.lock();
            if st.writable && st.jnl.as_ref().is_some_and(|j| !j.run.is_empty()) { Some(st.jcommit()) } else { None }
        };
        match committed {
            Some(r) => r?,
            None => self.dev.flush()?,
        }
        self.dev.settle()
    }

    /// syncfs / unmount: everything committed and checkpointed, the log empty, the superblock "clean"
    /// without needs_recovery, then the device's cache.
    pub fn sync_fs(&self) -> Result<(), i64> {
        {
            let mut st = self.lock();
            st.make_clean()?;
        }
        self.dev.flush()?;
        self.dev.settle()
    }

    /// FsFreeze (VS-2, `ext4_freeze`, super.c:6369): the caller has drained the writers and written back the
    /// files' pages; this commits, empties the log, marks the volume clean on disk (an image taken now
    /// mounts and checks clean without a replay) and holds every further change back until `thaw`.
    pub fn freeze(&self) -> Result<(), i64> {
        if self.frozen.swap(true, Ordering::AcqRel) {
            return Err(EBUSY);
        }
        let r = {
            let mut st = self.lock();
            let r = st.make_clean();
            if r.is_ok() {
                st.frozen = true;
            }
            r
        };
        match r.and_then(|()| self.dev.flush()) {
            Ok(()) => {
                klog::event!(Info, "ext4", "frozen");
                Ok(())
            }
            Err(e) => {
                self.thaw();
                Err(e)
            }
        }
    }

    pub fn thaw(&self) {
        self.st.lock().frozen = false;
        self.wb_gave_up.store(false, Ordering::Release);
        if self.frozen.swap(false, Ordering::AcqRel) {
            klog::event!(Info, "ext4", "thawed");
        }
        self.thawed.wake_all();
    }

    pub fn is_frozen(&self) -> bool {
        self.frozen.load(Ordering::Acquire)
    }
}

/// The last reference went (unmount): what the running journal transaction holds is committed and the
/// volume marked clean (`ext4_put_super`), best effort.
impl Drop for Volume {
    /// The VFS has no unmount hook (`MountNs::umount` drops the mount), so this is `ext4_put_super`: without
    /// it a umount would leave the running transaction uncommitted and the log to replay. The device's cache
    /// flush runs after the volume lock is released, like `sync_fs`.
    fn drop(&mut self) {
        let r = {
            let mut st = self.st.lock();
            (st.writable && st.dirty && !st.frozen).then(|| st.make_clean())
        };
        if let Some(Err(e)) = r.map(|r| r.and_then(|()| self.dev.flush())) {
            klog::event!(Warn, "ext4", "unmount.sync_failed", errno = e);
        }
    }
}

/// The background write-back reaches the volume after its files (every 5 s, `vfs-sys` flusher): the running
/// journal transaction is committed (Linux's `commit=5`); a durable pass (sync(2), Reload) flushes too.
impl vfs::writeback::Flush for Volume {
    fn flush(&self, why: vfs::pagecache::SyncKind) -> Result<(), i64> {
        if why.durable() {
            return Volume::flush(self);
        }
        if self.is_frozen() {
            return Ok(());
        }
        let mut st = self.lock();
        if !st.writable {
            return Ok(());
        }
        st.jcommit()
    }
    /// Before a Reload or power-off: committed, checkpointed, clean on disk (`sync_fs`); a frozen volume was
    /// made clean by its freeze and stays as it is.
    fn stop(&self) -> Result<bool, i64> {
        if self.is_frozen() {
            return Ok(false);
        }
        self.sync_fs().map(|()| true)
    }
}

impl State {
    /// Commit, empty the log, superblock clean (`STATE_VALID`, no needs_recovery, no write-around mark).
    pub fn make_clean(&mut self) -> Result<(), i64> {
        if !self.writable || !self.dirty {
            return Ok(());
        }
        let r = self.empty_journal().and_then(|()| {
            // A writable volume had no recorded error at mount (`sb::parse`): STATE_ERROR here is the mark
            // of a transaction written around the log (`write_around`), stable now.
            let s = (g16(&self.sb, sb::STATE) | sb::STATE_VALID) & !sb::STATE_ERROR;
            p16(&mut self.sb, sb::STATE, s);
            let f = g32(&self.sb, sb::FEATURE_INCOMPAT) & !sb::incompat::RECOVER;
            p32(&mut self.sb, sb::FEATURE_INCOMPAT, f);
            self.write_super()
        });
        match r {
            Ok(()) => {
                self.dirty = false;
                Ok(())
            }
            Err(e) => Err(self.failed(e)),
        }
    }

    /// The superblock's free counts from the group descriptors (Linux keeps them in percpu counters
    /// initialised from the groups at mount and writes them with the superblock).
    pub fn counts_from_groups(&mut self) {
        let (mut b, mut i) = (0u64, 0u64);
        for g in 0..self.geo.groups {
            b += self.gd.free_blocks(g) as u64;
            i += self.gd.free_inodes(g) as u64;
        }
        if b != self.free_blocks() || i != self.free_inodes() as u64 {
            klog::event!(Info, "ext4", "sb.counts_from_groups", blocks = b, inodes = i, sb_blocks = self.free_blocks(), sb_inodes = self.free_inodes());
            self.sb = sb::with_counts(&self.sb, &self.geo, b, i.min(u32::MAX as u64) as u32);
        }
    }
}

pub const EXT4_SUPER_MAGIC: i64 = 0xEF53;

/// Inode fields the VFS owns (None: unchanged).
#[derive(Clone, Copy, Default, Debug)]
pub struct Meta {
    pub mode: Option<u16>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub atime: Option<Time>,
    pub mtime: Option<Time>,
    pub ctime: Option<Time>,
}

impl Meta {
    pub fn apply(&self, di: &mut DiskInode) {
        use crate::inode::Stamp;
        if let Some(m) = self.mode {
            di.set_mode((di.mode() & crate::inode::S_IFMT) | (m & 0o7777));
        }
        if self.uid.is_some() || self.gid.is_some() {
            di.set_owner(self.uid.unwrap_or(di.uid()), self.gid.unwrap_or(di.gid()));
        }
        if let Some(t) = self.atime {
            di.set_time(Stamp::A, t);
        }
        if let Some(t) = self.mtime {
            di.set_time(Stamp::M, t);
        }
        if let Some(t) = self.ctime {
            di.set_time(Stamp::C, t);
        }
    }
}
