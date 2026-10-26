//! Consistent snapshots of an ext4 volume on a bitsvol pool (B1, VS-2). Owner: VS.
//!
//! A snapshot is taken with the file system frozen (`FsFreeze`: writers drained, dirty pages written back,
//! ext4 committed and marked clean, new changes held back), so every snapshot mounts and checks clean without a
//! journal replay; the pool's snapshot is made while frozen and committed after the thaw (VS-2b, `take`). Linux does the same in two steps
//! (`fsfreeze -f`, `lvcreate -s`, `fsfreeze -u`: linux-6.6/fs/super.c freeze_super, fs/ext4/super.c:6369
//! ext4_freeze, drivers/md/dm-thin-metadata.c:1235 dm_pool_create_snap); here the freeze never outlives the
//! call that took it, so a dying caller cannot leave /data frozen.
//!
//! - `Target`: what is snapshotted (the boot mount `/data`, or any pool volume a test mounted).
//! - `take`, `list`, `size_of`, `delete`, `stats`: the operations, serialised by one sleeping lock.
//! - `mounts`: a snapshot mounted read-only (VS-3's file restore copies back from it).
//! - `dev`: `/dev/bitssnap`, the text interface `bitssnap` (user/tools/bitssnap) talks to.
//! - `schedule_rollback`, `cancel_rollback`, `pending_rollback` (VS-3): the whole volume back to a snapshot at
//!   the next mount of /data (boot or Reload; bitsvol carries it out at the volume's first open, before ext4
//!   reads anything); `system`: which system generation ran at a time and `system_pin` for "everything as at
//!   15:00"; `console`: the console commands.
//!
//! Times are wall-clock UTC ns (`time::wall::realtime_ns`): retention by age must survive a reboot.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod console;
pub mod dev;
mod ktests;
pub mod mounts;
pub mod system;

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use bitsvol::{PoolHandle, VolError};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use devmodel::DevError;
use uapi::errno::*;
use vfs::{Errno, FsFreeze, MountNs, SuperBlock};

/// How long an operation waits for another one (a take waits at most 10 s for writers, 30 s for the pool).
const OPS_WAIT_NS: u64 = 60_000_000_000;
/// Longest label (bitsvol's own bound is larger; labels are names, not texts).
pub const LABEL_MAX: usize = 64;

static OPS: ksync::Mutex<()> = ksync::Mutex::named("snap.ops", ());
static TAKEN: AtomicU64 = AtomicU64::new(0);
static UNCHANGED: AtomicU64 = AtomicU64::new(0);
static LAST_PAUSE_NS: AtomicU64 = AtomicU64::new(0);
static MAX_PAUSE_NS: AtomicU64 = AtomicU64::new(0);
/// ktests only: the next take fails inside its frozen section (the thaw on an error path is checked).
static FAIL_FROZEN: AtomicBool = AtomicBool::new(false);

/// An ext4 file system on a volume of a bitsvol pool.
#[derive(Clone)]
pub struct Target {
    /// The pool's name (stacked disks are `<pool>.v<id>`).
    pub pool_name: String,
    pub sb: Arc<SuperBlock>,
    pub pool: Arc<PoolHandle>,
    pub vol: u64,
}

impl Target {
    /// The boot mount at /data. ENODEV: nothing mounted; EOPNOTSUPP: a plain ext4 image (no pool, no
    /// snapshots); EROFS: mounted read-only (a read-only stick).
    pub fn data() -> Result<Target, Errno> {
        let m = fs_ext4::data::mounted().ok_or(ENODEV)?;
        Target::of(&vfs::root_ns(), fs_ext4::data::AT, &m)
    }

    /// The pool volume `m` mounted at `at` in `ns`.
    pub fn of(ns: &MountNs, at: &str, m: &fs_ext4::data::Mounted) -> Result<Target, Errno> {
        let (pool, vol) = m.handle.clone().ok_or(EOPNOTSUPP)?;
        let p = ns.lookup_path(at.as_bytes(), true)?;
        if !p.is_mount_root() || p.dentry.sb.fstype != "ext4" {
            return Err(ENODEV);
        }
        if p.mnt.readonly {
            return Err(EROFS);
        }
        let disk = m.disk.clone().unwrap_or_default();
        let pool_name = disk.rsplit_once(".v").map(|(a, _)| String::from(a)).unwrap_or(disk);
        Ok(Target { pool_name, sb: p.dentry.sb.clone(), pool, vol })
    }
}

pub fn vol_errno(e: VolError) -> Errno {
    match e {
        VolError::Dev(DevError::Timeout) => ETIMEDOUT,
        e => e.errno() as Errno,
    }
}

fn dev_errno(e: DevError) -> Errno {
    vol_errno(VolError::Dev(e))
}

fn now() -> u64 {
    ksync::now_ns().unwrap_or(0)
}

/// UTC ns now, 0 without a wall clock (such snapshots are kept by age rules and dropped first for space).
pub fn wall_ns() -> u64 {
    time::wall::realtime_ns().unwrap_or(0)
}

pub fn check_label(label: &str) -> Result<(), Errno> {
    let ok = !label.is_empty() && label.len() <= LABEL_MAX && label.bytes().all(|b| b.is_ascii_alphanumeric() || b"._:-".contains(&b));
    if ok { Ok(()) } else { Err(EINVAL) }
}

fn ops() -> Result<ksync::MutexGuard<'static, ()>, Errno> {
    OPS.lock_timeout(OPS_WAIT_NS).ok_or_else(|| {
        klog::event!(Warn, "snap", "ops.busy", wait_ns = OPS_WAIT_NS; "another snapshot operation did not end");
        EBUSY
    })
}

/// Thaws when dropped: every path out of `take` (errors included) thaws.
struct Frozen<'a>(&'a SuperBlock);

impl Drop for Frozen<'_> {
    fn drop(&mut self) {
        self.0.thaw();
    }
}

/// The result of a take.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Taken {
    /// The new snapshot, or with `new == false` the newest one, which already holds this state.
    pub id: u64,
    pub new: bool,
    pub created_ns: u64,
    /// The write-back and the file system's commit before the freeze (writers go on meanwhile; not part of
    /// the pause).
    pub preflush_ns: u64,
    /// Freeze (drain, write-back, ext4 commit), the pool's snapshot (in memory), and the whole pause writers saw.
    pub freeze_ns: u64,
    pub snap_ns: u64,
    pub pause_ns: u64,
    /// The pool commit that makes the snapshot durable, after the thaw (not part of the pause).
    pub commit_ns: u64,
    /// Device barriers (cache flushes, FUA writes) inside the pause and the time they took: where it goes.
    pub pause_barriers: u64,
    pub pause_barrier_ns: u64,
}

/// Freeze, snapshot, thaw, commit. `auto`: a scheduled snapshot (retention and the pool's space reclaim may drop
/// it); `if_changed`: no new snapshot when nothing was written since the newest one (checked while frozen, so the
/// dirty pages are in).
///
/// VS-2b keeps the frozen pause short (desk-minipc-w11.md S4: 71-405 ms on the stick): everything that can
/// happen with writers going on happens before the freeze (write-back, then the ext4 commit and checkpoint,
/// `sync_fs`), so the freeze only holds what changed in the last moment; inside it the device barriers wait
/// (`PoolHandle::window`: the pool's copy window keeps every power-cut state a prefix of the writes, so the
/// freeze's flushes may return at once) and the pool's snapshot is made in memory; one pool commit after the
/// thaw makes the freeze's writes and the snapshot durable together (`commit_ns`, outside the pause).
pub fn take(t: &Target, label: &str, auto: bool, if_changed: bool) -> Result<Taken, Errno> {
    check_label(label)?;
    let _ops = ops()?;
    let created_ns = wall_ns();
    if created_ns == 0 {
        klog::event!(Warn, "snap", "no_wall_clock"; "the snapshot gets time 0");
    }
    // Writers go on meanwhile (Linux users run `sync` before `fsfreeze -f`): one pass of the background flusher
    // over every file, then the file system's own commit and checkpoint.
    let p0 = now();
    let _ = vfs::writeback::flush_all(vfs::pagecache::SyncKind::Background);
    if let Some(o) = &t.sb.ops
        && let Err(e) = o.sync()
    {
        klog::event!(Info, "snap", "presync_failed", pool = t.pool_name.as_str(), errno = e; "the freeze commits instead");
    }
    // A freeze someone else holds (FIFREEZE): EBUSY at once, as `freeze_super` (super.c:1940) answers a second
    // freezer; no window, so nothing is deferred for a freeze that is not this take's.
    if t.sb.is_frozen() {
        klog::event!(Info, "snap", "busy", pool = t.pool_name.as_str(); "the volume is frozen by someone else");
        return Err(EBUSY);
    }
    let b0 = t.pool.lock().map(|p| p.barriers()).unwrap_or_default();
    let t0 = now();
    // The volume's flushes wait while this take's `freeze` runs (drain included): the freeze's own, and those of
    // fsyncs under way meanwhile (the block dispatcher sends both, bitsvol cannot tell them apart). Those are
    // durable with the commit right after the thaw; every power-cut state stays a prefix of the writes, and an
    // fsync that asks `PoolHandle::settle` returns only then (notes vs-VS-2b.md, decision 2 and the fixer's
    // change request for the fsync path).
    let (sb, freezing) = (t.sb.clone(), Arc::new(AtomicBool::new(false)));
    let f2 = freezing.clone();
    let window = t.pool.window(t.vol, Arc::new(move || f2.load(Ordering::Acquire) && sb.is_frozen())).map_err(dev_errno)?;
    let r = frozen_part(t, &freezing, label, created_ns, auto, if_changed);
    let t3 = now();
    let b1 = t.pool.lock().map(|p| p.barriers()).unwrap_or_default();
    // After the thaw (or a failed freeze, whose flushes may have waited as well) on every path; once more when
    // it fails (a lock timeout under load). If both fail the deferred flushes stay owed: `PoolHandle::settle`
    // (fsync's answer) and the next flush of the volume commit for real and report the error.
    let mut c = t.pool.commit().map_err(dev_errno);
    if let Err(e) = c {
        klog::event!(Warn, "snap", "commit_retry", pool = t.pool_name.as_str(), errno = e);
        c = t.pool.commit().map_err(dev_errno);
    }
    drop(window);
    let t4 = now();
    if let Err(e) = c {
        klog::event!(Warn, "snap", "commit_failed", pool = t.pool_name.as_str(), errno = e, owed = t.pool.owed_flushes(); "the freeze's writes and the snapshot are not durable yet");
    }
    let (id, new, t1, t2) = r?;
    c?;
    let taken = Taken {
        id,
        new,
        created_ns,
        preflush_ns: t0.saturating_sub(p0),
        freeze_ns: t1.saturating_sub(t0),
        snap_ns: t2.saturating_sub(t1),
        pause_ns: t3.saturating_sub(t0),
        commit_ns: t4.saturating_sub(t3),
        pause_barriers: b1.0.saturating_sub(b0.0),
        pause_barrier_ns: b1.1.saturating_sub(b0.1),
    };
    LAST_PAUSE_NS.store(taken.pause_ns, Ordering::Relaxed);
    MAX_PAUSE_NS.fetch_max(taken.pause_ns, Ordering::Relaxed);
    if new {
        TAKEN.fetch_add(1, Ordering::Relaxed);
        klog::event!(Info, "snap", "taken", pool = t.pool_name.as_str(), id = id, auto = auto, label = label,
            preflush_us = taken.preflush_ns / 1000, freeze_us = taken.freeze_ns / 1000, snap_us = taken.snap_ns / 1000, pause_us = taken.pause_ns / 1000,
            commit_us = taken.commit_ns / 1000, barriers = taken.pause_barriers, barrier_us = taken.pause_barrier_ns / 1000);
    } else {
        UNCHANGED.fetch_add(1, Ordering::Relaxed);
        klog::event!(Info, "snap", "unchanged", pool = t.pool_name.as_str(), latest = id, pause_us = taken.pause_ns / 1000);
    }
    Ok(taken)
}

/// Freeze, the pool's snapshot in memory, thaw: (id, new, frozen at, snapshot made at).
fn frozen_part(t: &Target, freezing: &AtomicBool, label: &str, created_ns: u64, auto: bool, if_changed: bool) -> Result<(u64, bool, u64, u64), Errno> {
    freezing.store(true, Ordering::Release);
    let f = t.sb.freeze();
    freezing.store(false, Ordering::Release);
    if let Err(e) = f {
        klog::event!(Warn, "snap", "freeze_failed", pool = t.pool_name.as_str(), errno = e);
        return Err(e);
    }
    let _frozen = Frozen(&t.sb);
    let t1 = now();
    let r = (|| -> Result<(u64, bool), Errno> {
        if FAIL_FROZEN.swap(false, Ordering::Relaxed) {
            return Err(EIO);
        }
        let mut p = t.pool.lock().map_err(dev_errno)?;
        if if_changed
            && let Some(latest) = p.latest_snapshot(t.vol)
            && !p.changed_since(t.vol, latest).map_err(vol_errno)?
        {
            return Ok((latest, false));
        }
        p.snapshot_uncommitted(t.vol, label, created_ns, auto).map(|id| (id, true)).map_err(vol_errno)
    })();
    let t2 = now();
    let (id, new) = r.inspect_err(|&e| klog::event!(Warn, "snap", "take_failed", pool = t.pool_name.as_str(), errno = e))?;
    Ok((id, new, t1, t2))
}

/// A snapshot of the target's volume.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snap {
    pub id: u64,
    pub created_ns: u64,
    pub auto: bool,
    pub label: String,
    /// Bytes a delete would free (`list` with sizes).
    pub bytes: Option<u64>,
    pub mounted: bool,
    /// /data goes back to it at the next mount (VS-3).
    pub rollback: bool,
}

/// Tree pages one step of a size walk reads with the pool locked (the volume's I/O goes on in between).
const SIZE_STEP_PAGES: u64 = 64;

/// Bytes deleting snapshot (or volume) `id` would free, walked in steps (`Pool::exclusive_chunks_step`).
fn exclusive_bytes(t: &Target, id: u64) -> Result<u64, Errno> {
    let (mut chunks, mut from) = (0u64, 0u64);
    loop {
        let mut p = t.pool.lock().map_err(dev_errno)?;
        let chunk = p.chunk_size() as u64;
        let (n, next) = p.exclusive_chunks_step(id, from, SIZE_STEP_PAGES).map_err(vol_errno)?;
        drop(p);
        chunks = chunks.saturating_add(n);
        match next {
            Some(k) => from = k,
            None => return Ok(chunks.saturating_mul(chunk)),
        }
    }
}

/// The target's snapshots, oldest first. `sizes`: each one's own bytes (its tree walked in short steps, so
/// I/O on the volume goes on in between).
pub fn list(t: &Target, sizes: bool) -> Result<Vec<Snap>, Errno> {
    let mut out = {
        let p = t.pool.lock().map_err(dev_errno)?;
        let all = p.list().map_err(vol_errno)?;
        all.into_iter()
            .filter(|v| v.snapshot && v.origin == t.vol)
            .map(|v| Snap { id: v.id, created_ns: v.created_ns, auto: v.auto, label: v.label, bytes: None, mounted: false, rollback: v.rollback_pending })
            .collect::<Vec<Snap>>()
    };
    out.sort_by_key(|s| (s.created_ns, s.id));
    let mounted = mounts::mounted_ids(&t.pool_name)?;
    for s in &mut out {
        s.mounted = mounted.contains(&s.id);
        if sizes {
            // A snapshot deleted meanwhile has no size; it stays in this answer without one.
            s.bytes = match exclusive_bytes(t, s.id) {
                Ok(b) => Some(b),
                Err(ENOENT) => None,
                Err(e) => return Err(e),
            };
        }
    }
    Ok(out)
}

/// Bytes deleting snapshot `id` would free.
pub fn size_of(t: &Target, id: u64) -> Result<u64, Errno> {
    own_snapshot(&*t.pool.lock().map_err(dev_errno)?, t, id)?;
    exclusive_bytes(t, id)
}

fn own_snapshot(p: &bitsvol::Pool, t: &Target, id: u64) -> Result<bitsvol::VolumeInfo, Errno> {
    let v = p.info(id).map_err(vol_errno)?;
    if !v.snapshot || v.origin != t.vol {
        return Err(EINVAL);
    }
    Ok(v)
}

/// Deletes snapshot `id` of the target (never the volume itself: EINVAL; mounted: EBUSY).
pub fn delete(t: &Target, id: u64) -> Result<(), Errno> {
    let _ops = ops()?;
    let mut p = t.pool.lock().map_err(dev_errno)?;
    let v = own_snapshot(&p, t, id)?;
    p.delete(id).map_err(vol_errno).inspect_err(|&e| klog::event!(Warn, "snap", "delete_failed", id = id, errno = e))?;
    klog::event!(Info, "snap", "deleted", pool = t.pool_name.as_str(), id = id, auto = v.auto, created_ns = v.created_ns);
    Ok(())
}

/// Schedules the target's rollback to its snapshot `id` for the volume's next open (VS-3): at the next mount of
/// /data the volume's root switches to the snapshot and the state before is kept as the snapshot
/// "before-rollback" (so the rollback is undone the same way). Not at once: the mounted ext4 holds the current
/// tree in its caches and journal, and programs hold files open (LVM defers a merge into an origin in use the
/// same way, to its next activation). Returns the snapshot scheduled before (replaced).
pub fn schedule_rollback(t: &Target, id: u64) -> Result<Option<u64>, Errno> {
    let _ops = ops()?;
    let mut p = t.pool.lock().map_err(dev_errno)?;
    own_snapshot(&p, t, id)?;
    let old = p.schedule_rollback(t.vol, id).map_err(vol_errno).inspect_err(|&e| klog::event!(Warn, "snap", "rollback_schedule_failed", to = id, errno = e))?;
    klog::event!(Info, "snap", "rollback_scheduled", pool = t.pool_name.as_str(), to = id, replaced = old.unwrap_or(0); "carried out at the next mount of the volume");
    Ok(old)
}

/// Drops a scheduled rollback; returns the snapshot it named.
pub fn cancel_rollback(t: &Target) -> Result<Option<u64>, Errno> {
    let _ops = ops()?;
    let r = t.pool.lock().map_err(dev_errno)?.cancel_rollback(t.vol).map_err(vol_errno)?;
    klog::event!(Info, "snap", "rollback_cancelled", pool = t.pool_name.as_str(), to = r.unwrap_or(0));
    Ok(r)
}

/// The snapshot the target goes back to at its next mount, if any.
pub fn pending_rollback(t: &Target) -> Result<Option<u64>, Errno> {
    Ok(t.pool.lock().map_err(dev_errno)?.pending_rollback(t.vol))
}

/// The scheduled rollback this start's mount could not carry out (klog `volume.rollback_failed`), while it is
/// still the scheduled one: it waits for the next start, and a system restore's generation changed without it.
pub fn rollback_not_done(t: &Target) -> Result<Option<u64>, Errno> {
    let pending = pending_rollback(t)?;
    Ok(t.pool.rollback_failed().filter(|&s| pending == Some(s)))
}

fn set_clock() -> Result<(), &'static str> {
    bitsvol::set_clock(wall_ns);
    Ok(())
}
// Before any pool opens (the /data mount runs at Service level).
kreg::initcall!(kreg::Level::Subsys, "snap.clock", set_clock);

/// The pool's space and this boot's freeze pauses, as `key=value` words.
pub fn stats(t: &Target) -> Result<String, Errno> {
    let s = t.pool.lock().map_err(dev_errno)?.stats();
    let pct10 = if s.chunks == 0 { 0 } else { s.free.saturating_add(s.pending).saturating_mul(1000) / s.chunks };
    Ok(format!(
        "pool={} chunk_size={} chunks={} free={} pending={} reserve={} snapshots={} dropped={} free_permille={} wall_ns={} utc_offset_min={} taken={} unchanged={} last_pause_us={} max_pause_us={}",
        t.pool_name,
        s.chunk_size,
        s.chunks,
        s.free,
        s.pending,
        s.reserve,
        s.snapshots,
        s.dropped,
        pct10,
        wall_ns(),
        time::wall::utc_offset_minutes(),
        TAKEN.load(Ordering::Relaxed),
        UNCHANGED.load(Ordering::Relaxed),
        LAST_PAUSE_NS.load(Ordering::Relaxed) / 1000,
        MAX_PAUSE_NS.load(Ordering::Relaxed) / 1000,
    ))
}
