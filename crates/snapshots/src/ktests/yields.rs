//! `snap.delete_yields` (Safe, RAM; VS-H): a foreground write during a burst of snapshot deletes on a slow disk.
//! The measure is a count, so a loaded host cannot turn it red: how many whole deletes ended while one write
//! waited. A delete that yields lets the write in at its next step; one that goes straight on makes the write wait
//! for the whole burst (red log of vs/30-snapshots: 33 s of deletes, the write's 30 s bound ran out, /data read-only).
//! The device's flushes really sleep, as a stick's do.

use super::{MS, e};
use crate::{Target, delete};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use bitsvol::memdev::MemDev;
use bitsvol::{Config, Pool, PoolHandle};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use devmodel::DevError;
use devmodel::block::{BlockDevice, BlockInfo, IoPriority};
use ktest::{TestResult, kassert, ktest};
use vfs::MountNs;

const BURST: u64 = 48;
/// A write waits for the step under way and, on a wake-up that came late, for one or two more; the burst is 48.
const MOST_BEHIND: u64 = 4;
const FLUSH_NS: u64 = 2 * MS;
const FLUSHES: u64 = 32;

struct SlowFlush(Arc<MemDev>);
/// `flush_leaves_io`'s device: its flush count, odd during a flush.
static FLUSHING: AtomicU64 = AtomicU64::new(0);

impl BlockDevice for SlowFlush {
    fn name(&self) -> &str {
        "vsh-slow"
    }
    fn info(&self) -> BlockInfo {
        self.0.info()
    }
    fn read_blocks(&self, lba: u64, buf: &mut [u8], prio: IoPriority) -> Result<(), DevError> {
        self.0.read_blocks(lba, buf, prio)
    }
    fn write_blocks(&self, lba: u64, buf: &[u8], fua: bool, prio: IoPriority) -> Result<(), DevError> {
        if fua {
            sched::sleep_ns(FLUSH_NS);
        }
        self.0.write_blocks(lba, buf, fua, prio)
    }
    fn flush(&self) -> Result<(), DevError> {
        FLUSHING.fetch_add(1, Ordering::AcqRel);
        sched::sleep_ns(FLUSH_NS);
        let r = self.0.flush();
        FLUSHING.fetch_add(1, Ordering::AcqRel);
        r
    }
    fn discard(&self, lba: u64, blocks: u64) -> Result<(), DevError> {
        self.0.discard(lba, blocks)
    }
}

struct Fg {
    deleted: AtomicU64,
    done: AtomicBool,
    writes: AtomicU64,
    errors: AtomicU64,
    /// Most deletes that ended while one foreground write waited.
    behind: AtomicU64,
}

fn delete_yields() -> TestResult {
    let mem = Arc::new(MemDev::new("vsh", 512, 16384, 64).ok_or("no memory")?);
    let mut p = Pool::format(Arc::new(SlowFlush(mem)), Config { chunk_size: 4096, quiet: true, ..Config::default() }).map_err(e)?;
    let v = p.create_volume("a", 1 << 20, 1).map_err(e)?;
    let block = vec![0x5au8; 512];
    let mut ids: Vec<u64> = Vec::new();
    for i in 0..BURST {
        // Each snapshot holds a chunk and a leaf of its own, so its delete has work beyond its record.
        p.write(v, i * 8, &block, false, IoPriority::Normal).map_err(e)?;
        ids.push(p.snapshot(v, &format!("s{i}"), 0, false).map_err(e)?);
    }
    p.commit().map_err(e)?;
    let h = PoolHandle::new(p, "ktest-vsh");
    let vd = h.open_volume(v).map_err(e)?;
    let fg = Arc::new(Fg { deleted: AtomicU64::new(0), done: AtomicBool::new(false), writes: AtomicU64::new(0), errors: AtomicU64::new(0), behind: AtomicU64::new(0) });
    let f2 = fg.clone();
    // As the disk's dispatcher, which carries a write-back of /data.
    let w = sched::spawn("snap-vsh-write", sched::Class::Interactive, move || {
        let block = vec![0xa5u8; 512];
        let mut k = 0u64;
        while !f2.done.load(Ordering::Acquire) {
            let before = f2.deleted.load(Ordering::Acquire);
            if vd.write_blocks(k % 2048, &block, false, IoPriority::Normal).is_err() {
                f2.errors.fetch_add(1, Ordering::Relaxed);
            }
            f2.behind.fetch_max(f2.deleted.load(Ordering::Acquire) - before, Ordering::Relaxed);
            f2.writes.fetch_add(1, Ordering::Relaxed);
            k += 8;
            sched::sleep_ns(MS / 2);
        }
    })
    .map_err(|x| format!("spawn: {x:?}"))?;
    let mut failed: Option<String> = None;
    for &id in &ids {
        if let Err(x) = burst_delete(&h, id) {
            failed = Some(format!("delete {id}: {x:?}"));
            break;
        }
        fg.deleted.fetch_add(1, Ordering::AcqRel);
    }
    fg.done.store(true, Ordering::Release);
    let ended = w.join_timeout(30_000 * MS);
    if let Some(m) = failed {
        return Err(m);
    }
    let (writes, behind, errors) = (fg.writes.load(Ordering::Relaxed), fg.behind.load(Ordering::Relaxed), fg.errors.load(Ordering::Relaxed));
    klog::event!(Info, "snap", "delete_yields", deletes = BURST, writes = writes, most_behind = behind, errors = errors);
    kassert!(ended, "the writer did not stop");
    kassert!(errors == 0, "{errors} foreground writes failed");
    kassert!(writes > 0, "no foreground write got through");
    kassert!(behind <= MOST_BEHIND, "one foreground write waited for {behind} of {BURST} whole deletes");
    Ok(())
}
ktest!("snap.delete_yields", Safe, delete_yields);

/// An fsync of data rewritten in place owes no metadata, so its device flush leaves the pool free: reads run inside
/// a flush. With the flush under the pool lock none ever does (VS-H: under fsync load every request of a
/// snapshot's write-back waited one flush). A count of what ran between two steps of the device, no clock judged.
fn flush_leaves_io() -> TestResult {
    let mem = Arc::new(MemDev::new("vsh", 512, 16384, 64).ok_or("no memory")?);
    let mut p = Pool::format(Arc::new(SlowFlush(mem)), Config { chunk_size: 4096, quiet: true, ..Config::default() }).map_err(e)?;
    let v = p.create_volume("a", 1 << 20, 1).map_err(e)?;
    let block = vec![0x5au8; 512];
    for i in 0..8 {
        p.write(v, i * 8, &block, false, IoPriority::Normal).map_err(e)?;
    }
    p.commit().map_err(e)?;
    let h = PoolHandle::new(p, "ktest-vsh-flush");
    let vd = Arc::new(h.open_volume(v).map_err(e)?);
    let (done, v2) = (Arc::new(AtomicBool::new(false)), vd.clone());
    let d2 = done.clone();
    let f = sched::spawn("snap-vsh-flush", sched::Class::Normal, move || {
        let block = vec![0xa5u8; 512];
        for i in 0..FLUSHES {
            let _ = v2.write_blocks(i % 8 * 8, &block, false, IoPriority::Normal);
            let _ = v2.flush();
        }
        d2.store(true, Ordering::Release);
    })
    .map_err(|x| format!("spawn: {x:?}"))?;
    let (mut inside, mut reads) = (0u64, 0u64);
    let mut b = vec![0u8; 512];
    while !done.load(Ordering::Acquire) {
        let before = FLUSHING.load(Ordering::Acquire);
        vd.read_blocks(reads % 64, &mut b, IoPriority::Normal).map_err(e)?;
        reads += 1;
        if before % 2 == 1 && FLUSHING.load(Ordering::Acquire) == before {
            inside += 1;
        }
        sched::sleep_ns(MS / 4);
    }
    let ended = f.join_timeout(30_000 * MS);
    klog::event!(Info, "snap", "flush_leaves_io", flushes = FLUSHES, reads = reads, inside = inside);
    kassert!(ended, "the flusher did not stop");
    kassert!(inside > 0, "none of {reads} reads ran inside any of {FLUSHES} device flushes");
    Ok(())
}
ktest!("snap.flush_leaves_io", Safe, flush_leaves_io);

fn burst_delete(h: &PoolHandle, id: u64) -> Result<(), bitsvol::VolError> {
    h.delete(id)
}

/// How long `delete_under_writes` waits for its burst before it fails: only a real stop, as on a stick under load.
const BURST_BOUND_NS: u64 = 600_000 * MS;
/// How many snapshots `delete_writable` deletes; each owns a file's worth of chunks.
const WRITABLE_BURST: u64 = 192;

/// `snap.load`'s deletes without its 100 takes on /data: ext4 on a private pool in RAM, an fsync writer through a burst
/// of deletes. Red when a write fails or the volume went read-only after it. No count of deletes behind: the RAM device
/// never sleeps, so that count is the writer's share of the CPU (see plan/notes/vs-VS-H.md); `delete_yields` has a
/// sleeping flush and `load` a real one for that.
fn delete_writable() -> TestResult {
    super::with_private_mib(128, |ns, t| {
        let mut ids = Vec::new();
        for i in 0..WRITABLE_BURST {
            ns.create_file_bytes(format!("/d/own{i}").as_bytes(), 0o644, &vec![i as u8; 16 << 10]).map_err(e)?;
            ids.push(crate::take(t, "kt-del", false, false).map_err(e)?.id);
        }
        // The file exists: the writes are what the burst meets, not the cost of a first create.
        ns.create_file_bytes(b"/d/during-deletes", 0o644, &[0u8; 4096]).map_err(e)?;
        delete_under_writes(t, ns, "/d", &ids, None)?;
        ns.create_file_bytes(b"/d/after-deletes", 0o644, b"still writable").map_err(|x| format!("/d after the deletes: errno {x}"))?;
        ns.lookup("/d/after-deletes").map_err(e)?.inode.sync(vfs::pagecache::SyncKind::Fsync).map_err(|x| format!("fsync after the deletes: errno {x}"))?;
        Ok(())
    })
}
ktest!("snap.delete_writable", Safe, delete_writable);

/// The deletes of `snap.load` under a real /data write: pwrite and fsync of one file through ext4, about a dozen
/// holds of the pool lock per step. Fails with an errno the writer saw (read-only, EIO) and, with `most_part`, when one
/// write waited for more than that part (1 in n) of the burst.
pub(super) fn delete_under_writes(t: &Target, ns: &Arc<MountNs>, dir: &str, ids: &[u64], most_part: Option<u64>) -> TestResult {
    let fg = Arc::new(Fg { deleted: AtomicU64::new(0), done: AtomicBool::new(false), writes: AtomicU64::new(0), errors: AtomicU64::new(0), behind: AtomicU64::new(0) });
    let (f2, n2, path) = (fg.clone(), ns.clone(), format!("{dir}/during-deletes"));
    let begun = Arc::new(AtomicBool::new(false));
    let b2 = begun.clone();
    let w = sched::spawn("snap-vsh-fsync", sched::Class::Normal, move || {
        let c = vfs::ops::Ctx::kernel(&n2);
        let rec = [0x5au8; 4096];
        let mut k = 0u64;
        loop {
            let before = f2.deleted.load(Ordering::Acquire);
            b2.store(true, Ordering::Release);
            let r = (|| -> Result<(), i64> {
                let f = c.open(None, path.as_bytes(), vfs::abi::O_RDWR | vfs::abi::O_CREAT, 0o644)?;
                f.pwrite(k % 64 * 4096, &rec)?;
                drop(f);
                n2.lookup(&path)?.inode.sync(vfs::pagecache::SyncKind::Fsync)
            })();
            if r.is_err() {
                f2.errors.fetch_add(1, Ordering::Relaxed);
            }
            f2.behind.fetch_max(f2.deleted.load(Ordering::Acquire) - before, Ordering::Relaxed);
            f2.writes.fetch_add(1, Ordering::Relaxed);
            k += 1;
            if f2.done.load(Ordering::Acquire) {
                break;
            }
            sched::sleep_ns(MS);
        }
    })
    .map_err(|x| format!("spawn: {x:?}"))?;
    // The first write is under way before the burst begins, or a burst on a fast device could end before it starts.
    let mut waited = 0;
    while !begun.load(Ordering::Acquire) && waited < 30_000 {
        sched::sleep_ns(MS);
        waited += 1;
    }
    // As bitssnap's retention, a Normal thread: the test's own thread would keep one CPU from the writer on a fast device.
    let failed = Arc::new(ksync::SpinLock::named("snap.vsh_failed", None));
    let (f3, t3, l3, list) = (fg.clone(), t.clone(), failed.clone(), ids.to_vec());
    let burst = sched::spawn("snap-vsh-delete", sched::Class::Normal, move || {
        for id in list {
            if let Err(x) = delete(&t3, id) {
                *l3.lock() = Some(format!("delete {id}: errno {x}"));
                break;
            }
            f3.deleted.fetch_add(1, Ordering::AcqRel);
        }
    })
    .map_err(|x| format!("spawn: {x:?}"));
    let deleted_all = match &burst {
        Ok(b) => b.join_timeout(BURST_BOUND_NS),
        Err(_) => false,
    };
    fg.done.store(true, Ordering::Release);
    let ended = w.join_timeout(30_000 * MS);
    burst?;
    kassert!(begun.load(Ordering::Acquire), "the writer never began");
    kassert!(deleted_all, "the burst of {} deletes did not end", ids.len());
    if let Some(m) = failed.lock().take() {
        return Err(m);
    }
    let (writes, behind, errors) = (fg.writes.load(Ordering::Relaxed), fg.behind.load(Ordering::Relaxed), fg.errors.load(Ordering::Relaxed));
    klog::event!(Info, "snap", "delete_under_writes", deletes = ids.len() as u64, writes = writes, most_behind = behind, errors = errors);
    kassert!(ended, "the writer did not stop");
    kassert!(errors == 0, "{errors} /data writes failed during the deletes");
    if let Some(part) = most_part {
        kassert!(behind * part <= ids.len() as u64, "one /data write and fsync waited for {behind} of {} whole deletes", ids.len());
    }
    Ok(())
}
