//! In-kernel tests of VS-1 (all class Safe: every test builds its own RAM device, nothing touches hardware,
//! so they are rerunnable on the Mini-PC too).
//! - `bitsvol.model`: 400 random steps against the reference model (64 KiB and 4 KiB chunks).
//! - `bitsvol.crash`: 80 random steps with a crash image at every device write, each opened and checked.
//! - `bitsvol.stack`: a volume as a stacked `BlockDevice` on a 16 MiB RAM device with the USB stick's request
//!   limit (64 blocks): writes across chunks, snapshot through `Snapshots`, flush, reopen, discard.
//! - `bitsvol.enospc`: a pool smaller than its volume; NoSpace through the stacked device, discard, write again.
//! - `bitsvol.rollback`: rollback to a snapshot and the undo of it.
//! - `bitsvol.superblock`: damaged newest superblock -> the older one; both damaged -> open fails.
//! - `bitsvol.commit_time` (VS-7): a flush after 320 KiB of writes flushes before the pool lock and reports its
//!   fields with the kernel's clock; an idle flush costs nothing. Bumps the global `bitsvol.commit.*` counters by 2.

use crate::memdev::MemDev;
use crate::model::{RunCfg, Runner, fill, stamp_of};
use crate::{Config, Pool, PoolHandle};
use alloc::format;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use devmodel::block::{BlockDevice, IoPriority::Normal, Snapshots};
use ktest::{TestResult, kassert, ktest};

fn seed() -> u64 {
    ksync::now_ns().unwrap_or(7) | 1
}

fn model() -> TestResult {
    let s = seed();
    let a = Runner::new(RunCfg::small(s, 400))?.run()?;
    let c = RunCfg { chunk_size: 65536, dev_chunks: 40, vol_chunks: 12, max_req: 64, cache_pages: 8, ..RunCfg::small(s + 1, 200) };
    let b = Runner::new(c)?.run()?;
    klog::event!(Info, "bitsvol", "ktest.model", seed = s, steps = a.steps + b.steps, nospace = a.nospace + b.nospace,
        dropped = a.dropped + b.dropped, reopens = a.reopens + b.reopens);
    Ok(())
}
ktest!("bitsvol.model", Safe, model);

fn crash() -> TestResult {
    let s = seed();
    let c = RunCfg { dev_chunks: 64, vol_chunks: 12, crash: true, ..RunCfg::small(s, 80) };
    let r = Runner::new(c)?.run()?;
    kassert!(r.crash_images > 0, "no crash image was taken");
    klog::event!(Info, "bitsvol", "ktest.crash", seed = s, images = r.crash_images, generations = r.generations);
    Ok(())
}
ktest!("bitsvol.crash", Safe, crash);

fn e<E: core::fmt::Debug>(x: E) -> alloc::string::String {
    format!("{x:?}")
}

fn stack() -> TestResult {
    let dev = Arc::new(MemDev::new("ram", 512, 32768, 64).ok_or("no memory for the RAM device")?);
    let pool = Pool::format(dev.clone(), Config::default()).map_err(e)?;
    let h = PoolHandle::new(pool, "ktest");
    let id = h.lock().map_err(e)?.create_volume("data", 8 << 20, 1).map_err(e)?;
    let vd = h.open_volume(id).map_err(e)?;
    kassert!(vd.info().blocks == 16384 && vd.info().max_blocks_per_request >= 64);
    let blk = |s: u64| {
        let mut b = vec![0u8; 512];
        fill(&mut b, s);
        b
    };
    let data: Vec<u8> = (0..300u64).flat_map(|i| blk(i + 1)).collect();
    vd.write_blocks(100, &data, false, Normal).map_err(e)?;
    vd.flush().map_err(e)?;
    let snap = vd.snapshot("ktest").map_err(e)?;
    vd.write_blocks(100, &blk(999), true, Normal).map_err(e)?;
    kassert!(vd.list().iter().any(|x| x.0 == snap));
    let mut b = vec![0u8; 512];
    vd.read_blocks(100, &mut b, Normal).map_err(e)?;
    kassert!(stamp_of(&b) == Some(999));
    drop(vd);
    drop(h);
    let mut p = Pool::open(dev.clone(), Config::default()).map_err(e)?;
    let mut back = vec![0u8; data.len()];
    p.read(snap, 100, &mut back, Normal).map_err(e)?;
    kassert!(back == data, "the snapshot changed");
    p.discard(id, 0, 16384).map_err(e)?;
    p.commit().map_err(e)?;
    let st = p.stats();
    klog::event!(Info, "bitsvol", "ktest.stack", free = st.free, chunks = st.chunks, generation = st.generation,
        device_writes = dev.writes(), device_flushes = dev.flushes());
    Ok(())
}
ktest!("bitsvol.stack", Safe, stack);

ktest!("bitsvol.enospc", Safe, crate::checks::enospc_then_discard);
ktest!("bitsvol.rollback", Safe, crate::checks::rollback_and_undo);
ktest!("bitsvol.superblock", Safe, crate::checks::damaged_superblock);

fn commit_time() -> TestResult {
    let dev = Arc::new(MemDev::new("vs7", 512, 4096, 64).ok_or("no memory")?);
    let mut p = Pool::format(dev.clone(), Config { chunk_size: 4096, quiet: true, ..Config::default() }).map_err(|e| format!("{e:?}"))?;
    let v = p.create_volume("a", 1 << 20, 1).map_err(|e| format!("{e:?}"))?;
    p.commit().map_err(|e| format!("{e:?}"))?;
    let h = PoolHandle::new(p, "ktest-vs7");
    let vd = h.open_volume(v).map_err(|e| format!("{e:?}"))?;
    let big = vec![0x5au8; 64 << 10];
    for i in 0..5u64 {
        vd.write_blocks(i * 128, &big, false, Normal).map_err(|e| format!("{e:?}"))?;
    }
    vd.flush().map_err(|e| format!("{e:?}"))?;
    let t = h.last_commit();
    kassert!(t.why == "flush" && t.pre_bytes >= crate::timing::PREFLUSH_BYTES, "flush before the lock: {t:?}");
    kassert!(t.ns > 0 && t.io.flushes >= 1 && t.io.bytes > 0 && !t.slow(), "fields: {t:?}");
    vd.flush().map_err(|e| format!("{e:?}"))?;
    let i = h.last_commit();
    kassert!(i.pre_bytes == 0 && i.io.flushes == 0 && i.io.covered == 0, "idle flush: {i:?}");
    klog::event!(Info, "bitsvol", "ktest.commit_time", ns = t.ns, pre_ns = t.pre_ns, pre_bytes = t.pre_bytes, flushes = t.io.flushes, bytes = t.io.bytes);
    Ok(())
}
ktest!("bitsvol.commit_time", Safe, commit_time);
