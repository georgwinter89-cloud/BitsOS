//! BL-3 ktests: ext4 file data straight from the disk into the file's cache pages (`Volume::read_pages`,
//! `CachedDiskDev::read_pages`), on a RAM disk of their own through the block layer, as /data runs on the Mini-PC.
//!
//! - `fs.ext4.frag_read`: a fragmented file (16 data extents between another file's, holes, an unwritten extent,
//!   a block alone in a hole, a tail not on a page) reads back byte by byte as written, cold; the disk's own cache
//!   gains no file page and no read went through it or through a copy; `block.copy_bytes.bdev_cache`,
//!   `ext4_page` and `ext4.read_page_calls` move by less than half a file's worth, `bdev_cache` (which counts
//!   the metadata reads) by less than one (other readers of the boot move them too; the old path moved them by the
//!   whole file).
//! - `fs.ext4.read_unflushed`: bytes written into the disk's cache and not flushed are read through the cache
//!   (the disk does not hold them), the pages around them from the disk; after the flush the same read goes to the disk and finds them; a read not in
//!   whole disk blocks goes through the cache.
//!
//! Both Safe and rerunnable: their own RAM disks, unregistered at the end. `frag_read` drops the machine's clean
//! cache once per run (drop_caches 1, as BL-0's tests).

use crate::dev::{CachedDiskDev, Dev, PageRead, COPY_BDEV_CACHE};
use crate::file::{COPY_EXT4_PAGE, READ_PAGE_CALLS};
use crate::{mount, Options};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use block::PageSeg;
use core::sync::atomic::{AtomicU64, Ordering};
use ktest::{kassert, ktest, TestResult};
use vfs::abi::{FALLOC_FL_KEEP_SIZE, O_CREAT, O_RDONLY, O_RDWR, O_TRUNC};
use vfs::ops::Ctx;
use vfs::pagecache::{Backend, CachePage, PageCache, SyncKind};

static IMAGE: &[u8] = include_bytes!("../testdata/ktest.img.z");
static RUN: AtomicU64 = AtomicU64::new(0);
const P: u64 = 4096;
/// Rounds of the fragmented file: 3 pages of data, then 3 pages of hole (the other file's 2 pages between).
pub(crate) const ROUNDS: u64 = 16;
const TAIL: usize = 1234;

fn e<T: core::fmt::Debug>(x: T) -> String {
    format!("{x:?}")
}

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed) ^ (i >> 12) as u8).collect()
}

fn put(want: &mut Vec<u8>, at: u64, data: &[u8]) {
    let end = at as usize + data.len();
    if want.len() < end {
        want.resize(end, 0);
    }
    want[at as usize..end].copy_from_slice(data);
}

/// Builds `dir`/bl3-frag on a mounted ext4 and returns its path and its bytes. Each round's data is written back
/// before the filler's, so the allocator puts them between (one extent per round); round 5's hole is preallocated
/// (unwritten), round 9's hole holds 100 bytes in a block of their own, and the file ends 1234 bytes into a page.
/// Fsynced at the end: nothing is left in the disk's cache that the disk does not hold.
pub(crate) fn build_fragmented(ns: &vfs::MountNs, dir: &str) -> Result<(String, Vec<u8>), String> {
    let path = format!("{dir}/bl3-frag");
    let fill = format!("{dir}/bl3-fill");
    let k = Ctx::kernel(ns);
    let f = k.open(None, path.as_bytes(), O_RDWR | O_CREAT | O_TRUNC, 0o644).map_err(e)?;
    let g = k.open(None, fill.as_bytes(), O_RDWR | O_CREAT | O_TRUNC, 0o644).map_err(e)?;
    let fi = ns.lookup(&path).map_err(e)?.inode.clone();
    let gi = ns.lookup(&fill).map_err(e)?.inode.clone();
    let mut want = Vec::new();
    for r in 0..ROUNDS {
        let data = pattern((3 * P) as usize, r as u8 + 1);
        kassert!(f.pwrite(r * 6 * P, &data).map_err(e)? == data.len(), "write round {r}");
        put(&mut want, r * 6 * P, &data);
        fi.sync(SyncKind::Fsync).map_err(e)?;
        kassert!(g.pwrite(r * 2 * P, &pattern((2 * P) as usize, 200)).map_err(e)? == (2 * P) as usize, "filler round {r}");
        gi.sync(SyncKind::Fsync).map_err(e)?;
    }
    fi.fallocate(FALLOC_FL_KEEP_SIZE, 5 * 6 * P + 3 * P, 3 * P).map_err(e)?;
    let lone = pattern(100, 77);
    kassert!(f.pwrite(9 * 6 * P + 4 * P + 1000, &lone).map_err(e)? == 100, "write into round 9's hole");
    put(&mut want, 9 * 6 * P + 4 * P + 1000, &lone);
    let tail = pattern(TAIL, 99);
    kassert!(f.pwrite(ROUNDS * 6 * P, &tail).map_err(e)? == TAIL, "write the tail");
    put(&mut want, ROUNDS * 6 * P, &tail);
    fi.sync(SyncKind::Fsync).map_err(e)?;
    kassert!(fi.size() == want.len() as u64, "size {} want {}", fi.size(), want.len());
    Ok((path, want))
}

/// (data extents, unwritten extents, holes) of regular file `ino`, from its mapping.
pub(crate) fn extents(v: &crate::Volume, ino: u32, size: u64) -> Result<(u64, u64, u64), String> {
    let st = v.lock();
    let di = st.inode(None, ino).map_err(e)?;
    let (bs, mut l, mut n) = (v.geo.bs, 0u64, (0, 0, 0));
    while l < size.div_ceil(bs) {
        match st.map_at(None, &di, l).map_err(e)? {
            Ok((_, len, unwritten)) => {
                if unwritten {
                    n.1 += 1;
                } else {
                    n.0 += 1;
                }
                l += len.max(1);
            }
            Err(hole) => {
                n.2 += 1;
                l = l.saturating_add(hole.max(1));
            }
        }
    }
    Ok(n)
}

/// The file's bytes by pread in 64 KiB steps (each a window of the cache, so many `read_pages` calls).
pub(crate) fn read_all(ns: &vfs::MountNs, path: &str, len: usize) -> Result<Vec<u8>, String> {
    let f = Ctx::kernel(ns).open(None, path.as_bytes(), O_RDONLY, 0).map_err(e)?;
    let mut back = alloc::vec![0xEEu8; len + 100];
    let mut done = 0;
    while done < back.len() {
        let end = (done + 65536).min(back.len());
        let n = f.pread(done as u64, &mut back[done..end]).map_err(e)?;
        if n == 0 {
            break;
        }
        done += n;
    }
    kassert!(done == len, "read {done} bytes of {len}");
    back.truncate(len);
    Ok(back)
}

/// The first byte where `got` and `want` differ.
pub(crate) fn first_diff(got: &[u8], want: &[u8]) -> Option<usize> {
    got.iter().zip(want).position(|(a, b)| a != b).or((got.len() != want.len()).then_some(got.len().min(want.len())))
}

/// Cold: the file's pages and the machine's clean cache dropped, then read back and compared byte by byte.
pub(crate) fn cold_check(ns: &vfs::MountNs, path: &str, want: &[u8]) -> TestResult {
    let d = ns.lookup(path).map_err(e)?;
    let c = d.inode.page_cache().ok_or("no page cache")?;
    c.dont_need(0, 0);
    kassert!(c.pages() == 0, "the file's pages went: {} left", c.pages());
    let back = read_all(ns, path, want.len())?;
    if let Some(i) = first_diff(&back, want) {
        let (g, w) = (back.get(i).copied().unwrap_or(0), want.get(i).copied().unwrap_or(0));
        return Err(format!("byte {i} (page {}) reads {g:#x}, written {w:#x}", i / P as usize));
    }
    Ok(())
}

fn frag_read() -> TestResult {
    let img = miniz_oxide::inflate::decompress_to_vec_zlib(IMAGE).map_err(|x| format!("inflate: {x:?}"))?;
    let name = format!("bl3frag{}", RUN.fetch_add(1, Ordering::Relaxed));
    let blocks = img.len() as u64 / 512;
    let rd = Arc::new(block::RamDisk::from_image(&name, 512, img, false));
    let disk = block::register(rd.clone(), block::DiskKind::Ram).map_err(e)?;
    let r = (|| {
        let ns = vfs::MountNs::new_empty();
        let dev = Arc::new(CachedDiskDev::new(disk.clone(), 0, blocks));
        let (v, _m) = mount::mount_dev(dev.clone(), None, &ns, "/x", Options::default()).map_err(e)?;
        let r = (|| {
            let (path, want) = build_fragmented(&ns, "/x")?;
            let ino = Ctx::kernel(&ns).stat(None, path.as_bytes(), true).map_err(e)?.ino as u32;
            let (data, unwritten, holes) = extents(&v, ino, want.len() as u64)?;
            kassert!(data >= ROUNDS && unwritten >= 1 && holes >= ROUNDS - 2, "extents: {data} data, {unwritten} unwritten, {holes} holes");
            // The disk's cache still holds the file's blocks as they were written: drop them, so a read through it
            // would have to fill it again.
            vfs::reclaim::drop_caches(1, "ktest", 0);
            let (cached0, via0, frames0, st0) = (dev.cached_pages(), dev.file_reads_via_cache(), rd.frame_calls(), disk.stats());
            let (ep0, rp0, bc0) = (COPY_EXT4_PAGE.sum(), READ_PAGE_CALLS.sum(), COPY_BDEV_CACHE.sum());
            cold_check(&ns, &path, &want)?;
            let (ep, rp, bc) = (COPY_EXT4_PAGE.sum() - ep0, READ_PAGE_CALLS.sum() - rp0, COPY_BDEV_CACHE.sum() - bc0);
            let st = disk.stats();
            let grown = dev.cached_pages().saturating_sub(cached0);
            let file_pages = want.len().div_ceil(P as usize);
            kassert!(grown <= 4, "the disk's cache gained {grown} pages reading a {file_pages}-page file (metadata only)");
            kassert!(dev.file_reads_via_cache() == via0, "{} file reads through the disk's cache", dev.file_reads_via_cache() - via0);
            kassert!(rd.frame_calls() > frames0, "no read went into frames");
            kassert!(st.copied_bytes == st0.copied_bytes, "the disk copied {} bytes", st.copied_bytes - st0.copied_bytes);
            kassert!(st.read_bytes - st0.read_bytes >= ROUNDS * 3 * P, "the disk read {} bytes", st.read_bytes - st0.read_bytes);
            // The machine-wide counters (BL-0's columns): other readers of the boot move them too, so not "0". The
            // old path moves them by the whole file (a copy of each page through the disk's cache and into the
            // page, a call per page): below one file's worth means this read did not; metadata (the extent tree,
            // the inode) is what `bdev_cache` may count here.
            let bytes = want.len() as u64;
            kassert!(ep < bytes / 2, "block.copy_bytes.ext4_page moved {ep} bytes reading a {bytes}-byte file");
            kassert!(rp < file_pages as u64 / 2, "ext4.read_page_calls moved {rp} reading a {file_pages}-page file");
            // The extent tree read per window copies some 150 KB here (QEMU); the old path copied every file byte on top.
            kassert!(bc < bytes, "block.copy_bytes.bdev_cache moved {bc} bytes reading a {bytes}-byte file");
            klog::event!(Info, "ktest", "bl3.counters", ext4_page = ep, read_page_calls = rp, bdev_cache = bc; "frag_read: the machine-wide copy counters while reading the file");
            Ok(())
        })();
        let s = v.sync_fs().map_err(e);
        r.and(s)
    })();
    block::unregister(&name);
    r
}
ktest!("fs.ext4.frag_read", Safe, frag_read);

/// Zeroed cache pages of a cache that reads nothing.
pub(crate) fn blank_pages(n: u64) -> Result<Vec<Arc<CachePage>>, String> {
    struct Nothing;
    impl Backend for Nothing {
        fn read_page(&self, _: u64, _: &Arc<CachePage>) -> Result<(), i64> {
            Ok(())
        }
        fn write_run(&self, _: u64, _: &[Arc<CachePage>], _: u64) -> Result<(), i64> {
            Ok(())
        }
    }
    let c = PageCache::with_backend(Arc::new(Nothing), n * P);
    (0..n).map(|i| c.page_for_map(i, false).map_err(e)).collect()
}

/// One read of `n` whole pages at byte `at` of `dev`; the bytes read.
pub(crate) fn read_into(dev: &CachedDiskDev, at: u64, n: u64) -> Result<Vec<u8>, String> {
    let pages = blank_pages(n)?;
    let segs = pages.iter().map(|p| PageSeg { page: p.clone(), off: 0, len: P as usize }).collect();
    dev.read_pages(alloc::vec![PageRead { at, len: (n * P) as usize, segs }]).map_err(e)?;
    let mut out = alloc::vec![0u8; (n * P) as usize];
    for (p, b) in pages.iter().zip(out.chunks_mut(P as usize)) {
        p.read(0, b);
    }
    Ok(out)
}

/// The steps of `fs.ext4.read_unflushed` on `dev` over a disk holding `img` whose reads into frames `frames` counts.
pub(crate) fn unflushed_steps(dev: &CachedDiskDev, img: &[u8], frames: &dyn Fn() -> u64) -> TestResult {
    let at = 64 * P;
    let got = read_into(dev, at, 4)?;
    kassert!(got[..] == img[at as usize..(at + 4 * P) as usize], "a cold read from the disk");
    kassert!(dev.file_reads_via_cache() == 0 && dev.cached_pages() == 0, "it went to the disk: via cache {}, cached {}", dev.file_reads_via_cache(), dev.cached_pages());
    let new = pattern(P as usize, 123);
    dev.write(at + P, &new).map_err(e)?;
    let f0 = frames();
    let got = read_into(dev, at, 4)?;
    kassert!(got[P as usize..2 * P as usize] == new[..], "the bytes written and not flushed read back");
    kassert!(got[..P as usize] == img[at as usize..(at + P) as usize], "the page before is the disk's");
    kassert!(dev.file_reads_via_cache() == 1, "the written page went through the cache: {}", dev.file_reads_via_cache());
    kassert!(frames() > f0, "the pages around it went to the disk");
    dev.flush().map_err(e)?;
    let f1 = frames();
    let got = read_into(dev, at, 4)?;
    kassert!(got[P as usize..2 * P as usize] == new[..], "after the flush the disk holds them");
    kassert!(dev.file_reads_via_cache() == 1 && frames() > f1, "after the flush the read went to the disk");
    // Not in whole disk blocks (an ext4 block of 1 KiB on a 4 KiB-sector disk): through the cache.
    let pages = blank_pages(1)?;
    let seg = PageSeg { page: pages[0].clone(), off: 0, len: 256 };
    dev.read_pages(alloc::vec![PageRead { at: 100 * P, len: 256, segs: alloc::vec![seg] }]).map_err(e)?;
    let mut b = alloc::vec![0u8; 256];
    pages[0].read(0, &mut b);
    kassert!(b[..] == img[(100 * P) as usize..(100 * P) as usize + 256], "a part block reads through the cache");
    kassert!(dev.file_reads_via_cache() == 2, "the part block went through the cache: {}", dev.file_reads_via_cache());
    Ok(())
}

fn read_unflushed() -> TestResult {
    let name = format!("bl3dirty{}", RUN.fetch_add(1, Ordering::Relaxed));
    let img = pattern(256 * P as usize, 5);
    let rd = Arc::new(block::RamDisk::from_image(&name, 512, img.clone(), false));
    let disk = block::register(rd.clone(), block::DiskKind::Ram).map_err(e)?;
    let dev = CachedDiskDev::new(disk.clone(), 0, 256 * P / 512);
    let r = unflushed_steps(&dev, &img, &|| rd.frame_calls());
    drop(dev);
    block::unregister(&name);
    r
}
ktest!("fs.ext4.read_unflushed", Safe, read_unflushed);
