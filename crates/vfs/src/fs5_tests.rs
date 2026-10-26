//! FS-5 host tests: the page cache through the file calls, file mappings at the backing level (the Chromium
//! shm pattern with every order of munmap/munmap/close), Linux's file checks of mmap, write-back ordering.
//! Only per-mount pools are asserted here (the system pool is process-wide: its own binary
//! `tests/budget_and_freeze.rs` checks it).

extern crate std;

use crate::abi::{O_CREAT, O_DIRECTORY, O_EXCL, O_PATH, O_RDONLY, O_RDWR, O_WRONLY};
use crate::mmap::{self, MapBacking, MAP_PRIVATE, MAP_SHARED, MAP_SHARED_VALIDATE, MAP_SYNC};
use crate::mount_ns::MountNs;
use crate::ops::Ctx;
use crate::pagecache::{Backend, CachePage, PageCache, PageFault, SyncKind};
use crate::tmpfs;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use alloc::{format, vec};
use ksync::SpinLock;
use uapi::errno::*;

const P: u64 = 4096;
const RW: u64 = 3;

fn shm_view() -> (MountNs, Arc<crate::mount_ns::Mount>) {
    let ns = MountNs::new_empty();
    Ctx::kernel(&ns).mkdir(None, b"/dev", 0o755).unwrap();
    let m = tmpfs::mount_new(&ns, "/dev/shm", 0o1777, None).unwrap();
    (ns, m)
}

/// Create in /dev/shm, unlink, size it, map it twice (MAP_SHARED), write through one and read through the
/// other, then munmap/munmap/close in each of the six orders: the inode, its cache pages and the pool charge
/// go exactly when the last of the three ends.
#[test]
fn chromium_shm_pattern_every_order() {
    let orders: [[usize; 3]; 6] = [[0, 1, 2], [0, 2, 1], [1, 0, 2], [1, 2, 0], [2, 0, 1], [2, 1, 0]];
    for order in orders {
        let (ns, m) = shm_view();
        let c = Ctx::kernel(&ns);
        let f = c.open(None, b"/dev/shm/.org.chromium.Chromium.x1", O_CREAT | O_EXCL | O_RDWR, 0o600).unwrap();
        c.unlink(None, b"/dev/shm/.org.chromium.Chromium.x1").unwrap();
        f.inode().unwrap().truncate(3 * P).unwrap();
        assert_eq!(m.sb.pool.used(), 0, "FS-14: a growing ftruncate charges nothing");
        let a = mmap::check(&f, RW, MAP_SHARED, 0, 3 * P).unwrap();
        let b = mmap::check(&f, RW, MAP_SHARED, 0, 3 * P).unwrap();
        assert!(a.shared && a.may_write && b.shared);
        // Both mappings fault page 1 in: one frame.
        let pa = a.backing.get_page(1, true).unwrap();
        let pb = b.backing.get_page(1, false).unwrap();
        assert_eq!(pa, pb, "MAP_SHARED twice maps the same frame");
        // A store through mapping A (the frame itself) is what mapping B and read(2) see.
        let page = f.inode().unwrap().page_cache().unwrap().cached(1).unwrap();
        page.write(100, b"through A");
        let mut buf = [0u8; 9];
        assert_eq!(f.pread(P + 100, &mut buf).unwrap(), 9);
        assert_eq!(&buf, b"through A");
        let w_page = Arc::downgrade(&page);
        drop(page);
        // Page 0 only through B, page 2 nowhere.
        b.backing.get_page(0, true).unwrap();
        assert_eq!(a.backing.get_page(3, false), Err(PageFault::Bus), "past the end: SIGBUS");
        assert_eq!(m.sb.pool.used(), 2, "the two faulted pages are charged");
        let w_inode = Arc::downgrade(f.inode().unwrap());
        let (mut a, mut b, mut f) = (Some(a), Some(b), Some(f));
        for (step, &who) in order.iter().enumerate() {
            match who {
                // munmap of A: MM puts every page it mapped back, then drops the VMA's reference.
                0 => {
                    let m = a.take().unwrap();
                    assert!(m.backing.put_page(pa));
                    assert_eq!(m.backing.pinned(), 0);
                }
                1 => {
                    let m = b.take().unwrap();
                    assert!(m.backing.put_page(pb));
                    let p0 = m.backing.get_page(0, false).unwrap();
                    assert!(m.backing.put_page(p0) && m.backing.put_page(p0));
                    assert!(!m.backing.put_page(p0), "nothing left to put");
                }
                _ => drop(f.take()),
            }
            let last = step == 2;
            assert_eq!(w_inode.upgrade().is_none(), last, "order {order:?} step {step}: the file lives exactly until the last user");
            assert_eq!(w_page.upgrade().is_none(), last, "order {order:?} step {step}: the frame lives as long");
        }
        assert_eq!(m.sb.pool.used(), 0, "the charge went back with the file");
    }
}

/// A page cut off by truncate stays alive while a mapping pins it (its frame is never reused under a program),
/// new faults past the new end are SIGBUS, and the frame goes when the pin does.
#[test]
fn truncate_keeps_pinned_pages_alive() {
    let (ns, m) = shm_view();
    let c = Ctx::kernel(&ns);
    let f = c.open(None, b"/dev/shm/t", O_CREAT | O_RDWR, 0o600).unwrap();
    assert_eq!(f.pwrite(0, &vec![5u8; 2 * P as usize]).unwrap(), 2 * P as usize);
    let map = mmap::check(&f, RW, MAP_SHARED, 0, 2 * P).unwrap();
    let phys = map.backing.get_page(1, false).unwrap();
    let w = Arc::downgrade(&f.inode().unwrap().page_cache().unwrap().cached(1).unwrap());
    f.inode().unwrap().truncate(P).unwrap();
    assert_eq!(m.sb.pool.used(), 2, "the cut-off page is still held by the mapping: it keeps its charge");
    assert!(w.upgrade().is_some(), "still mapped: kept");
    assert_eq!(map.backing.get_page(1, false), Err(PageFault::Bus));
    assert!(map.backing.put_page(phys));
    assert!(w.upgrade().is_none(), "unmapped: freed");
    assert_eq!(m.sb.pool.used(), 1, "and its charge went with it");
    // Growing again reads zeros there (a new page), not the old content.
    f.inode().unwrap().truncate(2 * P).unwrap();
    let mut b = [9u8; 4];
    f.pread(P, &mut b).unwrap();
    assert_eq!(b, [0; 4]);
}

/// Linux's file checks of do_mmap (linux-6.6/mm/mmap.c:1279-1337) in their order.
#[test]
fn mmap_file_checks() {
    let (ns, _m) = shm_view();
    let c = Ctx::kernel(&ns);
    let rw = c.open(None, b"/dev/shm/c", O_CREAT | O_RDWR, 0o644).unwrap();
    rw.inode().unwrap().truncate(P).unwrap();
    let ro = c.open(None, b"/dev/shm/c", O_RDONLY, 0).unwrap();
    let wo = c.open(None, b"/dev/shm/c", O_WRONLY, 0).unwrap();
    let e = |r: Result<mmap::Mapped, i64>| r.err().unwrap_or(0);
    // Shared + PROT_WRITE needs an fd open for writing; any mapping needs one open for reading.
    assert_eq!(e(mmap::check(&ro, RW, MAP_SHARED, 0, P)), EACCES);
    let m = mmap::check(&ro, 1, MAP_SHARED, 0, P).unwrap();
    assert!(!m.shared && !m.may_write, "read-only fd: shared but never writable (mprotect EACCES)");
    let m = mmap::check(&ro, RW, MAP_PRIVATE, 0, P).unwrap();
    assert!(!m.shared && m.may_write, "private copies may be written");
    assert_eq!(e(mmap::check(&wo, 1, MAP_PRIVATE, 0, P)), EACCES);
    assert_eq!(e(mmap::check(&wo, RW, MAP_SHARED, 0, P)), EACCES);
    // Map types and extension flags.
    assert_eq!(e(mmap::check(&rw, RW, 0, 0, P)), EINVAL);
    assert_eq!(e(mmap::check(&rw, RW, 0x0f, 0, P)), EINVAL);
    assert_eq!(e(mmap::check(&rw, RW, MAP_SHARED_VALIDATE | MAP_SYNC, 0, P)), EOPNOTSUPP);
    assert!(mmap::check(&rw, RW, MAP_SHARED | MAP_SYNC, 0, P).is_ok(), "plain MAP_SHARED ignores unknown flags");
    assert!(mmap::check(&rw, RW, MAP_SHARED_VALIDATE, 0, P).unwrap().shared);
    assert_eq!(e(mmap::check(&rw, RW, MAP_PRIVATE | mmap::MAP_GROWSDOWN, 0, P)), EINVAL);
    // file_mmap_ok: offset + length past MAX_LFS_FILESIZE.
    let max_pg = (i64::MAX as u64 - P) >> 12;
    assert!(mmap::check(&rw, RW, MAP_SHARED, max_pg, P).is_ok());
    assert_eq!(e(mmap::check(&rw, RW, MAP_SHARED, max_pg + 1, P)), EOVERFLOW);
    // Objects without mmap: a directory (EACCES-free, readable) and a device are ENODEV; O_PATH is EBADF.
    let d = c.open(None, b"/dev/shm", O_RDONLY | O_DIRECTORY, 0).unwrap();
    assert_eq!(e(mmap::check(&d, 1, MAP_SHARED, 0, P)), ENODEV);
    let dev = crate::fd::OpenFile::new(Arc::new(crate::devfs::MemDev(crate::devfs::Mem::Null)), crate::fd::OpenFlags { read: true, write: true, ..Default::default() });
    assert_eq!(e(mmap::check(&dev, 1, MAP_SHARED, 0, P)), ENODEV);
    let p = c.open(None, b"/dev/shm/c", O_PATH, 0).unwrap();
    assert_eq!(mmap::fd_for_mmap(Some(p)).err(), Some(EBADF));
    assert_eq!(mmap::fd_for_mmap(None).err(), Some(EBADF));
    // A private mapping never marks pages dirty; a frame it does not know is not its own.
    let m = mmap::check(&rw, RW, MAP_PRIVATE, 0, P).unwrap();
    assert!(!m.backing.put_page(0x1234_5000));
    assert!(!m.backing.track_dirty());
}

struct Recorder {
    log: SpinLock<Vec<String>>,
    fail_at: SpinLock<Option<u64>>,
    fail_begin: SpinLock<bool>,
    /// The cache this backend serves: read_page checks it is not called under its data lock.
    cache: SpinLock<Option<alloc::sync::Weak<PageCache>>>,
}

impl Default for Recorder {
    fn default() -> Self {
        Recorder { log: SpinLock::new(Vec::new()), fail_at: SpinLock::new(None), fail_begin: SpinLock::new(false), cache: SpinLock::new(None) }
    }
}

impl Backend for Recorder {
    fn read_page(&self, pgoff: u64, page: &Arc<CachePage>) -> Result<(), i64> {
        let cache = self.cache.lock().as_ref().and_then(|w| w.upgrade());
        if let Some(c) = cache {
            assert!(c.data_lock_free(), "read_page runs with vfs.inode.data held");
        }
        self.log.lock().push(format!("read {pgoff}"));
        page.write(0, &[pgoff as u8 + 1]);
        Ok(())
    }
    fn begin(&self, why: SyncKind) -> Result<(), i64> {
        if *self.fail_begin.lock() {
            self.log.lock().push(format!("begin failed {why:?}"));
            return Err(EIO);
        }
        self.log.lock().push(format!("begin {why:?}"));
        Ok(())
    }
    fn write_run(&self, first: u64, pages: &[Arc<CachePage>], size: u64) -> Result<(), i64> {
        if *self.fail_at.lock() == Some(first) {
            self.log.lock().push(format!("fail {first}"));
            return Err(EIO);
        }
        let mut b = [0u8; 1];
        pages[0].read(0, &mut b);
        self.log.lock().push(format!("run {first}+{} size {size} first byte {}", pages.len(), b[0]));
        Ok(())
    }
    fn commit(&self, why: SyncKind) -> Result<(), i64> {
        self.log.lock().push(format!("commit {why:?}"));
        Ok(())
    }
}

fn take(r: &Recorder) -> Vec<String> {
    core::mem::take(&mut *r.log.lock())
}

/// Write-back through the backend: missing pages are read, dirty pages go back as runs of consecutive pages
/// in file order between begin and commit, a failed run stays dirty (with what follows) and commit still runs.
#[test]
fn writeback_order_and_failures() {
    let rec = Arc::new(Recorder::default());
    let c = PageCache::with_backend(rec.clone(), 0);
    let mut one = [0u8; 1];
    c.truncate(8 * P).unwrap();
    c.read_at(2 * P, &mut one).unwrap();
    assert_eq!(one[0], 3, "a missing page comes from the backend");
    assert_eq!(take(&rec), ["read 2"]);
    for pg in [0u64, 1, 2, 5, 6] {
        c.write_at(pg * P + 1, b"w").unwrap();
    }
    assert_eq!(c.dirty_pages(), 5);
    c.sync_all(SyncKind::Fsync).unwrap();
    assert_eq!(take(&rec), ["read 0", "read 1", "read 5", "read 6", "begin Fsync", "run 0+3 size 32768 first byte 1", "run 5+2 size 32768 first byte 6", "commit Fsync"]);
    assert_eq!(c.dirty_pages(), 0);
    c.sync_all(SyncKind::Fsync).unwrap();
    assert!(take(&rec).is_empty(), "nothing dirty: no write-back at all");
    // A write through a shared mapping marks its page dirty.
    let _ = c.page_for_map(1, true).unwrap();
    c.write_at(6 * P, b"x").unwrap();
    *rec.fail_at.lock() = Some(6);
    assert_eq!(c.sync_range(0, 7, SyncKind::Msync), Err(EIO));
    assert_eq!(take(&rec), ["begin Msync", "run 1+1 size 32768 first byte 2", "fail 6", "commit Msync"]);
    assert_eq!(c.dirty_pages(), 1, "the failed run stays dirty");
    *rec.fail_at.lock() = None;
    c.sync_range(0, 7, SyncKind::Background).unwrap();
    assert_eq!(take(&rec), ["begin Background", "run 6+1 size 32768 first byte 120", "commit Background"]);
    // Ranges: only pages inside are written.
    c.write_at(0, b"a").unwrap();
    c.write_at(7 * P, b"b").unwrap();
    c.sync_range(5, 7, SyncKind::Msync).unwrap();
    assert_eq!(take(&rec), ["read 7", "begin Msync", "run 7+1 size 32768 first byte 98", "commit Msync"], "a partial write reads the page first");
    assert_eq!(c.dirty_pages(), 1);
    assert!(!SyncKind::Background.durable() && SyncKind::Msync.durable());
}

fn backed(size: u64) -> (Arc<Recorder>, Arc<PageCache>) {
    let rec = Arc::new(Recorder::default());
    let c = Arc::new(PageCache::with_backend(rec.clone(), size));
    *rec.cache.lock() = Some(Arc::downgrade(&c));
    (rec, c)
}

/// Fixer finding 1: a failing `begin` writes nothing, commits nothing and leaves every page dirty (it used to
/// clear the flags first and lose the pages).
#[test]
fn writeback_begin_failure_keeps_pages_dirty() {
    let (rec, c) = backed(4 * P);
    for pg in 0..4 {
        c.write_at(pg * P, &vec![7u8; P as usize]).unwrap();
    }
    take(&rec);
    *rec.fail_begin.lock() = true;
    assert_eq!(c.sync_all(SyncKind::Fsync), Err(EIO));
    assert_eq!(take(&rec), ["begin failed Fsync"], "no data and no commit after a failed begin");
    assert_eq!(c.dirty_pages(), 4, "nothing was written: everything stays dirty");
    *rec.fail_begin.lock() = false;
    c.sync_all(SyncKind::Fsync).unwrap();
    assert_eq!(take(&rec), ["begin Fsync", "run 0+4 size 16384 first byte 7", "commit Fsync"]);
    assert_eq!(c.dirty_pages(), 0);
}

/// Fixer finding 2: running out of memory while collecting dirty pages never ends in a silent Ok. With room
/// for some pages the range is written in batches (one begin, one commit); with room for none: ENOMEM and
/// the pages stay dirty.
#[test]
fn writeback_out_of_memory_is_not_silent() {
    use core::sync::atomic::Ordering;
    let (rec, c) = backed(5 * P);
    for pg in 0..5 {
        c.write_at(pg * P, &vec![1u8 + pg as u8; P as usize]).unwrap();
    }
    take(&rec);
    c.batch_cap.store(3, Ordering::Relaxed); // two pages per batch
    c.sync_all(SyncKind::Fsync).unwrap();
    assert_eq!(
        take(&rec),
        ["begin Fsync", "run 0+2 size 20480 first byte 1", "run 2+2 size 20480 first byte 3", "run 4+1 size 20480 first byte 5", "commit Fsync"]
    );
    assert_eq!(c.dirty_pages(), 0);
    for pg in [1u64, 3] {
        c.write_at(pg * P, b"x").unwrap();
    }
    c.batch_cap.store(1, Ordering::Relaxed); // no room at all
    assert_eq!(c.sync_all(SyncKind::Msync), Err(ENOMEM));
    assert!(take(&rec).is_empty(), "nothing collected: no begin, no commit");
    assert_eq!(c.dirty_pages(), 2, "still dirty");
    c.write_at(4 * P, b"y").unwrap();
    c.batch_cap.store(2, Ordering::Relaxed); // one page per batch
    c.sync_all(SyncKind::Msync).unwrap();
    assert_eq!(take(&rec), ["begin Msync", "run 1+1 size 20480 first byte 120", "run 3+1 size 20480 first byte 120", "run 4+1 size 20480 first byte 121", "commit Msync"]);
    assert_eq!(c.dirty_pages(), 0);
}

/// Fixer finding 4: the backend reads with `vfs.inode.data` free (Recorder asserts it), for reads, partial
/// writes, appends and faults; a write covering whole pages, or pages past the old end, reads nothing.
#[test]
fn backend_reads_run_without_the_data_lock() {
    let (rec, c) = backed(3 * P);
    let mut b = [0u8; 2];
    assert_eq!(c.read_at(P - 1, &mut b).unwrap(), 2);
    assert_eq!(b, [0, 2], "page 0 byte P-1 (zero), page 1 byte 0 (its number + 1)");
    assert_eq!(take(&rec), ["read 0", "read 1"]);
    c.write_at(2 * P + 5, b"ab").unwrap();
    assert_eq!(take(&rec), ["read 2"], "a partial write inside the file reads its page first");
    c.write_at(3 * P, &vec![9u8; 2 * P as usize]).unwrap();
    assert!(take(&rec).is_empty(), "whole pages past the old end: nothing to read");
    c.truncate(6 * P).unwrap();
    c.write_at(5 * P, &vec![4u8; P as usize]).unwrap();
    assert!(take(&rec).is_empty(), "a whole-page write never reads");
    c.truncate(6 * P + 10).unwrap();
    let (n, end) = c.append(b"tail", u64::MAX >> 1).unwrap();
    assert_eq!((n, end), (4, 6 * P + 14));
    assert_eq!(take(&rec), ["read 6"], "append into a partial last page reads it");
    // A fault on a page not cached yet.
    let (rec, c) = backed(2 * P);
    let p = c.page_for_map(1, false).unwrap();
    let mut one = [0u8; 1];
    p.read(0, &mut one);
    assert_eq!((take(&rec), one[0]), (vec![String::from("read 1")], 2));
    assert!(Arc::ptr_eq(&p, &c.page_for_map(1, true).unwrap()), "cached now: the same page, no second read");
    assert!(take(&rec).is_empty());
}

/// Fixer finding 3: ftruncate/mmap/touch/ftruncate(0) in a loop cannot hold frames beyond the pool bound: the
/// cut-off pages a mapping still holds keep their charge, so the next fault is refused (FS-14: pages are
/// charged when allocated, the refused fault is SIGBUS as shmem_fault answers ENOSPC), and unmapping gives
/// everything back.
#[test]
fn truncated_mapped_pages_stay_charged() {
    let (ns, m) = shm_view();
    let c = Ctx::kernel(&ns);
    let f = c.open(None, b"/dev/shm/loop", O_CREAT | O_RDWR, 0o600).unwrap();
    let pool = &m.sb.pool;
    pool.set_size(Some(4));
    let inode = f.inode().unwrap().clone();
    let mut maps = Vec::new();
    let mut refused = None;
    for round in 0..10 {
        inode.truncate(2 * P).unwrap();
        let map = mmap::check(&f, RW, MAP_SHARED, 0, 2 * P).unwrap();
        let first = match map.backing.get_page(0, true) {
            Ok(p) => p,
            Err(e) => {
                refused = Some((round, e));
                break;
            }
        };
        let pins = [first, map.backing.get_page(1, true).unwrap()];
        inode.truncate(0).unwrap();
        maps.push((map, pins));
    }
    assert_eq!(refused, Some((2, PageFault::Bus)), "two rounds fill the 4-page pool; the third round's fault is refused");
    assert_eq!(pool.used(), 4);
    assert!(crate::mmap::pinned_pages() >= 4);
    for (map, pins) in maps.drain(..) {
        for p in pins {
            assert!(map.backing.put_page(p));
        }
    }
    assert_eq!(pool.used(), 0, "unmapped: every carried charge went back");
    inode.truncate(4 * P).unwrap();
    assert_eq!(pool.used(), 0, "growing charges nothing");
    pool.set_size(None);
}

/// Fixer finding 5, FS-14: a 1-byte write far past the end costs one page (holes are free); a write that
/// partly fits still gets exactly what the pool has.
#[test]
fn far_write_does_not_drain_the_pool() {
    let (ns, m) = shm_view();
    let c = Ctx::kernel(&ns);
    let f = c.open(None, b"/dev/shm/far", O_CREAT | O_RDWR, 0o600).unwrap();
    let pool = &m.sb.pool;
    pool.set_size(Some(1 << 20));
    assert_eq!(f.pwrite(1 << 40, b"z").unwrap(), 1);
    assert_eq!(pool.used(), 1);
    f.inode().unwrap().truncate(0).unwrap();
    assert_eq!(pool.used(), 0);
    pool.set_size(Some(1000));
    assert_eq!(pool.charge_upto(1, 5000), (1000, Some(ENOSPC)), "halving finds the exact rest");
    pool.uncharge(1000);
    assert_eq!(pool.charge_upto(1001, 5000), (0, Some(ENOSPC)), "the least it needs does not fit: nothing");
    assert_eq!(pool.used(), 0);
    let big = vec![3u8; 1200 * P as usize];
    assert_eq!(f.pwrite(10 * P, &big).unwrap(), 1000 * P as usize, "short write up to the pool's end (holes are free)");
    assert_eq!(pool.used(), 1000);
    pool.set_size(None);
}

/// The background flusher's registry: live objects are flushed, dead ones pruned.
#[test]
fn writeback_registry() {
    struct Counting(SpinLock<u32>);
    impl crate::writeback::Flush for Counting {
        fn flush(&self, _why: SyncKind) -> Result<(), i64> {
            *self.0.lock() += 1;
            Ok(())
        }
    }
    let a: Arc<dyn crate::writeback::Flush> = Arc::new(Counting(SpinLock::new(0)));
    let gone: Arc<dyn crate::writeback::Flush> = Arc::new(Counting(SpinLock::new(0)));
    crate::writeback::register(Arc::downgrade(&a)).unwrap();
    crate::writeback::register(Arc::downgrade(&gone)).unwrap();
    drop(gone);
    let p = crate::writeback::flush_all(SyncKind::Background);
    assert!(p.flushed >= 1 && p.failed == 0);
}

/// Sizes through the file calls: O_TRUNC, ftruncate growth and shrink, writes past the end; FS-14: the mount's
/// pool is charged for the pages really allocated, and st_blocks counts them.
#[test]
fn charges_follow_the_size() {
    let (ns, m) = shm_view();
    let c = Ctx::kernel(&ns);
    let f = c.open(None, b"/dev/shm/s", O_CREAT | O_RDWR, 0o600).unwrap();
    let pool = &m.sb.pool;
    f.pwrite(10 * P + 5, b"z").unwrap();
    assert_eq!(pool.used(), 1);
    assert_eq!(f.inode().unwrap().attr(0, 0).blocks, 8, "one frame held: 8 blocks of 512");
    f.inode().unwrap().truncate(P / 2).unwrap();
    assert_eq!(pool.used(), 0);
    let g = c.open(None, b"/dev/shm/s", O_RDWR | crate::abi::O_TRUNC, 0).unwrap();
    assert_eq!((pool.used(), g.file.size()), (0, 0));
    pool.set_size(Some(2));
    assert_eq!(f.pwrite(0, &vec![1u8; 3 * P as usize]).unwrap(), 2 * P as usize);
    assert_eq!(f.pwrite(2 * P, b"q").err(), Some(ENOSPC));
    pool.set_size(None);
    assert_eq!(f.pwrite(2 * P, b"q").unwrap(), 1);
    assert_eq!(pool.used(), 3);
}
