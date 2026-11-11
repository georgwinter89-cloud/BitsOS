//! BL-0 ktests: POSIX_FADV_DONTNEED and `/proc/sys/vm/drop_caches` on real frames, ext4 on a RAM disk through the
//! block layer and its disk cache (`CachedDiskDev`), as /data runs on the Mini-PC. What goes and what stays:
//!
//! - `fs.fadvise.dontneed`: clean file pages go and read back from the disk; a page a mapping pins and a page a
//!   reader holds stay; a dirty page is written back first, then goes, and reads back with its new bytes.
//! - `fs.drop_caches.bdev`: drop_caches 1 empties the disk's own cache of its clean pages (ext4's metadata); a dirty
//!   disk page stays until it is written; file pages go, a mapped one stays.
//! - `fs.drop_caches.sysctl`: the file takes 1..=4 and refuses 0, 5, a sign and words (EINVAL) as Linux's
//!   `proc_dointvec_minmax`; uid 1000 may not open it for writing, root not for reading (EACCES).
//! - `fs.drop_caches.all`: drop_caches 3 through the file, the step blk0.sh runs before each measurement.
//!
//! All Safe and rerunnable: a RAM disk of their own, unregistered at the end. drop_caches drops the whole machine's
//! clean cache once per run (the feature itself; the next reads refill it).

use crate::dev::{CachedDiskDev, Dev, COPY_BDEV_CACHE, FRAME_BYTES};
use crate::{mount, Options};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use ktest::{kassert, ktest, TestResult};
use vfs::abi::{O_CREAT, O_RDONLY, O_RDWR, O_WRONLY};
use vfs::ops::Ctx;
use vfs::pagecache::{PageCache, SyncKind};

static IMAGE: &[u8] = include_bytes!("../testdata/ktest.img.z");
static RUN: AtomicU64 = AtomicU64::new(0);
const P: u64 = 4096;

fn e<T: core::fmt::Debug>(x: T) -> String {
    format!("{x:?}")
}

/// The test image on a RAM disk of its own, mounted at /x through the disk cache; `f` runs on it.
fn on_disk(f: impl FnOnce(&vfs::MountNs, &Arc<block::Disk>) -> TestResult) -> TestResult {
    let img = miniz_oxide::inflate::decompress_to_vec_zlib(IMAGE).map_err(|x| format!("inflate: {x:?}"))?;
    let name = format!("bl0test{}", RUN.fetch_add(1, Ordering::Relaxed));
    let blocks = img.len() as u64 / 512;
    let rd = Arc::new(block::RamDisk::from_image(&name, 512, img, false));
    let disk = block::register(rd, block::DiskKind::Ram).map_err(e)?;
    let r = (|| {
        let ns = vfs::MountNs::new_empty();
        let (v, _m) = mount::mount_disk(disk.clone(), 0, blocks, &ns, "/x", Options::default()).map_err(e)?;
        let r = f(&ns, &disk);
        // Committed while the disk is still registered: the volume's drop after unregister would write to a gone
        // disk (EIO, "volume now read-only") on every run.
        let s = v.sync_fs().map_err(e);
        r.and(s)
    })();
    block::unregister(&name);
    r
}

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed)).collect()
}

/// A file of `pages` pages with `seed`'s pattern, written and fsynced: every page is in the cache, clean, and no
/// read missed, so no read-ahead job runs later and refills what a test drops.
fn make(ns: &vfs::MountNs, path: &str, pages: u64, seed: u8) -> Result<Vec<u8>, String> {
    let data = pattern((pages * P) as usize, seed);
    let f = Ctx::kernel(ns).open(None, path.as_bytes(), O_RDWR | O_CREAT, 0o644).map_err(e)?;
    kassert!(f.pwrite(0, &data).map_err(e)? == data.len(), "write {path}");
    ns.lookup(path).map_err(e)?.inode.sync(SyncKind::Fsync).map_err(e)?;
    Ok(data)
}

fn cache_of(d: &Arc<vfs::Dentry>) -> Result<&PageCache, String> {
    d.inode.page_cache().ok_or_else(|| String::from("no page cache"))
}

fn resident(c: &PageCache, pages: u64) -> Vec<u64> {
    (0..pages).filter(|&i| c.cached(i).is_some()).collect()
}

fn fadvise_dontneed() -> TestResult {
    on_disk(|ns, disk| {
        let path = "/x/bl0-dontneed";
        let data = make(ns, path, 32, 7)?;
        let d = ns.lookup(path).map_err(e)?;
        let c = cache_of(&d)?;
        kassert!(resident(c, 32).len() == 32, "all 32 pages cached after the read: {:?}", resident(c, 32));
        let pin = c.page_for_map(3, false).map_err(e)?;
        let held = c.cached(5).ok_or("page 5 missing")?;
        c.write_at(9 * P, b"new bytes").map_err(e)?;
        let r = c.dont_need(0, 0);
        kassert!(r.dropped == 30 && r.kept == 2, "{r:?}");
        kassert!(resident(c, 32) == [3, 5], "mapped page 3 and held page 5 stay: {:?}", resident(c, 32));
        kassert!(c.cached(3).is_some_and(|p| Arc::ptr_eq(&p, &pin)), "the mapping's page is the cache's");
        drop((pin, held));
        let frames0 = FRAME_BYTES.sum();
        let f = Ctx::kernel(ns).open(None, path.as_bytes(), O_RDWR, 0).map_err(e)?;
        let mut back = alloc::vec![0u8; data.len()];
        kassert!(f.pread(0, &mut back).map_err(e)? == back.len(), "read back");
        // BL-3: the 30 dropped pages came back from the disk straight into the cache pages (before BL-3 through
        // ext4's read_page and two copies); the dirty page 9 may still be in the disk's cache only and come from
        // there. The counter the BL-0 procedure reads saw them (other readers of the boot only add to it).
        let frames = FRAME_BYTES.sum() - frames0;
        kassert!(frames >= 29 * P, "ext4.read_frames_bytes +{frames}");
        let st = disk.stats();
        kassert!(st.inflight_max >= 1 && st.inflight == 0, "disk in flight now {} max {}", st.inflight, st.inflight_max);
        let mut want = data.clone();
        want[(9 * P) as usize..(9 * P) as usize + 9].copy_from_slice(b"new bytes");
        kassert!(back == want, "the file reads back from the disk, the dirty page's bytes included");
        Ok(())
    })
}
ktest!("fs.fadvise.dontneed", Safe, fadvise_dontneed);

fn drop_caches_bdev() -> TestResult {
    on_disk(|ns, disk| {
        let path = "/x/bl0-drop";
        make(ns, path, 16, 3)?;
        let d = ns.lookup(path).map_err(e)?;
        let c = cache_of(&d)?;
        let pin = c.page_for_map(0, false).map_err(e)?;
        // A disk cache of its own on the same disk: eight scattered page reads (each a random miss, so no read-ahead
        // job refills the cache later) and one dirty page.
        let dev = CachedDiskDev::new(disk.clone(), 0, disk.dev_info().blocks);
        let mut first = alloc::vec![0u8; P as usize];
        let mut page = alloc::vec![0u8; P as usize];
        for i in 0..8 {
            dev.read((50 + 3 * i) * P, if i == 0 { &mut first } else { &mut page }).map_err(e)?;
        }
        kassert!(dev.cached_pages() == 8, "8 disk pages cached: {}", dev.cached_pages());
        dev.read(40 * P, &mut page).map_err(e)?;
        dev.write(40 * P, &page).map_err(e)?;
        let r = vfs::reclaim::drop_caches(1, "ktest", 0);
        kassert!(dev.cached_pages() == 1, "the disk's clean pages went, its dirty page stays: {} left ({r:?})", dev.cached_pages());
        kassert!(resident(c, 16) == [0], "file pages went, the mapped one stays: {:?}", resident(c, 16));
        kassert!(r.dropped >= 8 + 15 && r.kept >= 2, "{r:?}");
        dev.flush().map_err(e)?;
        vfs::reclaim::drop_caches(1, "ktest", 0);
        kassert!(dev.cached_pages() == 0, "written: now it goes too ({} left)", dev.cached_pages());
        let bdev0 = COPY_BDEV_CACHE.sum();
        dev.read(50 * P, &mut page).map_err(e)?;
        kassert!(page == first, "the disk page reads back the same from the disk");
        kassert!(COPY_BDEV_CACHE.sum() - bdev0 >= P, "bdev_cache counted the read");
        let st = disk.stats();
        kassert!(st.inflight_max >= 1 && st.inflight == 0, "disk in flight now {} max {}", st.inflight, st.inflight_max);
        drop(pin);
        Ok(())
    })
}
ktest!("fs.drop_caches.bdev", Safe, drop_caches_bdev);

fn drop_caches_sysctl() -> TestResult {
    let ns = vfs::root_ns();
    let path = "/proc/sys/vm/drop_caches";
    let wr = |v: &[u8]| ns.write_file(path, v, Some(0));
    for (v, want) in [(&b"0"[..], Err(uapi::errno::EINVAL)), (b"5", Err(uapi::errno::EINVAL)), (b"-1", Err(uapi::errno::EINVAL)), (b"abc", Err(uapi::errno::EINVAL)), (b"1\n", Ok(2)), (b"3", Ok(1))] {
        let got = wr(v);
        kassert!(got == want, "drop_caches {:?}: {got:?}, Linux {want:?}", String::from_utf8_lossy(v));
    }
    let a = Ctx::kernel(&ns).stat(None, path.as_bytes(), true).map_err(e)?;
    kassert!(a.mode & 0o7777 == 0o200 && a.uid == 0, "mode {:o} uid {}", a.mode, a.uid);
    // Linux's test_perm: root gets the owner bits (write only), everyone else the other bits (nothing).
    let mut user = Ctx::kernel(&ns);
    user.cred = vfs::inode::Cred { uid: 1000, gid: 1000 };
    let got = user.open(None, path.as_bytes(), O_WRONLY, 0).map(|_| ());
    kassert!(got == Err(uapi::errno::EACCES), "uid 1000 opens it for writing: {got:?}, Linux EACCES");
    let got = Ctx::kernel(&ns).open(None, path.as_bytes(), O_RDONLY, 0).map(|_| ());
    kassert!(got == Err(uapi::errno::EACCES), "root opens it for reading: {got:?}, Linux EACCES");
    Ok(())
}
ktest!("fs.drop_caches.sysctl", Safe, drop_caches_sysctl);

/// The drop host/tests/minipc/blk0.sh asks for before every measured run: "3" through the sysctl file, as root's
/// `echo 3 > /proc/sys/vm/drop_caches` (MCP's `run` starts programs as uid 1000 and `write_file` stays out of
/// /proc: plan/notes/bl-BL-0.md, change request). The clean page cache of every file and disk leaves.
fn drop_caches_all() -> TestResult {
    let before = vfs::reclaim::drop_caches_done();
    let got = vfs::root_ns().write_file("/proc/sys/vm/drop_caches", b"3\n", Some(0));
    kassert!(got == Ok(2), "write 3: {got:?}");
    kassert!(vfs::reclaim::drop_caches_done() > before, "the sysctl ran the drop");
    Ok(())
}
ktest!("fs.drop_caches.all", Safe, drop_caches_all);
