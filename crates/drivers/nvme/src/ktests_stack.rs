//! BL-2 in-kernel test (class Safe, rerunnable: its own RAM disk, never registered, nothing of the machine touched):
//! - `block.stack.readers`: the three layers of /data on the Mini-PC, on RAM: a disk with a GPT and 32 slots, its
//!   partition over the disk's `PartPort` (the window armed on exactly that partition, as `nvme_data=` does), a
//!   bitsvol pool and volume on the partition, the volume as a stacked disk. 32 readers read the whole volume at
//!   once through a cold page cache of the volume; every byte is checked, and no layer copied: the three disks'
//!   `copied_bytes` stay as they were, and the RAM disk is read through `read_frames` (straight into the pages).
//!   Lives in the nvme crate, the only one of lane BL that has the block layer and bitsvol both.
//! - `nv.frames` (RunHw("nv/10-nvme")): below.

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use bitsvol::{Config, Pool, PoolHandle};
use block::bdev::DiskFile;
use block::{Disk, DiskDevice, DiskKind, Dispatch, RamDisk};
use core::sync::atomic::{AtomicU64, Ordering};
use devmodel::block::IoPriority;
use ktest::{TestResult, kassert, ktest};

const MIB: usize = 1 << 20;
const DISK: usize = 40 * MIB;
const VOL: usize = 16 * MIB;
const READERS: usize = 32;
const PIECE: usize = 64 * 1024;
const MS: u64 = 1_000_000;

fn e<T: core::fmt::Debug>(x: T) -> String {
    format!("{x:?}")
}

/// The volume's bytes: different in every block and page.
fn byte(off: usize) -> u8 {
    ((off ^ (off >> 9) ^ (off >> 17)) as u8).wrapping_mul(29) ^ 0x5A
}

fn stack_readers() -> TestResult {
    let blocks = (DISK / 512) as u64;
    let img = block::testimg::gpt(512, blocks, &[(block::table::GUID_LINUX_DATA, 2048, blocks - 2048)]);
    let ram = Arc::new(RamDisk::from_image("bl2stack", 512, img, false).with_max_blocks(256).with_queue_depth(READERS as u32));
    let bottom = Disk::open(ram.clone(), DiskKind::Ram, Dispatch::Inline).map_err(e)?;
    let (first, n) = bottom.scan().parts.first().map(|p| (p.0, p.1)).ok_or("the test disk has no partition")?;
    bottom.arm_window(first, n, None);
    let res = (|| -> TestResult {
        let part = block::gpt::Partition::new(Arc::new(bottom.part_port(first, n)), "bl2stack", first, n, 1, "data").ok_or("partition")?;
        let pd = Disk::open(Arc::new(part), DiskKind::Stacked, Dispatch::Inline).map_err(e)?;
        let pool = Pool::format(Arc::new(DiskDevice(pd.clone())), Config { quiet: true, ..Config::default() }).map_err(e)?;
        let h = PoolHandle::new(pool, "bl2stack");
        let id = h.lock().map_err(e)?.create_volume("data", VOL as u64, 1).map_err(e)?;
        let vd = Disk::open(Arc::new(h.open_volume(id).map_err(e)?), DiskKind::Stacked, Dispatch::Inline).map_err(e)?;
        // Fill through the stack, commit: the mapping's pages stay in the pool's cache (the reads below copy nothing
        // there either; a cold mapping is BL-3's measurement on the Mini-PC).
        let mut chunk = vec![0u8; MIB];
        for m in 0..VOL / MIB {
            for (i, b) in chunk.iter_mut().enumerate() {
                *b = byte(m * MIB + i);
            }
            vd.write((m * MIB / 512) as u64, &chunk, false, IoPriority::Normal).map_err(e)?;
        }
        vd.flush(IoPriority::Normal).map_err(e)?;
        h.commit().map_err(e)?;
        let base = [bottom.stats().copied_bytes, pd.stats().copied_bytes, vd.stats().copied_bytes];
        let frames0 = ram.frame_calls();
        let pc = Arc::new(DiskFile::page_cache(vd.clone()));
        let bad = Arc::new(AtomicU64::new(0));
        let failed = Arc::new(AtomicU64::new(0));
        let mut threads = Vec::new();
        for t in 0..READERS {
            let (pc, bad, failed) = (pc.clone(), bad.clone(), failed.clone());
            let th = sched::spawn("bl2-reader", sched::Class::Normal, move || {
                let mut buf = vec![0u8; PIECE];
                // Interleaved: neighbours read neighbouring pieces at the same time (merges, many in flight).
                let mut at = t * PIECE;
                while at < VOL {
                    match pc.read_at(at as u64, &mut buf) {
                        Ok(got) if got == PIECE => {
                            let wrong = buf.iter().enumerate().filter(|&(i, &b)| b != byte(at + i)).count();
                            bad.fetch_add(wrong as u64, Ordering::Relaxed);
                        }
                        _ => {
                            failed.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    at += READERS * PIECE;
                }
            })
            .map_err(e)?;
            threads.push(th);
        }
        for th in &threads {
            kassert!(th.join_timeout(60_000 * MS), "a reader did not finish in 60 s");
        }
        let after = [bottom.stats().copied_bytes, pd.stats().copied_bytes, vd.stats().copied_bytes];
        let st = bottom.stats();
        klog::event!(Info, "block", "ktest.stack_readers", readers = READERS, bytes = VOL, frame_calls = ram.frame_calls() - frames0,
            inflight_max = st.inflight_max, merged = st.merged, dispatches = st.dispatches);
        kassert!(failed.load(Ordering::Relaxed) == 0, "{} reads failed", failed.load(Ordering::Relaxed));
        kassert!(bad.load(Ordering::Relaxed) == 0, "{} bytes differ", bad.load(Ordering::Relaxed));
        // The disks' own count is the isolated form of block.copy_bytes.{readvec_zero,disk_copy,bounce}: every site
        // that bumps one of those global (per-CPU batched) counters bumps the disk's `copied` beside it
        // (disk.rs read_vec/read, dispatch.rs move_data), and the globals move with other disks' I/O as well
        // (the Mini-PC's /data, a stick). That several reads are in the device at once is proven on the host
        // (frames_tests `two_readers_are_in_the_device_at_once`); here `inflight_max` is logged, it depends on CPUs.
        kassert!(after == base, "a layer copied: bottom/partition/volume copied_bytes {base:?} -> {after:?}");
        kassert!(ram.frame_calls() > frames0, "the RAM disk was never read through read_frames");
        drop(pc);
        vd.stop();
        pd.stop();
        Ok(())
    })();
    bottom.disarm_window();
    bottom.stop();
    res
}
ktest!("block.stack.readers", Safe, stack_readers);

// ---- nv.frames

/// - `nv.frames` (RunHw("nv/10-nvme"): every NVMe namespace QEMU or the machine has; only reads): page-cache pages
///   read through the namespace's disk land by DMA straight in the pages (`nvme.direct_bytes` grows by at least
///   their bytes), byte for byte what a byte read gives; also with a first segment that starts inside its page
///   (PRP1 with an offset) and a last one that ends inside its page.
fn frames() -> TestResult {
    use block::PageSeg;
    use vfs::pagecache::{Backend, CachePage, PageCache};
    struct Nothing;
    impl Backend for Nothing {
        fn read_page(&self, _: u64, _: &Arc<CachePage>) -> Result<(), i64> {
            Ok(())
        }
        fn write_run(&self, _: u64, _: &[Arc<CachePage>], _: u64) -> Result<(), i64> {
            Ok(())
        }
    }
    if !crate::settle(30_000 * MS) {
        return ktest::fail("a controller's bring-up did not finish in 30 s");
    }
    let mut disks = Vec::new();
    for c in crate::controllers() {
        for ns in c.info().namespaces.iter().filter(|n| n.unusable.is_none()) {
            if let Some(d) = block::find(&format!("{}n{}", c.name, ns.nsid)) {
                disks.push(d);
            }
        }
    }
    kassert!(!disks.is_empty(), "no NVMe namespace is registered as a disk");
    const PAGES: u64 = 64;
    let cache = PageCache::with_backend(Arc::new(Nothing), (PAGES + 1) * 4096);
    let pages: Vec<Arc<CachePage>> = (0..=PAGES).map(|i| cache.page_for_map(i, false).map_err(e)).collect::<Result<_, _>>()?;
    let mut checked = 0u64;
    for d in &disks {
        let bs = d.block_size() as u64;
        let blocks = d.dev_info().blocks;
        let per = PAGES * 4096 / bs;
        if blocks < 4 * per {
            continue;
        }
        let direct0 = crate::ctrl::NVME_DIRECT.sum();
        let mut ours = 0u64;
        for (k, lba) in [0, blocks / 3, 2 * blocks / 3, blocks - per - 1].into_iter().enumerate() {
            // k odd: the first segment starts 512 bytes into its page, the last ends 512 bytes into one more page.
            let segs: Vec<PageSeg> = if k % 2 == 1 {
                let mut v = vec![PageSeg { page: pages[0].clone(), off: 512, len: 4096 - 512 }];
                v.extend((1..PAGES as usize).map(|i| PageSeg { page: pages[i].clone(), off: 0, len: 4096 }));
                v.push(PageSeg { page: pages[PAGES as usize].clone(), off: 0, len: 512 });
                v
            } else {
                (0..PAGES as usize).map(|i| PageSeg { page: pages[i].clone(), off: 0, len: 4096 }).collect()
            };
            let len: usize = segs.iter().map(|s| s.len).sum();
            d.read_pages(lba, segs.clone(), IoPriority::Normal).map_err(|x| format!("{}: frames at {lba}: {x:?}", d.name()))?;
            let want = d.read_vec(lba, len, IoPriority::Normal).map_err(|x| format!("{}: bytes at {lba}: {x:?}", d.name()))?;
            let mut got = vec![0u8; len];
            let mut at = 0;
            for s in &segs {
                s.page.read(s.off, &mut got[at..at + s.len]);
                at += s.len;
            }
            kassert!(got == want, "{}: the frames at {lba} differ from a byte read", d.name());
            ours += len as u64;
        }
        let direct = crate::ctrl::NVME_DIRECT.sum() - direct0;
        kassert!(direct >= ours, "{}: only {direct} of {ours} bytes went straight into the pages", d.name());
        checked += 1;
    }
    kassert!(checked > 0, "no namespace was large enough ({} disks)", disks.len());
    klog::event!(Info, "nvme", "ktest.frames", disks = checked);
    Ok(())
}
ktest!("nv.frames", RunHw("nv/10-nvme"), frames);
