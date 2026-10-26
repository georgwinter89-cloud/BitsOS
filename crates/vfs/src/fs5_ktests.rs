//! FS-5 ktests: the page cache on real frames, the `/tmp` and `/dev/shm` mounts, the pools (ENOSPC/ENOMEM),
//! the Chromium shm pattern at the backing level (MM's fault path on top is tested by `vfs-sys` once MM's side
//! is in). All Safe and rerunnable in one boot: unique names, the pool limit restored, everything removed.

use crate::abi::{O_CREAT, O_EXCL, O_RDWR};
use crate::mmap::{self, MapBacking, MAP_SHARED};
use crate::mount_ns::MountNs;
use crate::ops::Ctx;
use crate::pagecache::{self, PageFault};
use crate::{pool, tmpfs};
use alloc::format;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use ktest::{kassert, ktest, TestResult};
use uapi::errno::*;

const P: u64 = 4096;
static RUN: AtomicU64 = AtomicU64::new(0);

/// `/tmp` and `/dev/shm` are tmpfs instances of their own (1777), bounded only by the system pool.
fn mounts() -> TestResult {
    let ns = crate::root_ns();
    let root_dev = ns.root_path().attr().dev;
    let mut devs = Vec::new();
    for path in ["/tmp", "/dev/shm"] {
        let p = ns.lookup_path(path.as_bytes(), true).map_err(|e| format!("{path}: {e}"))?;
        let a = p.attr();
        kassert!(p.is_mount_root() && p.mnt.sb.fstype == "tmpfs", "{path}: {} mount root {}", p.mnt.sb.fstype, p.is_mount_root());
        kassert!(a.mode == uapi::S_IFDIR | 0o1777, "{path}: mode {:o}", a.mode);
        kassert!(a.dev != root_dev && !devs.contains(&a.dev), "{path}: dev {:#x} not its own", a.dev);
        kassert!(p.mnt.sb.pool.size().is_none(), "{path} has its own size {:?}", p.mnt.sb.pool.size());
        devs.push(a.dev);
    }
    let lim = pool::system_limit();
    let total = mm_core::frames().map(|f| f.stats().total_bytes).unwrap_or(0);
    // Exact: measured again after the initramfs module went back to the frame allocator (D4, INT-A1d).
    kassert!(lim == pool::limit_for(total), "pool {lim} pages of {} (RAM minus POOL_KEEP: {})", total / P, pool::limit_for(total));
    Ok(())
}
ktest!("fs.cache.mounts", Safe, mounts);

/// Regular files live in frames: distinct, page-aligned physical addresses; read back what was written;
/// truncate gives frames and charge back.
fn frames() -> TestResult {
    let ns = MountNs::new_empty();
    let c = Ctx::kernel(&ns);
    let m = tmpfs::mount_new(&ns, "/t", 0o1777, None).map_err(|e| format!("mount: {e}"))?;
    let before = pagecache::frames_held();
    let f = c.open(None, b"/t/f", O_CREAT | O_RDWR, 0o600).map_err(|e| format!("open: {e}"))?;
    let data: Vec<u8> = (0..3 * P as usize).map(|i| (i * 7 % 251) as u8).collect();
    kassert!(f.pwrite(0, &data) == Ok(data.len()), "write");
    kassert!(pagecache::frames_held() >= before + 3, "frames held {} (before {before})", pagecache::frames_held());
    // FS-14b: page-cache frames are their own kind (`mem.used.pagecache`), no longer counted as DMA.
    let kind = mm_core::frames().map_or(0, |f| f.stats().used_by_kind[mm_core::Kind::PageCache as usize]);
    kassert!(kind >= 3 * P, "Kind::PageCache holds {kind} bytes with 3 pages cached");
    let cache = f.inode().ok_or("no inode")?.page_cache().ok_or("no cache")?;
    let mut phys = Vec::new();
    for pg in 0..3 {
        let p = cache.cached(pg).and_then(|p| p.phys()).ok_or_else(|| format!("page {pg} has no frame"))?;
        kassert!(p % P == 0 && !phys.contains(&p), "page {pg} at {p:#x}");
        phys.push(p);
    }
    let mut back = vec![0u8; data.len()];
    kassert!(f.pread(0, &mut back) == Ok(data.len()) && back == data, "read back differs");
    kassert!(m.sb.pool.used() == 3, "charged {}", m.sb.pool.used());
    f.inode().ok_or("no inode")?.truncate(0).map_err(|e| format!("truncate: {e}"))?;
    kassert!(m.sb.pool.used() == 0 && cache.pages() == 0, "after truncate: charged {} pages {}", m.sb.pool.used(), cache.pages());
    drop(f);
    Ok(())
}
ktest!("fs.cache.frames", Safe, frames);

/// The pools (FS-14: pages charged when allocated): a mount's own size and the system pool both answer ENOSPC
/// (a short write first), a growing ftruncate is free; both give everything back.
fn pools() -> TestResult {
    let ns = MountNs::new_empty();
    let c = Ctx::kernel(&ns);
    let small = tmpfs::mount_new(&ns, "/small", 0o1777, Some(4)).map_err(|e| format!("mount: {e}"))?;
    let big = tmpfs::mount_new(&ns, "/big", 0o1777, None).map_err(|e| format!("mount: {e}"))?;
    let f = c.open(None, b"/small/f", O_CREAT | O_RDWR, 0o600).map_err(|e| format!("open: {e}"))?;
    let i = f.inode().ok_or("no inode")?.clone();
    kassert!(i.truncate(4 * P).is_ok(), "ftruncate to the size");
    kassert!(i.truncate(5 * P).is_ok(), "ftruncate past the size is free");
    kassert!(f.pwrite(0, &vec![1u8; 8 * P as usize]) == Ok(4 * P as usize), "short write at the size");
    kassert!(f.pwrite(4 * P, b"x") == Err(ENOSPC), "write past the size");
    let g = c.open(None, b"/big/g", O_CREAT | O_RDWR, 0o600).map_err(|e| format!("open: {e}"))?;
    let gi = g.inode().ok_or("no inode")?.clone();
    let charged = pool::system_charged();
    // The limit is on data and metadata together (review INT-A1d fs 1): two pages above what is in use now.
    pool::set_system_limit(Some(pool::system_used() + 2));
    let r1 = gi.truncate(3 * P);
    let r2 = g.pwrite(0, &vec![2u8; 3 * P as usize]);
    let r3 = g.pwrite(2 * P, b"y");
    pool::set_system_limit(None);
    kassert!(r1 == Ok(()), "ftruncate past the system pool is free: {r1:?}");
    kassert!(r2 == Ok(2 * P as usize), "write at the system pool: {r2:?}");
    kassert!(r3 == Err(ENOSPC), "write past the system pool: {r3:?}");
    drop((f, g, i, gi));
    for p in [b"/small/f".as_slice(), b"/big/g"] {
        c.unlink(None, p).map_err(|e| format!("unlink: {e}"))?;
    }
    kassert!(small.sb.pool.used() == 0 && big.sb.pool.used() == 0, "charges left: {} {}", small.sb.pool.used(), big.sb.pool.used());
    kassert!(pool::system_charged() <= charged, "system pool {} > {charged}", pool::system_charged());
    Ok(())
}
ktest!("fs.cache.pools", Safe, pools);

/// FS-5 fixer: ftruncate/mmap/touch/ftruncate(0) cannot hold frames past the pool bound. A page truncate cuts
/// off while a mapping pins it keeps its pool charge (and frame) until unmapped, so the next new page is refused
/// on a full mount (FS-14: a fault there is SIGBUS); unmapping gives charge and frames back.
fn truncate_mapped() -> TestResult {
    let ns = MountNs::new_empty();
    let c = Ctx::kernel(&ns);
    let m = tmpfs::mount_new(&ns, "/s", 0o1777, Some(4)).map_err(|e| format!("mount: {e}"))?;
    let f = c.open(None, b"/s/f", O_CREAT | O_RDWR, 0o600).map_err(|e| format!("open: {e}"))?;
    let i = f.inode().ok_or("no inode")?.clone();
    let before = pagecache::frames_held();
    let mut maps = Vec::new();
    for round in 0..2 {
        i.truncate(2 * P).map_err(|e| format!("round {round} grow: {e}"))?;
        let map = mmap::check(&f, 3, MAP_SHARED, 0, 2 * P).map_err(|e| format!("map: {e}"))?;
        let pins = [map.backing.get_page(0, true), map.backing.get_page(1, true)];
        let pins = [pins[0].map_err(|e| format!("fault 0: {e:?}"))?, pins[1].map_err(|e| format!("fault 1: {e:?}"))?];
        i.truncate(0).map_err(|e| format!("round {round} shrink: {e}"))?;
        maps.push((map, pins));
    }
    kassert!(m.sb.pool.used() == 4, "cut-off mapped pages keep their charge: {}", m.sb.pool.used());
    i.truncate(2 * P).map_err(|e| format!("grow: {e}"))?;
    let refused = mmap::check(&f, 3, MAP_SHARED, 0, 2 * P).map_err(|e| format!("map: {e}"))?.backing.get_page(0, true);
    kassert!(refused == Err(crate::pagecache::PageFault::Bus), "the pool bound holds for frames still mapped: {refused:?}");
    kassert!(f.pwrite(0, b"x") == Err(ENOSPC), "a write there is ENOSPC");
    kassert!(pagecache::frames_held() >= before + 4, "frames {} before {before}", pagecache::frames_held());
    for (map, pins) in maps.drain(..) {
        kassert!(pins.iter().all(|&p| map.backing.put_page(p)), "put");
    }
    kassert!(m.sb.pool.used() == 0, "charge left after unmapping: {}", m.sb.pool.used());
    drop((f, i));
    c.unlink(None, b"/s/f").map_err(|e| format!("unlink: {e}"))?;
    Ok(())
}
ktest!("fs.cache.truncate_mapped", Safe, truncate_mapped);

/// The Chromium shm pattern at the backing level in the boot view's /dev/shm: create, unlink, ftruncate,
/// two shared mappings fault the same frame, a store through one is what read(2) sees, munmap/munmap/close in
/// a different order each run; the frames go back exactly at the end.
fn shm_pattern() -> TestResult {
    let ns = crate::root_ns();
    let c = Ctx::kernel(&ns);
    let run = RUN.fetch_add(1, Ordering::Relaxed);
    let name = format!("/dev/shm/.org.chromium.Chromium.ktest{run}");
    let before = pagecache::frames_held();
    let f = c.open(None, name.as_bytes(), O_CREAT | O_EXCL | O_RDWR, 0o600).map_err(|e| format!("open: {e}"))?;
    c.unlink(None, name.as_bytes()).map_err(|e| format!("unlink: {e}"))?;
    let pages = 64;
    f.inode().ok_or("no inode")?.truncate(pages * P).map_err(|e| format!("ftruncate: {e}"))?;
    let a = mmap::check(&f, 3, MAP_SHARED, 0, pages * P).map_err(|e| format!("map a: {e}"))?;
    let b = mmap::check(&f, 3, MAP_SHARED, 0, pages * P).map_err(|e| format!("map b: {e}"))?;
    let (mut pa, mut pb) = (Vec::new(), Vec::new());
    for pg in 0..pages {
        let x = a.backing.get_page(pg, true).map_err(|e| format!("fault a {pg}: {e:?}"))?;
        let y = b.backing.get_page(pg, false).map_err(|e| format!("fault b {pg}: {e:?}"))?;
        kassert!(x == y && x % P == 0, "page {pg}: {x:#x} vs {y:#x}");
        pa.push(x);
        pb.push(y);
    }
    kassert!(a.backing.get_page(pages, false) == Err(PageFault::Bus), "past the end must be SIGBUS");
    let cache = f.inode().ok_or("no inode")?.page_cache().ok_or("no cache")?;
    cache.cached(17).ok_or("page 17")?.write(9, b"store via A");
    let mut buf = [0u8; 11];
    kassert!(f.pread(17 * P + 9, &mut buf) == Ok(11) && &buf == b"store via A", "read(2) does not see the store");
    kassert!(pagecache::frames_held() >= before + pages, "frames {} before {before}", pagecache::frames_held());
    let w = Arc::downgrade(f.inode().ok_or("no inode")?);
    let order = [[0, 1, 2], [0, 2, 1], [1, 0, 2], [1, 2, 0], [2, 0, 1], [2, 1, 0]][(run % 6) as usize];
    let (mut a, mut b, mut f) = (Some(a), Some(b), Some(f));
    for (step, who) in order.into_iter().enumerate() {
        match who {
            0 => {
                let m = a.take().ok_or("a twice")?;
                kassert!(pa.iter().all(|&p| m.backing.put_page(p)), "put a");
            }
            1 => {
                let m = b.take().ok_or("b twice")?;
                kassert!(pb.iter().all(|&p| m.backing.put_page(p)), "put b");
            }
            _ => drop(f.take()),
        }
        kassert!(w.upgrade().is_none() == (step == 2), "order {order:?} step {step}: file alive {}", w.upgrade().is_some());
    }
    kassert!(pagecache::frames_held() <= before, "frames {} > {before} after the last user", pagecache::frames_held());
    Ok(())
}
ktest!("fs.cache.shm_pattern", Safe, shm_pattern);

/// FS-15: tmpfs statfs reports Linux's default `nr_inodes` (RAM pages / 2, `shmem_default_max_inodes`) as
/// f_files and lowers f_ffree for each inode in use (nplb PosixStatvfsTest.SunnyDay/.UsageChanges).
fn statfs_inodes() -> TestResult {
    let ns = MountNs::new_empty();
    let c = Ctx::kernel(&ns);
    let m = tmpfs::mount_new(&ns, "/i", 0o1777, None).map_err(|e| format!("mount: {e}"))?;
    let ram = pool::ram_pages().ok_or("no frame allocator")?;
    let st = || crate::statfs::of(&m.sb, false);
    let s0 = st().map_err(|e| format!("statfs: {e}"))?;
    kassert!(s0.files == ram / 2 && s0.files > 0, "f_files {} (RAM {ram} pages)", s0.files);
    kassert!(s0.ffree == s0.files - 1, "fresh mount: f_ffree {} of {}", s0.ffree, s0.files);
    let f = c.open(None, b"/i/f", O_CREAT | O_EXCL | O_RDWR, 0o600).map_err(|e| format!("open: {e}"))?;
    let s1 = st().map_err(|e| format!("statfs: {e}"))?;
    kassert!(s1.ffree + 1 == s0.ffree && s1.files == s0.files, "after create: {} (before {})", s1.ffree, s0.ffree);
    // UsageChanges: 1 MiB of data lowers f_bavail by at least its 256 pages (other tmpfs users may add more).
    kassert!(f.pwrite(0, &vec![b'a'; 256 * P as usize]) == Ok(256 * P as usize), "write 1 MiB");
    let s1b = st().map_err(|e| format!("statfs: {e}"))?;
    kassert!(s1b.bavail + 256 <= s1.bavail && s1b.blocks == s1.blocks, "f_bavail {} after 1 MiB (before {})", s1b.bavail, s1.bavail);
    c.unlink(None, b"/i/f").map_err(|e| format!("unlink: {e}"))?;
    drop(f);
    let s2 = st().map_err(|e| format!("statfs: {e}"))?;
    kassert!(s2.ffree == s0.ffree, "after unlink and close: {} (before {})", s2.ffree, s0.ffree);
    kassert!(s2.bavail >= s1b.bavail + 256, "f_bavail {} after removal (with the file {})", s2.bavail, s1b.bavail);
    Ok(())
}
ktest!("fs.tmpfs.statfs_inodes", Safe, statfs_inodes);
