//! `snap.fsyncs` (QemuOnly: it writes /data; VS-2b): the freeze pause under a load like Brave's, several writers
//! each appending small records to its own log file with an fsync after every one (SQLite/LevelDB style), now and
//! then a file saved by rename-over. 30 automatic snapshots (`take(.., auto, if_changed)`, as bitssnap's rounds)
//! are taken meanwhile; the pauses, their parts, the device barriers inside them and the longest stall a writer
//! saw go out as one event `snap fsyncs.pauses` (timing is judged on the Mini-PC, not here). The snapshots and the
//! folder are removed again (rerunnable).

use super::{MS, RUN, SEC, data_mount, e};
use crate::{Taken, delete, take};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use ktest::{TestResult, kassert, ktest};
use vfs::MountNs;

const WRITERS: u64 = 4;
const TAKES: usize = 30;
const RECORD: usize = 4096;
/// Each log file wraps at this size (a WAL checkpointed now and then).
const LOG_BYTES: u64 = 1 << 20;

struct Load {
    stop: AtomicBool,
    steps: AtomicU64,
    errors: AtomicU64,
    /// Longest write+fsync step a writer saw (ns), and how many took over 50 ms.
    max_step_ns: AtomicU64,
    slow_steps: AtomicU64,
    /// Writers below this number end each fsync with `PoolHandle::settle`, as the fsync path does once the
    /// FS change request of VS-2b's fixer is in (exact durability during a take); the others as today.
    settlers: u64,
    pool: Arc<bitsvol::PoolHandle>,
}

fn writer(ns: Arc<MountNs>, dir: String, w: u64, l: Arc<Load>) {
    let c = vfs::ops::Ctx::kernel(&ns);
    let path = format!("{dir}/log{w}");
    let mut rec = [0u8; RECORD];
    let mut k: u64 = 0;
    while !l.stop.load(Ordering::Relaxed) {
        let t0 = time::now_ns();
        let r = (|| -> Result<(), i64> {
            let f = c.open(None, path.as_bytes(), vfs::abi::O_RDWR | vfs::abi::O_CREAT, 0o644)?;
            rec.fill(k as u8);
            f.pwrite((k * RECORD as u64) % LOG_BYTES, &rec)?;
            drop(f);
            ns.lookup(&path)?.inode.sync(vfs::pagecache::SyncKind::Fsync)?;
            if w < l.settlers {
                l.pool.settle().map_err(crate::dev_errno)?;
            }
            if k % 50 == 49 {
                let tmp = format!("{dir}/manifest{w}.tmp");
                let f = c.open(None, tmp.as_bytes(), vfs::abi::O_RDWR | vfs::abi::O_CREAT | vfs::abi::O_TRUNC, 0o644)?;
                f.pwrite(0, &rec[..512])?;
                drop(f);
                ns.lookup(&tmp)?.inode.sync(vfs::pagecache::SyncKind::Fsync)?;
                c.rename(None, tmp.as_bytes(), None, format!("{dir}/manifest{w}").as_bytes(), 0)?;
            }
            Ok(())
        })();
        let step = time::now_ns().saturating_sub(t0);
        l.max_step_ns.fetch_max(step, Ordering::Relaxed);
        if step > 50 * MS {
            l.slow_steps.fetch_add(1, Ordering::Relaxed);
        }
        if r.is_err() {
            l.errors.fetch_add(1, Ordering::Relaxed);
        }
        l.steps.fetch_add(1, Ordering::Relaxed);
        k += 1;
        sched::sleep_ns(5 * MS);
    }
}

/// (min, median, p95, max) in µs.
fn spread(mut v: Vec<u64>) -> (u64, u64, u64, u64) {
    if v.is_empty() {
        return (0, 0, 0, 0);
    }
    v.sort_unstable();
    let pick = |q: usize| v[(v.len() - 1) * q / 100] / 1000;
    (pick(0), pick(50), pick(95), pick(100))
}

fn fsyncs() -> TestResult {
    let t = match data_mount() {
        Ok(t) => t,
        Err(r) => return r,
    };
    let ns = vfs::root_ns();
    let dir = format!("/data/vs-fsyncs-{}", RUN.fetch_add(1, Ordering::Relaxed));
    ns.mkdir_p(&dir).map_err(e)?;
    let l = Arc::new(Load { stop: AtomicBool::new(false), steps: AtomicU64::new(0), errors: AtomicU64::new(0), max_step_ns: AtomicU64::new(0), slow_steps: AtomicU64::new(0), settlers: WRITERS / 2, pool: t.pool.clone() });
    let mut hs = Vec::new();
    for w in 0..WRITERS {
        let (n2, d2, l2) = (ns.clone(), dir.clone(), l.clone());
        match sched::spawn("snap-fsync-writer", sched::Class::Normal, move || writer(n2, d2, w, l2)) {
            Ok(h) => hs.push(h),
            Err(x) => {
                l.stop.store(true, Ordering::Relaxed);
                return Err(format!("spawn: {x:?}"));
            }
        }
    }
    // The writers get going (files exist, the log is busy) before the first take.
    sched::sleep_ns(500 * MS);
    let mut taken: Vec<Taken> = Vec::new();
    let r = (|| -> TestResult {
        for i in 0..TAKES {
            taken.push(take(&t, "vsfsync", true, true).map_err(|x| format!("take {i}: errno {x}"))?);
            sched::sleep_ns(300 * MS);
        }
        Ok(())
    })();
    l.stop.store(true, Ordering::Relaxed);
    let mut ended = true;
    for h in hs {
        ended &= h.join_timeout(30 * SEC);
    }
    let steps_before_check = l.steps.load(Ordering::Relaxed);
    for k in taken.iter().filter(|k| k.new) {
        let _ = delete(&t, k.id);
    }
    let _ = ns.remove_tree(&dir);
    let _ = vfs::writeback::sync_durable();
    r?;
    kassert!(ended, "a writer did not stop");
    kassert!(l.errors.load(Ordering::Relaxed) == 0, "{} writer errors", l.errors.load(Ordering::Relaxed));
    let new = taken.iter().filter(|k| k.new).count();
    kassert!(new * 2 >= TAKES, "only {new} of {TAKES} takes found a change under a steady write load");
    let col = |f: fn(&Taken) -> u64| spread(taken.iter().map(f).collect());
    let (p0, p50, p95, p100) = col(|k| k.pause_ns);
    let (_, f50, _, f100) = col(|k| k.freeze_ns);
    let (_, s50, _, s100) = col(|k| k.snap_ns);
    let (_, pre50, _, pre100) = col(|k| k.preflush_ns);
    let (_, c50, _, c100) = col(|k| k.commit_ns);
    let (_, _, _, b100) = col(|k| k.pause_barriers * 1000);
    let (_, bt50, _, bt100) = col(|k| k.pause_barrier_ns);
    klog::event!(Info, "snap", "fsyncs.pauses", snapshots = new, takes = TAKES, steps = steps_before_check,
        min_us = p0, median_us = p50, p95_us = p95, max_us = p100, freeze_median_us = f50, freeze_max_us = f100,
        snap_median_us = s50, snap_max_us = s100, preflush_median_us = pre50, preflush_max_us = pre100,
        commit_median_us = c50, commit_max_us = c100, barriers_max = b100, barrier_median_us = bt50, barrier_max_us = bt100,
        writer_max_stall_us = l.max_step_ns.load(Ordering::Relaxed) / 1000, writer_slow_steps = l.slow_steps.load(Ordering::Relaxed), deferred_flushes = t.pool.deferred_flushes());
    // Not timing (judged on the Mini-PC) but counts: no device barrier inside any pause (VS-2b's fix; the
    // version that deferred only after the drain had 4 in 12 of 30), and the last take's commit covered every
    // deferred flush.
    let inside: u64 = taken.iter().map(|k| k.pause_barriers).sum();
    kassert!(inside == 0, "{inside} device barriers inside the pauses");
    kassert!(t.pool.owed_flushes() == 0, "{} deferred flushes never committed", t.pool.owed_flushes());
    Ok(())
}
ktest!("snap.fsyncs", QemuOnly, fsyncs);
