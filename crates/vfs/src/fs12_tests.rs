//! FS-12 host tests: read-ahead windows (`readahead`). One test function: queuing is a process-wide switch.

extern crate std;

use crate::pagecache::{Advice, Backend, CachePage, MapHint, PageCache};
use crate::readahead::Window;
use crate::readahead;
use alloc::sync::Arc;
use alloc::vec::Vec;
use ksync::SpinLock;

const P: u64 = 4096;

struct Pages {
    reads: SpinLock<Vec<u64>>,
}

impl Default for Pages {
    fn default() -> Self {
        Pages { reads: SpinLock::new(Vec::new()) }
    }
}

impl Backend for Pages {
    fn read_page(&self, pgoff: u64, page: &Arc<CachePage>) -> Result<(), i64> {
        self.reads.lock().push(pgoff);
        page.write(0, &[pgoff as u8 ^ 0x5A]);
        Ok(())
    }
    fn write_run(&self, _first: u64, _pages: &[Arc<CachePage>], _size: u64) -> Result<(), i64> {
        Ok(())
    }
    fn read_ahead_pages(&self) -> u64 {
        32
    }
}

fn take(b: &Pages) -> Vec<u64> {
    core::mem::take(&mut *b.reads.lock())
}

fn byte(c: &PageCache, pg: u64) -> u8 {
    let mut one = [0u8; 1];
    assert_eq!(c.read_at(pg * P, &mut one), Ok(1));
    one[0]
}

fn pread(c: &PageCache, pg: u64, pages: u64) {
    let mut buf = alloc::vec![0u8; (pages * P) as usize];
    assert_eq!(c.read_at(pg * P, &mut buf), Ok(buf.len()));
    for k in 0..pages {
        assert_eq!(buf[(k * P) as usize], (pg + k) as u8 ^ 0x5A, "page {}", pg + k);
    }
}

#[test]
fn read_ahead_windows() {
    // BL-4: the reader's miss reads its window itself, also without workers (linux-7/mm/readahead.c:615: a miss at
    // the file start, 1 page asked: init 4 pages, the mark on page 1); no job is queued.
    let b = Arc::new(Pages::default());
    let c = PageCache::with_backend(b.clone(), 128 * P);
    assert_eq!(byte(&c, 0), 0x5A);
    assert_eq!(take(&b), [0, 1, 2, 3]);
    assert_eq!(byte(&c, 1), 1 ^ 0x5A);
    assert_eq!(readahead::pending(), 0, "no workers: the mark queues nothing");

    readahead::set_active(true);
    let b = Arc::new(Pages::default());
    let c = PageCache::with_backend(b.clone(), 128 * P);
    assert_eq!(byte(&c, 0), 0x5A);
    assert_eq!(take(&b), [0, 1, 2, 3]);
    assert_eq!(readahead::run_pending(), 0, "the first window is the reader's");
    // Page 1 is the mark: the next window (get_next_ra_size: 8), then 16, then 32 (the backend's maximum), each
    // with its mark on its first page.
    for (mark, first, end) in [(1u64, 4u64, 12u64), (4, 12, 28), (12, 28, 60), (28, 60, 92)] {
        assert_eq!(byte(&c, mark), mark as u8 ^ 0x5A);
        assert!(take(&b).is_empty(), "page {mark} was read ahead");
        assert_eq!(readahead::run_pending(), 1, "mark {mark}");
        assert_eq!(take(&b), (first..end).collect::<Vec<_>>(), "mark {mark}");
    }
    // A reader faster than the job: page 60 (the mark) queues [92, 124); the reader misses 92 before it ran and
    // reads the window itself (32 cached pages before it: a history window with its mark on its last page, 123).
    // It reaches that mark while the job still waits: the job finds nothing left to read and opens the next
    // window when it ends, cut at the file end and without a mark ([124, 128)).
    assert_eq!(byte(&c, 60), 60 ^ 0x5A);
    assert!(take(&b).is_empty());
    assert_eq!(byte(&c, 92), 92 ^ 0x5A);
    assert_eq!(take(&b), (92..124).collect::<Vec<_>>());
    assert_eq!(byte(&c, 123), 123 ^ 0x5A);
    assert_eq!(readahead::run_pending(), 2);
    assert_eq!(take(&b), (124..128).collect::<Vec<_>>());
    assert_eq!(byte(&c, 124), 124 ^ 0x5A);
    assert_eq!(readahead::run_pending(), 0, "no mark past the end");

    // A standalone random read is read as is (readahead.c:630): no window, no job; the next page then is a
    // sequential miss (where the last read ended) and gets the initial window.
    let b = Arc::new(Pages::default());
    let c = PageCache::with_backend(b.clone(), 128 * P);
    assert_eq!(byte(&c, 50), 50 ^ 0x5A);
    assert_eq!(take(&b), [50]);
    pread(&c, 51, 1);
    assert_eq!(take(&b), [51, 52, 53, 54]);
    assert_eq!(readahead::run_pending(), 0);
    // A miss after cached history (not where the last read ended): the window is history + request (async 1, so
    // its mark is its last page, readahead.c:647-650).
    pread(&c, 60, 1);
    assert_eq!(take(&b), [60], "no history before 60");
    pread(&c, 61, 1);
    assert_eq!(take(&b), [61, 62, 63, 64]);
    pread(&c, 70, 1);
    assert_eq!(take(&b), [70]);
    // 60..=64 cached before 65: a window of 5 + 1 from 65, cut where 70 is cached; its mark is its last page (70).
    pread(&c, 65, 1);
    assert_eq!(take(&b), [65, 66, 67, 68, 69]);
    // That mark: the next window ramps from 6 (get_next_ra_size: 12) and its end is aligned down to 4 pages
    // (71 + 12 = 83 -> 80).
    assert_eq!(byte(&c, 70), 70 ^ 0x5A);
    assert_eq!(readahead::run_pending(), 1);
    assert_eq!(take(&b), (71..80).collect::<Vec<_>>());

    // A truncate while a job waits: the job inserts nothing.
    let b = Arc::new(Pages::default());
    let c = PageCache::with_backend(b.clone(), 64 * P);
    assert_eq!(byte(&c, 0), 0x5A);
    assert_eq!(byte(&c, 1), 1 ^ 0x5A);
    take(&b);
    c.truncate(2 * P).unwrap();
    c.truncate(64 * P).unwrap();
    assert_eq!(readahead::run_pending(), 1);
    assert!(c.cached(5).is_none(), "a page read before the truncate is not inserted");
    // A dropped file: the job stops. Pages 2 and 3 are a miss inside the read: initial window [2, 6), mark 4.
    pread(&c, 0, 4);
    assert_eq!(take(&b), [2, 3, 4, 5]);
    assert_eq!(byte(&c, 4), 4 ^ 0x5A);
    drop(c);
    take(&b);
    assert_eq!(readahead::run_pending(), 1);
    assert!(take(&b).is_empty(), "nothing read for a dropped file");

    // posix_fadvise(WILLNEED): the range is queued for a worker, the call reads nothing itself.
    let b = Arc::new(Pages::default());
    let c = PageCache::with_backend(b.clone(), 16 * P);
    c.will_need(2 * P, 3 * P);
    assert!(take(&b).is_empty());
    assert_eq!(readahead::run_pending(), 1);
    assert_eq!(take(&b), [2, 3, 4]);
    c.will_need(14 * P, 0);
    assert_eq!(readahead::run_pending(), 1);
    assert_eq!(take(&b), [14, 15], "len 0: to the end of the file");
    c.will_need(16 * P, 4 * P);
    assert_eq!(readahead::run_pending(), 0, "past the end: nothing");

    // POSIX_FADV_RANDOM: a miss reads what it asks for, even at the file start; NORMAL brings the window back.
    let b = Arc::new(Pages::default());
    let c = PageCache::with_backend(b.clone(), 256 * P);
    c.advise(Advice::Random);
    assert_eq!(byte(&c, 0), 0x5A);
    assert_eq!(take(&b), [0]);
    pread(&c, 1, 2);
    assert_eq!(take(&b), [1, 2]);
    assert_eq!(readahead::run_pending(), 0);
    c.advise(Advice::Normal);
    pread(&c, 3, 1);
    assert_eq!(take(&b), [3, 4, 5, 6]);
    // POSIX_FADV_SEQUENTIAL: twice the backend's window (64): the ramp goes on past 32.
    c.advise(Advice::Sequential);
    for (mark, first, end) in [(4u64, 7u64, 12u64), (7, 12, 20), (12, 20, 36), (20, 36, 68), (36, 68, 132)] {
        assert_eq!(byte(&c, mark), mark as u8 ^ 0x5A);
        assert_eq!(readahead::run_pending(), 1, "mark {mark}");
        assert_eq!(take(&b), (first..end).collect::<Vec<_>>(), "mark {mark}");
    }
    // A mapping's fault on the read-around window's mark (do_async_mmap_readahead): the next window is a job.
    let b = Arc::new(Pages::default());
    let c = PageCache::with_backend(b.clone(), 256 * P);
    let h = MapHint { end: 256, ..MapHint::default() };
    c.fault_ra(100, &h);
    assert_eq!(take(&b), (84..116).collect::<Vec<_>>());
    c.fault_ra(108, &h);
    assert_eq!(readahead::run_pending(), 1);
    assert_eq!(take(&b), (116..148).collect::<Vec<_>>());
    // MADV_RANDOM: a fault on a cached mark starts nothing.
    c.fault_ra(116, &MapHint { random: true, ..h });
    assert_eq!(readahead::run_pending(), 0);
    readahead::set_active(false);
    big_read_takes_io_pages();
}

/// BL-4: the window arithmetic against linux-7/mm/readahead.c (7.3.1) `get_init_ra_size`, `get_next_ra_size`,
/// `ractl_max_pages`, `page_cache_sync_ra` and `page_cache_async_ra` (numbers worked out from the C by hand).
#[test]
fn ra_window_table() {
    // "1-2 page = 16k, 3-4 page 32k, 5-8 page = 64k, > 8 page = 128k initial" (readahead.c:386).
    for (req, want) in [(1u64, 4u64), (2, 4), (3, 8), (4, 8), (5, 16), (8, 16), (9, 32), (32, 32), (100, 32)] {
        assert_eq!(readahead::init_size(req, 32), want, "init {req}");
    }
    for (cur, want) in [(1u64, 4u64), (2, 4), (4, 8), (8, 16), (16, 32), (32, 32)] {
        assert_eq!(readahead::next_size(cur, 32), want, "next {cur} max 32");
    }
    for (cur, want) in [(4u64, 16u64), (16, 32), (32, 64), (64, 128), (128, 128)] {
        assert_eq!(readahead::next_size(cur, 128), want, "next {cur} max 128");
    }
    assert_eq!(readahead::max_pages(32, 0, 1), 32);
    assert_eq!(readahead::max_pages(32, 64, 100), 64, "a large read up to the device's request size");
    assert_eq!(readahead::max_pages(32, 16, 100), 32);
    assert_eq!(readahead::max_pages(32, 512, 300), 256, "one fill's reach");
    // A 4 KiB reader from the file start: the windows and marks (start + size - async_size).
    let w0 = Window::sync_seq(0, 1, 32);
    assert_eq!((w0, w0.mark_in(1000)), (Window { start: 0, size: 4, async_size: 3 }, Some(1)));
    let mut w = w0;
    for (start, size) in [(4u64, 8u64), (12, 16), (28, 32), (60, 32), (92, 32)] {
        w = w.next(32);
        assert_eq!(w, Window { start, size, async_size: size });
        assert_eq!(w.mark_in(1000), Some(start));
    }
    // A device with 128 pages per window.
    let mut w = Window::sync_seq(0, 1, 128);
    assert_eq!(w, Window { start: 0, size: 4, async_size: 3 });
    for (start, size) in [(4u64, 16u64), (20, 32), (52, 64), (116, 128)] {
        w = w.next(128);
        assert_eq!(w, Window { start, size, async_size: size });
    }
    // The aligned end (order 2: 4 pages): 71 + 12 = 83 -> 80.
    assert_eq!(Window { start: 65, size: 6, async_size: 1 }.next(32), Window { start: 71, size: 9, async_size: 9 });
    // A 64 KiB read (16 pages) at the start: 32 pages, half of them async.
    assert_eq!(Window::sync_seq(0, 16, 32), Window { start: 0, size: 32, async_size: 16 });
    assert_eq!(Window::sync_seq(0, 32, 32), Window { start: 0, size: 32, async_size: 16 });
    // History: none or not more than asked -> read as is; else history + request, async 1; from the file start x2.
    assert_eq!(Window::sync_history(10, 1, 1, false, 32), None);
    assert_eq!(Window::sync_history(10, 1, 5, false, 32), Some(Window { start: 10, size: 6, async_size: 1 }));
    assert_eq!(Window::sync_history(10, 2, 10, true, 32), Some(Window { start: 10, size: 22, async_size: 1 }));
    assert_eq!(Window::sync_history(40, 1, 32, false, 32), Some(Window { start: 40, size: 32, async_size: 1 }));
    // No mark past the file end, none without an async part; the backend's window keeps the mark in place.
    assert_eq!(Window { start: 60, size: 32, async_size: 32 }.mark_in(80), None);
    assert_eq!(Window { start: 10, size: 10, async_size: 0 }.mark_in(80), None);
    assert_eq!(w0.at_least(32), Window { start: 0, size: 32, async_size: 31 });
    assert_eq!(w0.at_least(32).mark_in(1000), Some(1));
}

/// BL-4: a mapping's sync windows (linux-7/mm/filemap.c:3329 `do_sync_mmap_readahead`); no workers needed.
#[test]
fn map_fault_windows() {
    let b = Arc::new(Pages::default());
    let c = PageCache::with_backend(b.clone(), 256 * P);
    let h = MapHint { end: 256, ..MapHint::default() };
    // Read-around: 32 pages centred on the fault, a quarter async (mark 84 + 32 - 8 = 108).
    c.fault_ra(100, &h);
    assert_eq!(take(&b), (84..116).collect::<Vec<_>>());
    assert!(c.ready_for_map(107));
    assert!(!c.ready_for_map(108), "the mark is left to its fault (filemap_map_pages skips PG_readahead)");
    assert!(!c.ready_for_map(116), "not cached");
    // A hit reads nothing.
    c.fault_ra(90, &h);
    assert!(take(&b).is_empty());
    // MADV_RANDOM: the page alone.
    c.fault_ra(200, &MapHint { random: true, ..h });
    assert_eq!(take(&b), [200]);
    // Code: ra_pages forward from the fault, not past the VMA; no mark.
    c.fault_ra(150, &MapHint { exec: true, first: 140, end: 160, ..h });
    assert_eq!(take(&b), (150..160).collect::<Vec<_>>());
    assert!(c.ready_for_map(159));
    // Code with MADV_RANDOM still reads ahead (VM_EXEC overrides VM_RAND_READ, filemap.c:3362).
    c.fault_ra(170, &MapHint { exec: true, random: true, first: 0, end: 256, ..h });
    assert_eq!(take(&b), (170..200).collect::<Vec<_>>(), "forward to the cached 200");
    // MADV_SEQUENTIAL: the read path's window (a 32-page request at page 0: 32 pages, mark 16).
    let b = Arc::new(Pages::default());
    let c = PageCache::with_backend(b.clone(), 256 * P);
    c.fault_ra(0, &MapHint { sequential: true, end: 256, ..MapHint::default() });
    assert_eq!(take(&b), (0..32).collect::<Vec<_>>());
    assert!(!c.ready_for_map(16));
    // Checker finding (fixer): a MADV_SEQUENTIAL first touch with no stream behind it (page 100, nothing cached
    // before) still reads ra_pages forward (readahead.c:640 `do_page_cache_ra(req_count)`), without a window.
    c.fault_ra(100, &MapHint { sequential: true, end: 256, ..MapHint::default() });
    assert_eq!(take(&b), (100..132).collect::<Vec<_>>());
    assert!(c.ready_for_map(131), "no mark set by a standalone read");
    // ... also after POSIX_FADV_RANDOM (readahead.c:603 `force_page_cache_ra(req_count)`).
    c.advise(Advice::Random);
    c.fault_ra(200, &MapHint { sequential: true, end: 256, ..MapHint::default() });
    assert_eq!(take(&b), (200..232).collect::<Vec<_>>());
    c.advise(Advice::Normal);
    // MMAP_LOTSAMISS: after 100 more misses than hits, a miss reads only its page.
    let b = Arc::new(Pages::default());
    let n = 128u64;
    let c = PageCache::with_backend(b.clone(), n * 64 * P);
    let h = MapHint { end: n * 64, ..MapHint::default() };
    for k in 0..100 {
        c.fault_ra(k * 64 + 32, &h);
        assert_eq!(take(&b).len(), 32, "miss {k}");
    }
    c.fault_ra(100 * 64 + 32, &h);
    assert_eq!(take(&b), [100 * 64 + 32], "the 101st miss");
    // Hits bring it down again: two hits, then a miss reads around once more.
    c.fault_ra(32, &h);
    c.fault_ra(33, &h);
    c.fault_ra(101 * 64 + 32, &h);
    assert_eq!(take(&b).len(), 32);
}

/// A backend that notes each device call (first page, pages) and reports a device request size (`io_pages`).
struct Big {
    calls: SpinLock<Vec<(u64, u64)>>,
    io: u64,
}

impl Backend for Big {
    fn read_page(&self, _pgoff: u64, _page: &Arc<CachePage>) -> Result<(), i64> {
        Ok(())
    }
    fn read_pages(&self, first: u64, pages: &[Arc<CachePage>]) -> Result<(), i64> {
        self.calls.lock().push((first, pages.len() as u64));
        Ok(())
    }
    fn write_run(&self, _first: u64, _pages: &[Arc<CachePage>], _size: u64) -> Result<(), i64> {
        Ok(())
    }
    fn read_ahead_pages(&self) -> u64 {
        32
    }
    fn io_pages(&self) -> u64 {
        self.io
    }
}

/// Called by `read_ahead_windows` with the workers off (queuing is a process-wide switch).
fn big_read_takes_io_pages() {
    // Checker finding (fixer): a read(2) larger than ra_pages gets one window up to the device's request size
    // (linux-7/mm/readahead.c:562 `ractl_max_pages`, :615 init size for req > max_pages: the request itself).
    let b = Arc::new(Big { calls: SpinLock::new(Vec::new()), io: 128 });
    let c = PageCache::with_backend(b.clone(), 1024 * P);
    let mut buf = alloc::vec![0u8; (128 * P) as usize];
    assert_eq!(c.read_at(0, &mut buf), Ok(buf.len()));
    assert_eq!(core::mem::take(&mut *b.calls.lock()), [(0, 128)], "one 512 KiB device call");
    // Without io_pages the same read is cut into ra_pages windows.
    let b = Arc::new(Big { calls: SpinLock::new(Vec::new()), io: 0 });
    let c = PageCache::with_backend(b.clone(), 1024 * P);
    assert_eq!(c.read_at(0, &mut buf), Ok(buf.len()));
    let calls = core::mem::take(&mut *b.calls.lock());
    assert!(calls.len() > 1 && calls.iter().all(|&(_, n)| n <= 32), "ra_pages windows: {calls:?}");
}
