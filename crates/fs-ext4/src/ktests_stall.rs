//! FS-STALL in the kernel: ext4 on a `block::RamDisk` through the block layer and the disk's page cache
//! (`CachedDiskDev`), the path /data takes on the Mini-PC.
//!
//! - `fs.ext4.sync_stall`: (1) load: three threads create, write, fsync, rename and unlink small files on a slowed
//!   disk while a fourth stats and reads cached ones; the reader's worst call and the lock numbers are logged
//!   (`ktest.sync_stall`), judged generously. (2) order: the disk is paused while an fsync waits in it; a stat, a
//!   cached read, a listing, a small write and a new file must all finish before the disk goes on (it resumes by
//!   itself after 10 s, so before FS-STALL they finished only then), and the fsync must not finish before. Safe:
//!   memory only.

use crate::mount;
use crate::Options;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use ktest::{kassert, ktest, TestResult};
use vfs::abi::{O_CREAT, O_RDWR};
use vfs::ops::Ctx;
use vfs::pagecache::SyncKind;

fn e<T: core::fmt::Debug>(x: T) -> String {
    format!("{x:?}")
}

fn now() -> u64 {
    ksync::now_ns().unwrap_or(0)
}

fn hold_max_ns(class: &str) -> u64 {
    ksync::stats::class(class).and_then(|c| ksync::stats::ticks_to_ns(c.hold_max_ticks)).unwrap_or(0)
}

/// Runs `f` on a thread; true when it ended within `limit_ns`.
fn within(limit_ns: u64, f: impl FnOnce() + Send + 'static) -> Result<bool, String> {
    let h = sched::spawn("fs-ktest-stallop", sched::Class::Normal, f).map_err(e)?;
    Ok(h.join_timeout(limit_ns))
}

fn sync_stall() -> TestResult {
    let name = "ext4stall";
    block::unregister(name);
    let img = {
        // 16 MiB holds the few hundred small files; the image is copied whole three times, and after the pressure
        // tests a 64 MiB contiguous heap block may not be there.
        let dev = crate::MemDev::new(vec![0u8; 16 << 20]);
        let mut o = crate::mkfs::MkfsOptions::new(b"stall", [5; 16], [6; 16], vfs::inode::now());
        o.zeroed = true;
        crate::mkfs::format(&dev, &o).map_err(e)?;
        dev.image()
    };
    let rd = Arc::new(block::RamDisk::from_image(name, 512, img.clone(), false));
    let disk = block::register(rd.clone(), block::DiskKind::Ram).map_err(e)?;
    let r = (|| -> TestResult {
        let ns = Arc::new(vfs::MountNs::new_empty());
        let (v, _m) = mount::mount_disk(disk.clone(), 0, img.len() as u64 / 512, &ns, "/x", Options::default()).map_err(e)?;
        let c = Ctx::kernel(&ns);
        c.mkdir(None, b"/x/s", 0o755).map_err(e)?;
        for i in 0..40 {
            ns.create_file_bytes(format!("/x/s/f{i}").as_bytes(), 0o644, &vec![i as u8; 2000]).map_err(e)?;
        }
        for t in 0..3 {
            c.mkdir(None, format!("/x/w{t}").as_bytes(), 0o755).map_err(e)?;
        }
        v.sync_fs().map_err(e)?;
        load(&rd, &ns)?;
        order(&rd, &ns)?;
        v.sync_fs().map_err(e)?;
        let (_d, ns2, _v) = {
            let dev = Arc::new(crate::MemDev::new(rd.snapshot()));
            let ns2 = vfs::MountNs::new_empty();
            let (v2, _) = mount::mount_dev(dev.clone(), None, &ns2, "/x", Options { writable: false, ..Options::default() }).map_err(e)?;
            (dev, ns2, v2)
        };
        kassert!(ns2.read_file("/x/s/f3", 4096).map_err(e)?.starts_with(b"small"), "the small write is not on the disk");
        kassert!(ns2.read_file("/x/fsynced", 100).map_err(e)? == b"fsynced while the disk was paused", "the paused fsync's data");
        ns2.lookup("/x/new-during-the-pause").map_err(e)?;
        Ok(())
    })();
    rd.resume();
    rd.set_delay_ns(0);
    block::unregister(name);
    r
}

/// Writers with fsync on a slowed disk, one reader of cached files; the reader's worst call is judged generously.
fn load(rd: &Arc<block::RamDisk>, ns: &Arc<vfs::MountNs>) -> TestResult {
    rd.set_delay_ns(300_000);
    let stop = Arc::new(AtomicBool::new(false));
    let worst = Arc::new(AtomicU64::new(0));
    let calls = Arc::new(AtomicU64::new(0));
    let failed = Arc::new(AtomicU64::new(0));
    let t0 = now();
    let mut writers = Vec::new();
    for t in 0..3u32 {
        let (ns, failed) = (ns.clone(), failed.clone());
        writers.push(
            sched::spawn("fs-ktest-stallw", sched::Class::Normal, move || {
                let c = Ctx::kernel(&ns);
                for i in 0..60u32 {
                    let p = format!("/x/w{t}/f{i}");
                    let ok = (|| -> Result<(), i64> {
                        let f = c.open(None, p.as_bytes(), O_RDWR | O_CREAT, 0o600)?;
                        f.pwrite(0, &[i as u8; 3000])?;
                        ns.lookup(&p)?.inode.sync(SyncKind::Fsync)?;
                        // Not one the step before renamed.
                        if i % 3 == 2 && (i - 1) % 5 != 4 {
                            c.unlink(None, format!("/x/w{t}/f{}", i - 1).as_bytes())?;
                        }
                        if i % 5 == 4 {
                            c.rename(None, p.as_bytes(), None, format!("/x/w{t}/r{i}").as_bytes(), 0)?;
                        }
                        Ok(())
                    })();
                    if ok.is_err() {
                        failed.fetch_add(1, Ordering::SeqCst);
                    }
                }
            })
            .map_err(e)?,
        );
    }
    let reader = {
        let (ns, stop, worst, calls, failed) = (ns.clone(), stop.clone(), worst.clone(), calls.clone(), failed.clone());
        sched::spawn("fs-ktest-stallr", sched::Class::Normal, move || {
            let c = Ctx::kernel(&ns);
            let mut k = 0u32;
            while !stop.load(Ordering::Acquire) {
                let p = format!("/x/s/f{}", k % 40);
                let a = now();
                let ok = c.stat(None, p.as_bytes(), true).is_ok() && ns.read_file(&p, 4096).is_ok_and(|d| d.len() == 2000);
                let took = now().saturating_sub(a);
                worst.fetch_max(took, Ordering::SeqCst);
                calls.fetch_add(1, Ordering::Relaxed);
                if !ok {
                    failed.fetch_add(1, Ordering::SeqCst);
                }
                k = k.wrapping_add(1);
                sched::sleep_ns(200_000);
            }
        })
        .map_err(e)?
    };
    for w in &writers {
        w.join();
    }
    stop.store(true, Ordering::Release);
    reader.join();
    rd.set_delay_ns(0);
    let took_ms = now().saturating_sub(t0) / 1_000_000;
    let (w, n) = (worst.load(Ordering::SeqCst), calls.load(Ordering::SeqCst));
    let (vol, commit, data) = (hold_max_ns("ext4.vol"), hold_max_ns("ext4.commit"), hold_max_ns("vfs.inode.data"));
    klog::event!(Info, "ext4", "ktest.sync_stall", reader_calls = n, reader_worst_us = w / 1000, run_ms = took_ms, vol_hold_max_us = vol / 1000, commit_hold_max_us = commit / 1000, data_hold_max_us = data / 1000);
    kassert!(failed.load(Ordering::SeqCst) == 0, "{} calls failed under the load", failed.load(Ordering::SeqCst));
    kassert!(n > 0, "the reader never ran");
    // Generous (QEMU's timing is loose): a stat and a cached read never wait for whole commits in a row.
    kassert!(w < 2_000_000_000, "a cached stat and read took {} ms beside fsyncs ({n} calls)", w / 1_000_000);
    Ok(())
}

/// The disk paused under an fsync: everything else goes on, the fsync waits.
fn order(rd: &Arc<block::RamDisk>, ns: &Arc<vfs::MountNs>) -> TestResult {
    let c = Ctx::kernel(ns);
    // Into the disk's cache first: the calls below must find what they read in memory.
    ns.create_file_bytes(b"/x/fsynced", 0o600, b"").map_err(e)?;
    ns.lookup("/x/fsynced").map_err(e)?.inode.sync(SyncKind::Fsync).map_err(e)?;
    let _ = c.stat(None, b"/x/s/f3", true).map_err(e)?;
    let _ = ns.list_dir("/x/s").map_err(e)?;
    rd.pause();
    let n = ns.clone();
    let fsync = sched::spawn("fs-ktest-stallf", sched::Class::Normal, move || {
        let c = Ctx::kernel(&n);
        let r = c.open(None, b"/x/fsynced", O_RDWR, 0).and_then(|f| f.pwrite(0, b"fsynced while the disk was paused")).and_then(|_| n.lookup("/x/fsynced")?.inode.sync(SyncKind::Fsync));
        kassert_log(r.is_ok());
    })
    .map_err(e)?;
    let mut waited = 0;
    while !rd.holding() && waited < 5000 {
        sched::sleep_ns(1_000_000);
        waited += 1;
    }
    kassert!(rd.holding(), "the fsync never reached the paused disk");
    const LIMIT: u64 = 3_000_000_000;
    let mut stuck = Vec::new();
    let n = ns.clone();
    if !within(LIMIT, move || { let _ = Ctx::kernel(&n).stat(None, b"/x/s/f7", true); })? {
        stuck.push("stat");
    }
    let n = ns.clone();
    if !within(LIMIT, move || { let _ = n.read_file("/x/s/f7", 4096); })? {
        stuck.push("cached read");
    }
    let n = ns.clone();
    if !within(LIMIT, move || { let _ = n.list_dir("/x/s"); })? {
        stuck.push("listing");
    }
    let n = ns.clone();
    if !within(LIMIT, move || { let _ = Ctx::kernel(&n).open(None, b"/x/s/f3", O_RDWR, 0).and_then(|f| f.pwrite(0, b"small")); })? {
        stuck.push("small write");
    }
    let n = ns.clone();
    if !within(LIMIT, move || { let _ = Ctx::kernel(&n).open(None, b"/x/new-during-the-pause", O_RDWR | O_CREAT, 0o600); })? {
        stuck.push("new file");
    }
    let early = fsync.is_finished();
    rd.resume();
    fsync.join();
    kassert!(stuck.is_empty(), "waited for the paused fsync: {stuck:?}");
    kassert!(!early, "the fsync returned while its disk was paused");
    kassert!(FSYNC_FAILED.load(Ordering::SeqCst) == 0, "the paused fsync failed");
    for p in ["/x/s/f3", "/x/new-during-the-pause"] {
        ns.lookup(p).map_err(e)?.inode.sync(SyncKind::Fsync).map_err(e)?;
    }
    Ok(())
}

static FSYNC_FAILED: AtomicU64 = AtomicU64::new(0);

fn kassert_log(ok: bool) {
    if !ok {
        FSYNC_FAILED.fetch_add(1, Ordering::SeqCst);
    }
}

ktest!("fs.ext4.sync_stall", Safe, sync_stall);
