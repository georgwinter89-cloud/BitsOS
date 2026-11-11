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
//!
//! FS-STALL (Steam's UI stalled 8-27 s behind fsyncs on the Mini-PC): no device flush and no wait for the device
//! under `ext4.vol` on the common paths. A commit writes the log with the lock released (`commit_out`, lock
//! `ext4.commit` > `ext4.vol`, one commit at a time; `journal`'s module text); a file read maps under the lock and
//! reads the blocks after it; read-only calls never read the device under it (`read_op`); `writable` and a file's
//! st_blocks need no lock. Left under the lock: a change's own metadata reads on a cache miss, and the copy of a
//! write-back step's data into the device's cache.

use crate::dev::Dev;
use crate::gd::{self, Table};
use crate::inode::DiskInode;
use crate::le::*;
use crate::sb::{self, Access, Geo};
use crate::state::{Node, State, MISS};
use core::cell::Cell;
use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
    /// One commit written to the log at a time, the volume lock released (`commit_out`).
    commit_lock: Mutex<()>,
    /// Its holder's requests go at the priority of the threads waiting for it (`block::prio::Lift`).
    commit_lift: block::prio::Lift,
    /// `State::stopped`: a failed write made the volume read-only.
    stopped: Arc<AtomicBool>,
}

/// Rounds of a read-only call that met blocks the device's cache did not hold (`read_op`): each round reads the
/// missing ones with the lock released; then the call reads under the lock.
const MISS_ROUNDS: u32 = 32;

/// Rounds of one `commit_out` that may retry owed home writes before it answers ENOMEM.
const HOME_ROUNDS: u32 = 3;

/// ETIMEDOUT marks a request of this volume's own device that timed out (`CachedDiskDev`, FS-STALL review E5);
/// programs get EIO for it, as before.
fn user_errno(e: i64) -> i64 {
    if e == ETIMEDOUT { EIO } else { e }
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
        Volume::mount_stage(dev, opts, None).map_err(user_errno)
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
        let stopped = Arc::new(AtomicBool::new(false));
        let mut st = State { dev: dev.clone(), geo: geo.clone(), sb: raw, gd, nodes: BTreeMap::new(), reserved: 0, now: opts.now, next_gen: seed, writable: access == Access::ReadWrite, written: 0, jnl: None, dirty: false, frozen: false, stopped: stopped.clone(), no_io: Cell::new(false), miss: Cell::new(None) };
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
        let vol = Arc::new(Volume { geo, access, st: Mutex::named("ext4.vol", st), dev, dropped: SpinLock::named("ext4.dropped", Vec::new()), label, frozen: AtomicBool::new(false), thawed: WaitQueue::new(), wb_gave_up: AtomicBool::new(false), commit_lock: Mutex::named("ext4.commit", ()), commit_lift: block::prio::Lift::new(), stopped });
        let _ = lift_dev;
        if writable {
            crate::file::write_bufs_init();
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
        self.access == Access::ReadWrite && !self.stopped.load(Ordering::Acquire)
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
    /// transaction grown past a quarter of the log is committed first, with the lock released (so one
    /// operation's transactions land in one journal transaction).
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
        let mut commits = 0;
        loop {
            self.wait_thawed(writeback)?;
            let mut st = self.lock();
            if !st.writable {
                return Err(EROFS);
            }
            if st.frozen {
                continue;
            }
            st.ensure_dirty()?;
            if st.journal_need_commit() && commits < 2 {
                drop(st);
                self.commit_out(None)?;
                commits += 1;
                continue;
            }
            return Ok(st);
        }
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

    /// The VFS holds inode `ino` (a regular file's ops object exists): its st_blocks as the volume keeps it.
    pub fn hold(&self, ino: u32) -> Result<Arc<AtomicU64>, i64> {
        // Read once without the lock held over the device, then again under it (from memory now): the inode may
        // have changed in between.
        self.read_op(|st| st.inode(None, ino))?;
        let mut st = self.lock();
        let di = st.inode(None, ino)?;
        let blocks = di.blocks512(&st.geo);
        let n = st.nodes.entry(ino).or_insert_with(|| Node { di, holders: 0, reserved: BTreeMap::new(), blocks: Arc::new(AtomicU64::new(blocks)) });
        n.holders += 1;
        Ok(n.blocks.clone())
    }

    /// The VFS let go of inode `ino` (the last reference to its ops object went). Only noted: the drop may
    /// come under any lock, freeing needs the volume lock and the device (done by the next `lock`; a cut
    /// before leaves it on the orphan list for the next mount).
    pub fn forget(&self, ino: u32) {
        self.dropped.lock().push(ino);
    }

    /// FS-STALL: a read-only call under the lock that never waits for the device there. A block the device's
    /// cache does not hold stops it (`State::no_io`); it is read with the lock released and the call starts over
    /// (`MISS_ROUNDS` times, then it reads under the lock as before). `f` must not change the state before its
    /// last read.
    fn read_op<T>(&self, mut f: impl FnMut(&mut State) -> Result<T, i64>) -> Result<T, i64> {
        for _ in 0..MISS_ROUNDS {
            let missed = {
                let mut st = self.lock();
                st.no_io.set(true);
                let r = f(&mut st);
                st.no_io.set(false);
                match r {
                    Err(MISS) => st.miss.take(),
                    r => return r,
                }
            };
            let Some((at, len)) = missed else { break };
            let mut tmp = Vec::new();
            if tmp.try_reserve_exact(len).is_err() {
                break;
            }
            tmp.resize(len, 0);
            if self.dev.read(at, &mut tmp).is_err() {
                break;
            }
        }
        let mut st = self.lock();
        f(&mut st)
    }

    pub fn stat(&self, ino: u32) -> Result<DiskInode, i64> {
        self.read_op(|st| st.inode(None, ino))
    }

    pub fn list(&self, dir: u32) -> Result<Vec<(Vec<u8>, u32, u8)>, i64> {
        self.read_op(|st| {
            let di = st.inode(None, dir)?;
            if !di.is_dir() {
                return Err(ENOTDIR);
            }
            st.list_dir(&di)
        })
    }

    pub fn readlink(&self, ino: u32) -> Result<Vec<u8>, i64> {
        self.read_op(|st| Self::readlink_in(st, ino))
    }

    fn readlink_in(st: &State, ino: u32) -> Result<Vec<u8>, i64> {
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

    /// A regular file's blocks are mapped under the lock and read after it (a page-cache fill; a truncate meanwhile
    /// makes the cache drop what it read, `PageCache::fill_with`'s epoch). Other inodes read under the lock.
    pub fn read(&self, ino: u32, off: u64, buf: &mut [u8]) -> Result<usize, i64> {
        let Some(plan) = self.file_plan(ino, off, buf.len())? else {
            return self.read_op(|st| {
                let di = st.inode(None, ino)?;
                st.read_file(&di, off, buf)
            });
        };
        let (bs, blocks) = (self.geo.bs, self.geo.blocks);
        crate::vol_data::read_by_plan(&plan, bs, buf, &mut |p, dst| {
            let n = (dst.len() as u64).div_ceil(bs);
            if p.checked_add(n).is_none_or(|e| e > blocks) {
                return Err(EUCLEAN);
            }
            self.dev.read(p * bs, dst)
        })
    }

    /// Where a regular file's bytes [off, off + len) lie, mapped under the lock (`read_op`: no device wait there).
    /// None: not a regular file, or inline data (read through the inode under the lock).
    pub(crate) fn file_plan(&self, ino: u32, off: u64, len: usize) -> Result<Option<crate::vol_data::ReadPlan>, i64> {
        self.read_op(|st| {
            let di = st.inode(None, ino)?;
            if di.is_reg() { st.read_plan(&di, off, len) } else { Ok(None) }
        })
    }

    pub fn xattrs(&self, ino: u32) -> Result<Vec<(Vec<u8>, Vec<u8>)>, i64> {
        self.read_op(|st| {
            let di = st.inode(None, ino)?;
            st.xattrs(&di)
        })
    }

    /// Writes a run of a file's pages (`write_run`): the disk size follows up to `size`.
    pub fn write(&self, ino: u32, off: u64, data: &[u8], size: u64, pages: core::ops::Range<u64>) -> Result<(), i64> {
        let mut st = self.lock_wb()?;
        // This run's promises are used now; the others' stay out of reach.
        // A run may use as much of the root reserve as it was promised (root's writes reserve from it). A promise
        // into the reserve can be smaller than what the reserve still has to give: `write_run`'s steps, and any short
        // run of root's with the volume already below the reserve. What was promised is always there (`reserve_pages`).
        let promised = st.release_reserved(ino, pages);
        let keep = (st.reserved + sb::reserved_blocks(&st.sb, &st.geo).saturating_sub(promised)).min(st.free_blocks().saturating_sub(promised));
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
        let same = self.read_op(|st| {
            let mut di = st.inode(None, ino)?;
            let before = di.raw.clone();
            m.apply(&mut di);
            Ok(di.raw == before)
        })?;
        if same {
            return Ok(());
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
    /// FS-STALL: the commit is written with the volume lock released (`commit_out`); this fsync's changes are in the
    /// running transaction now, or in the one in flight. When another fsync's commit took them, that commit's
    /// flushes came after this fsync's data reached the device (its data was written before the call, and that
    /// commit began after this call saw its transaction running): nothing more to flush.
    pub fn flush(&self) -> Result<(), i64> {
        let r = self.flush_inner();
        r.map_err(user_errno)
    }

    fn flush_inner(&self) -> Result<(), i64> {
        // The transaction this fsync needs: the running one, else the one in flight or waiting for its retry (it
        // may hold this file's last change). `commit_out` answers only once it is committed, or with an error: never
        // Ok without that (review E1, round 2 item 1).
        let target = {
            let st = self.lock();
            match st.jnl.as_ref() {
                Some(j) if !j.run.is_empty() || j.committing.is_some() => {
                    if !st.writable {
                        // Changes that will never reach the disk.
                        return Err(EIO);
                    }
                    Some(match &j.committing {
                        Some(c) if j.run.is_empty() => c.tid(),
                        _ => j.tid,
                    })
                }
                _ => None,
            }
        };
        let logged = match target {
            Some(tid) => self.commit_out(Some(tid))?,
            None => false,
        };
        if !logged {
            self.dev.flush()?;
        }
        self.dev.settle()
    }

    /// Commits the journal (FS-STALL): a transaction is taken out under the volume lock, written and flushed
    /// without it, booked under it again; one at a time (`ext4.commit`). A failed one goes first, then the running
    /// one. `upto`: an fsync's transaction, done when it and all before it are committed (EIO when the volume
    /// stopped first); None: everything there is now. Ok(true): a commit covered it.
    fn commit_out(&self, upto: Option<u32>) -> Result<bool, i64> {
        self.commit_out_inner(upto).map_err(user_errno)
    }

    fn commit_out_inner(&self, upto: Option<u32>) -> Result<bool, i64> {
        let lift = &self.commit_lift;
        let _one = match self.commit_lock.try_lock() {
            Some(g) => g,
            None => {
                let _w = lift.wait();
                self.commit_lock.lock()
            }
        };
        let _held = lift.hold();
        let mut logged = false;
        // Each round commits one transaction (a waiting retry first, then the running one), so an fsync's target
        // is reached after at most two, unless a commit fails (an error) or nothing is left to log. Home writes
        // still owed that keep finding no memory end the call with ENOMEM after `HOME_ROUNDS` (round 3: it looped
        // holding `ext4.commit`); the volume stays writable, the next commit tries them again.
        let mut home_rounds = 0;
        for round in 0.. {
            if upto.is_none() && round >= 3 {
                break;
            }
            let plan = {
                let mut st = self.lock();
                let Some(j) = st.jnl.as_ref() else { return Ok(logged) };
                if upto.is_some_and(|t| j.committed(t)) {
                    return Ok(true);
                }
                if upto.is_none() && logged && j.run.is_empty() && j.committing.is_none() {
                    return Ok(true);
                }
                if !st.writable {
                    return if upto.is_some() { Err(EIO) } else { Ok(logged) };
                }
                if st.frozen {
                    // A freeze committed everything before it: a target still open cannot be made durable now.
                    return if upto.is_some() { Err(EIO) } else { Ok(logged) };
                }
                if j.retry && j.home_only {
                    home_rounds += 1;
                    if home_rounds > HOME_ROUNDS {
                        return Err(ENOMEM);
                    }
                }
                st.timeout_begin();
                // A holder of the volume lock ends what it waits for itself (`settle_commit`): none in flight here.
                if let Err(e) = st.settle_commit() {
                    return Err(st.commit_failed(e));
                }
                match st.begin_commit() {
                    Ok(Some(p)) => p,
                    Ok(None) => return Ok(logged),
                    Err(e) => return Err(st.commit_failed(e)),
                }
            };
            plan.tx.finish(plan.run(Some(lift)));
            let tx = plan.tx.clone();
            drop(plan);
            let mut st = self.lock();
            let mine = st.jnl.as_ref().is_some_and(|j| j.in_flight() && j.committing.as_ref().is_some_and(|c| Arc::ptr_eq(c, &tx)));
            if mine {
                st.end_commit(tx).map_err(|e| st.commit_failed(e))?;
            } else {
                tx.result()?;
            }
            logged = true;
        }
        Ok(logged)
    }

    /// syncfs / unmount: everything committed and checkpointed, the log empty, the superblock "clean"
    /// without needs_recovery, then the device's cache. The commit and a first flush run without the volume lock,
    /// so what is left under it is short.
    pub fn sync_fs(&self) -> Result<(), i64> {
        self.sync_fs_inner().map_err(user_errno)
    }

    fn sync_fs_inner(&self) -> Result<(), i64> {
        self.commit_out(None)?;
        self.dev.flush()?;
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
                Err(user_errno(e))
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
        self.commit_out(None).map(|_| ())
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
        self.timeout_begin();
        let keep = self.sb.clone();
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
            // After a timeout it stays dirty and needs_recovery in memory too: the next attempt starts over.
            // So after a buffer the home writes could not get (round 3: ENOMEM is memory, not the disk: no stop).
            Err(e) if self.just_slow(e) || (e == ENOMEM && self.writable) => {
                self.sb = keep;
                Err(if e == ETIMEDOUT { EIO } else { e })
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
