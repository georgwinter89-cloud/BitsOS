//! FS-W2 host tests: `PageCache::write_direct` (`cache_around`), the call that lets a backend's storage be written
//! beside its cache. The storage here is a "disk" of one byte per page; a read takes its byte when the device
//! call begins and may then be held (a slow read), a write-back takes the page's byte and may be held before it
//! lands (a write at the device). Each stale-page case is judged by order and by the bytes left on the disk and in
//! the cache; with `write_direct` reduced to `write()` eight of the first ten fail (checked, see the notes; the two
//! that pass show what must not get worse: reads elsewhere, batches).
extern crate std;

use crate::pagecache::{Backend, CachePage, PageCache, SyncKind};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use core::time::Duration;
use ksync::SpinLock;
use uapi::errno::EIO;

const P: u64 = 4096;
const PAGES: u64 = 64;

struct Disk {
    bytes: SpinLock<Vec<u8>>,
    /// While set, a read that took its bytes waits before it gives them to the cache; so a write-back before it lands.
    hold_reads: AtomicBool,
    hold_writes: AtomicBool,
    reading: AtomicU32,
    writing: AtomicU32,
    /// (first page, pages) of every read and write-back, in order.
    reads: SpinLock<Vec<(u64, u64)>>,
    writes: SpinLock<Vec<(u64, u64)>>,
    fail_writes: AtomicBool,
    window: u64,
    /// Ticks of what landed on the disk, for "who was first".
    clock: AtomicU64,
    last_write_back: AtomicU64,
}

impl Disk {
    fn new(window: u64) -> Arc<Disk> {
        Arc::new(Disk {
            bytes: SpinLock::new((0..PAGES as u8).collect()),
            hold_reads: AtomicBool::new(false),
            hold_writes: AtomicBool::new(false),
            reading: AtomicU32::new(0),
            writing: AtomicU32::new(0),
            reads: SpinLock::new(Vec::new()),
            writes: SpinLock::new(Vec::new()),
            fail_writes: AtomicBool::new(false),
            window,
            clock: AtomicU64::new(1),
            last_write_back: AtomicU64::new(0),
        })
    }
    fn at(&self, page: u64) -> u8 {
        self.bytes.lock()[page as usize]
    }
    /// The direct write: the bytes of `pages` on the disk, at once.
    fn put(&self, pages: core::ops::RangeInclusive<u64>, b: u8) -> u64 {
        let mut d = self.bytes.lock();
        for p in pages {
            d[p as usize] = b;
        }
        self.clock.fetch_add(1, Ordering::AcqRel)
    }
}

fn wait_while(flag: &AtomicBool) {
    while flag.load(Ordering::Acquire) {
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn wait_for(n: &AtomicU32) {
    for _ in 0..10_000 {
        if n.load(Ordering::Acquire) > 0 {
            return;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    panic!("the device call never began");
}

impl Backend for Disk {
    fn read_page(&self, pgoff: u64, page: &Arc<CachePage>) -> Result<(), i64> {
        self.read_pages(pgoff, core::slice::from_ref(page))
    }
    fn read_pages(&self, first: u64, pages: &[Arc<CachePage>]) -> Result<(), i64> {
        self.reads.lock().push((first, pages.len() as u64));
        let got: Vec<u8> = self.bytes.lock()[first as usize..first as usize + pages.len()].to_vec();
        self.reading.fetch_add(1, Ordering::AcqRel);
        wait_while(&self.hold_reads);
        self.reading.fetch_sub(1, Ordering::AcqRel);
        for (p, b) in pages.iter().zip(got) {
            p.write(0, &[b]);
        }
        Ok(())
    }
    fn write_run(&self, first: u64, pages: &[Arc<CachePage>], _size: u64) -> Result<(), i64> {
        if self.fail_writes.load(Ordering::Acquire) {
            return Err(EIO);
        }
        self.writes.lock().push((first, pages.len() as u64));
        let mut got = alloc::vec![0u8; pages.len()];
        for (p, b) in pages.iter().zip(got.iter_mut()) {
            p.read(0, core::slice::from_mut(b));
        }
        self.writing.fetch_add(1, Ordering::AcqRel);
        wait_while(&self.hold_writes);
        self.bytes.lock()[first as usize..first as usize + pages.len()].copy_from_slice(&got);
        self.last_write_back.store(self.clock.fetch_add(1, Ordering::AcqRel), Ordering::Release);
        self.writing.fetch_sub(1, Ordering::AcqRel);
        Ok(())
    }
    fn read_window(&self) -> u64 {
        self.window
    }
}

fn cache(window: u64) -> (Arc<PageCache>, Arc<Disk>) {
    let d = Disk::new(window);
    (Arc::new(PageCache::with_backend(d.clone(), PAGES * P)), d)
}

fn byte(c: &PageCache, page: u64) -> u8 {
    let mut b = [0u8; 1];
    assert_eq!(c.read_at(page * P, &mut b), Ok(1));
    b[0]
}

/// A page a read left in the cache (a block that was metadata, or file data read through the disk's cache) must
/// not be what the next read gives once the block was written beside the cache.
#[test]
fn a_clean_page_of_the_range_is_not_read_instead_of_the_new_bytes() {
    let (c, d) = cache(1);
    assert_eq!((byte(&c, 3), byte(&c, 4), byte(&c, 9)), (3, 4, 9));
    c.write_direct(3, 4, || {
        d.put(3..=4, 0xAA);
        Ok(())
    })
    .unwrap();
    assert!(c.cached(3).is_none() && c.cached(4).is_none(), "the old pages stayed in the cache");
    assert!(c.cached(9).is_some(), "a page outside the range went too");
    assert_eq!((byte(&c, 3), byte(&c, 4), byte(&c, 9)), (0xAA, 0xAA, 9));
    assert_eq!(c.dirty_pages(), 0);
}

/// A dirty page of the range (an old home write of a block that was metadata; the zeroed edge of a truncated
/// file) is written out before the direct write, never after it and never thrown away.
#[test]
fn a_dirty_page_of_the_range_reaches_the_disk_before_the_direct_write_and_never_after() {
    let (c, d) = cache(1);
    assert_eq!(c.write_at(5 * P, &[0x55; P as usize]), Ok(P as usize));
    assert_eq!(c.write_at(20 * P, &[0x77; P as usize]), Ok(P as usize));
    // This cache's own counts: the process-wide dirty total moves with every other test that runs beside this one.
    assert_eq!((c.dirty_pages(), c.own_dirty()), (2, 2));
    c.write_direct(5, 5, || {
        assert_eq!(d.at(5), 0x55, "the dirty page was not written out before the direct write");
        d.put(5..=5, 0xAA);
        Ok(())
    })
    .unwrap();
    assert_eq!(d.at(20), 20, "a dirty page outside the range was written for it");
    assert_eq!(c.dirty_pages(), 1);
    assert_eq!(c.own_dirty(), 1);
    // The cache's next write-back has nothing old for the block.
    c.sync_all(SyncKind::Fsync).unwrap();
    assert_eq!((d.at(5), d.at(20)), (0xAA, 0x77));
    assert_eq!(byte(&c, 5), 0xAA);
}

/// A write-back that took the page before the call (the page is clean while it is at the device) lands first.
#[test]
fn a_write_back_at_the_device_lands_before_the_direct_write() {
    let (c, d) = cache(1);
    assert_eq!(c.write_at(7 * P, &[0x55; P as usize]), Ok(P as usize));
    d.hold_writes.store(true, Ordering::Release);
    let ran = Arc::new(AtomicU64::new(0));
    std::thread::scope(|s| {
        let c1 = c.clone();
        let back = s.spawn(move || c1.sync_range(7, 7, SyncKind::Background));
        wait_for(&d.writing);
        assert!(!c.cached(7).is_some_and(|p| p.is_dirty()), "the page at the device still counts as dirty: the test shows nothing");
        let (c2, d2, ran2) = (c.clone(), d.clone(), ran.clone());
        let direct = s.spawn(move || {
            c2.write_direct(7, 7, || {
                ran2.store(d2.put(7..=7, 0xAA), Ordering::Release);
                Ok(())
            })
        });
        std::thread::sleep(Duration::from_millis(100));
        let early = ran.load(Ordering::Acquire);
        d.hold_writes.store(false, Ordering::Release);
        assert_eq!(back.join().unwrap(), Ok(()));
        assert_eq!(direct.join().unwrap(), Ok(()));
        assert_eq!(early, 0, "the direct write ran while a write-back of its page was at the device");
    });
    assert!(ran.load(Ordering::Acquire) > d.last_write_back.load(Ordering::Acquire), "the old page landed after the direct write");
    assert_eq!(d.at(7), 0xAA, "the old page is on the disk");
    assert_eq!(byte(&c, 7), 0xAA);
}

/// A read of the block that took the old bytes from the device before the direct write ended and comes back after
/// it: what it read is not kept. Also a neighbour's read whose window reaches into the range.
#[test]
fn a_fill_that_read_old_bytes_inserts_nothing_after_the_direct_write() {
    for (window, reader) in [(1u64, 9u64), (8, 6)] {
        let (c, d) = cache(window);
        d.hold_reads.store(true, Ordering::Release);
        std::thread::scope(|s| {
            let c1 = c.clone();
            let read = s.spawn(move || (byte(&c1, reader), byte(&c1, 9)));
            wait_for(&d.reading);
            // The reader holds the old byte of page 9 (its own page, or one of its window's).
            c.write_direct(9, 9, || {
                d.put(9..=9, 0xAA);
                Ok(())
            })
            .unwrap();
            d.hold_reads.store(false, Ordering::Release);
            let (own, nine) = read.join().unwrap();
            assert_eq!(nine, 0xAA, "window {window}: the read gave the bytes from before the direct write");
            assert_eq!(own, if reader == 9 { 0xAA } else { reader as u8 });
        });
        assert_eq!(byte(&c, 9), 0xAA, "window {window}: the old page is in the cache");
        assert!(d.reads.lock().len() >= 2, "window {window}: the reader did not read again");
    }
}

/// A read that begins while the direct write runs may see the old bytes or the new ones; what it leaves in the
/// cache does not outlive the call.
#[test]
fn a_fill_during_the_direct_write_leaves_no_page_behind() {
    let (c, d) = cache(1);
    c.write_direct(11, 12, || {
        assert_eq!(byte(&c, 11), 11);
        assert!(c.cached(11).is_some());
        d.put(11..=12, 0xAA);
        Ok(())
    })
    .unwrap();
    assert!(c.cached(11).is_none(), "a page read during the direct write stayed");
    assert_eq!((byte(&c, 11), byte(&c, 12)), (0xAA, 0xAA));
}

/// Fills that do not touch the range keep what they read (no cache-wide flush of running reads per data write).
#[test]
fn a_fill_elsewhere_keeps_its_read() {
    let (c, d) = cache(1);
    d.hold_reads.store(true, Ordering::Release);
    std::thread::scope(|s| {
        let c1 = c.clone();
        let read = s.spawn(move || byte(&c1, 40));
        wait_for(&d.reading);
        c.write_direct(9, 9, || {
            d.put(9..=9, 0xAA);
            Ok(())
        })
        .unwrap();
        d.hold_reads.store(false, Ordering::Release);
        assert_eq!(read.join().unwrap(), 40);
    });
    let reads = d.reads.lock().clone();
    assert_eq!(reads.len(), 1, "a read outside the range was thrown away: {reads:?}");
}

/// Fixer round: with a fill of the range running too, only that one reads again. (The first form moved the cache's
/// epoch then: every metadata read in flight started over for each data write that met one read.)
#[test]
fn only_the_fill_of_the_range_reads_again() {
    let (c, d) = cache(1);
    d.hold_reads.store(true, Ordering::Release);
    std::thread::scope(|s| {
        let (c1, c2) = (c.clone(), c.clone());
        let near = s.spawn(move || byte(&c1, 9));
        let far = s.spawn(move || byte(&c2, 40));
        for _ in 0..10_000 {
            if d.reading.load(Ordering::Acquire) == 2 {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(d.reading.load(Ordering::Acquire), 2, "the two reads never were at the device together");
        c.write_direct(9, 9, || {
            d.put(9..=9, 0xAA);
            Ok(())
        })
        .unwrap();
        assert_eq!(c.direct_log().0, 1, "the write that met a read is not remembered for it");
        d.hold_reads.store(false, Ordering::Release);
        assert_eq!((near.join().unwrap(), far.join().unwrap()), (0xAA, 40));
    });
    let reads = d.reads.lock().clone();
    assert_eq!(reads.iter().filter(|r| r.0 == 40).count(), 1, "the read outside the range was thrown away: {reads:?}");
    assert_eq!(reads.iter().filter(|r| r.0 == 9).count(), 2, "the read of the range did not read again: {reads:?}");
    // What no running fill reads is not kept: the list does not grow with a download.
    c.write_direct(20, 21, || Ok(())).unwrap();
    assert_eq!(c.direct_log(), (0, 0));
}

/// A fill that could not note its pages (no memory for the list) is counted instead; while it runs every direct
/// write is remembered, so it reads again only when one touched its own pages, however many ended elsewhere
/// (the first form compared a count of all ended writes: under a download such a fill never got through).
#[test]
fn a_fill_without_a_note_reads_again_only_for_its_own_pages() {
    let (c, d) = cache(8);
    let mark = c.direct_blind_mark();
    assert_eq!(c.direct_log(), (0, 1));
    for p in 30..40 {
        c.write_direct(p, p, || {
            d.put(p..=p, 0xAA);
            Ok(())
        })
        .unwrap();
    }
    assert!(c.direct_fresh(&mark, 8, 16), "direct writes elsewhere made the fill read again");
    assert!(!c.direct_fresh(&mark, 28, 31), "a direct write of the fill's pages did not");
    c.write_direct(15, 15, || Ok(())).unwrap();
    assert!(!c.direct_fresh(&mark, 8, 16));
    assert!(c.direct_fresh(&mark, 8, 15) && c.direct_fresh(&mark, 16, 20));
    // A fill that begins now read after those writes.
    let later = c.direct_blind_mark();
    assert!(c.direct_fresh(&later, 8, 16));
    assert_eq!(c.direct_log(), (11, 2));
    drop(mark);
    drop(later);
    c.write_direct(50, 50, || Ok(())).unwrap();
    assert_eq!(c.direct_log(), (0, 0), "the list outlived the fills it was kept for");
}

/// While the direct write runs no write-back begins on its pages, whoever dirtied one meanwhile; such a page is
/// dropped at the end (the direct bytes are the block's owner's).
#[test]
fn no_write_back_begins_on_the_range_while_the_direct_write_runs() {
    let (c, d) = cache(1);
    c.write_direct(13, 14, || {
        // Against the caller's rule: somebody writes the block through the cache now.
        assert_eq!(c.write_at(13 * P, &[0x55; P as usize]), Ok(P as usize));
        assert_eq!(c.write_at(30 * P, &[0x66; P as usize]), Ok(P as usize));
        c.sync_all(SyncKind::Background).unwrap();
        assert_eq!(d.at(30), 0x66, "the write-back did not run at all");
        assert_eq!(d.at(13), 13, "a write-back took a page of the range during the direct write");
        d.put(13..=14, 0xAA);
        Ok(())
    })
    .unwrap();
    c.sync_all(SyncKind::Fsync).unwrap();
    assert_eq!(d.at(13), 0xAA, "the page dirtied through the cache was written over the direct bytes");
    assert_eq!(byte(&c, 13), 0xAA);
    assert_eq!((c.dirty_pages(), c.own_dirty()), (0, 0));
}

/// A failed direct write leaves the cache without pages of the range (the disk's bytes are the device's affair);
/// a dirty page that cannot be written out first fails the call before `write` runs and stays dirty.
#[test]
fn errors_leave_the_range_coherent() {
    let (c, d) = cache(1);
    assert_eq!(byte(&c, 3), 3);
    assert_eq!(c.write_direct(3, 3, || Err(EIO)), Err(EIO));
    assert!(c.cached(3).is_none());
    assert_eq!(byte(&c, 3), 3);

    assert_eq!(c.write_at(5 * P, &[0x55; P as usize]), Ok(P as usize));
    d.fail_writes.store(true, Ordering::Release);
    let mut ran = false;
    assert_eq!(c.write_direct(5, 5, || {
        ran = true;
        Ok(())
    }), Err(EIO));
    assert!(!ran, "the direct write ran although the old dirty page could not be written out");
    assert!(c.cached(5).is_some_and(|p| p.is_dirty()), "the page whose write failed is not dirty any more");
    d.fail_writes.store(false, Ordering::Release);
    c.write_direct(5, 5, || {
        d.put(5..=5, 0xAA);
        Ok(())
    })
    .unwrap();
    assert_eq!(byte(&c, 5), 0xAA);
    // A cache without a backend has no storage beside it.
    let t = PageCache::tmpfs(Arc::new(crate::pool::Pool::new("fsw2")));
    assert_eq!(t.write_direct(0, 0, || Ok(())), Err(uapi::errno::EINVAL));
}

/// Many direct writes beside readers and write-backs of other pages: every page ends with its last writer's byte
/// on the disk and in the cache.
#[test]
fn parallel_direct_writes_readers_and_write_backs_agree_at_the_end() {
    let (c, d) = cache(4);
    std::thread::scope(|s| {
        for t in 0..4u64 {
            let (c, d) = (c.clone(), d.clone());
            s.spawn(move || {
                for round in 0..200u64 {
                    let first = t * 8 + round % 4;
                    c.write_direct(first, first + 1, || {
                        d.put(first..=first + 1, 0x80 | (round % 128) as u8);
                        Ok(())
                    })
                    .unwrap();
                }
            });
        }
        for _ in 0..2 {
            let c = c.clone();
            s.spawn(move || {
                for i in 0..2000u64 {
                    let _ = byte(&c, i * 7 % 40);
                }
            });
        }
        let c2 = c.clone();
        s.spawn(move || {
            for i in 0..200u64 {
                assert_eq!(c2.write_at((48 + i % 8) * P, &[0x33; 1]), Ok(1));
                c2.sync_all(SyncKind::Background).unwrap();
            }
        });
    });
    c.sync_all(SyncKind::Fsync).unwrap();
    for p in 0..PAGES {
        // The last round that wrote each of a writer's five pages.
        let want = if p < 32 && p % 8 < 5 { 0x80 | ([196u64, 197, 198, 199, 199][(p % 8) as usize] % 128) as u8 } else if (48..56).contains(&p) { 0x33 } else { p as u8 };
        assert_eq!(d.at(p), want, "page {p} on the disk");
        assert_eq!(byte(&c, p), want, "page {p} through the cache");
    }
}

/// A write-back takes its dirty pages in batches where the backend asks for it (FS-W2: a page in a batch is not
/// counted dirty and not written yet); every page is written once all the same. Without the bound a run is one
/// call, as before.
#[test]
fn a_long_dirty_run_is_written_in_batches() {
    struct Count(SpinLock<Vec<u64>>, usize);
    impl Backend for Count {
        fn batch_pages(&self) -> usize {
            self.1
        }
        fn read_page(&self, _pgoff: u64, _page: &Arc<CachePage>) -> Result<(), i64> {
            Ok(())
        }
        fn write_run(&self, _first: u64, pages: &[Arc<CachePage>], _size: u64) -> Result<(), i64> {
            self.0.lock().push(pages.len() as u64);
            Ok(())
        }
    }
    for most in [1024usize, usize::MAX] {
        let b = Arc::new(Count(SpinLock::new(Vec::new()), most));
        let c = PageCache::with_backend(b.clone(), 0);
        let chunk = alloc::vec![7u8; 64 * P as usize];
        for i in 0..50u64 {
            assert_eq!(c.write_at(i * 64 * P, &chunk), Ok(chunk.len()));
        }
        assert_eq!(c.dirty_pages(), 3200);
        c.sync_all(SyncKind::Background).unwrap();
        let runs = b.0.lock().clone();
        assert_eq!(runs.iter().sum::<u64>(), 3200);
        if most == 1024 {
            assert_eq!(runs, [1024, 1024, 1024, 128], "the batches of a bounded backend");
        } else {
            assert_eq!(runs, [3200], "an unbounded backend's run was split");
        }
        assert_eq!(c.dirty_pages(), 0);
    }
}
