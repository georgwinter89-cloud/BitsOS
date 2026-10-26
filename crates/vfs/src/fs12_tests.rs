//! FS-12 host tests: read-ahead windows (`readahead`). One test function: queuing is a process-wide switch.

extern crate std;

use crate::pagecache::{Backend, CachePage, PageCache};
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
        16
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

#[test]
fn read_ahead_windows() {
    // Off without a reader thread: nothing queued.
    let b = Arc::new(Pages::default());
    let c = PageCache::with_backend(b.clone(), 64 * P);
    assert_eq!(byte(&c, 0), 0x5A);
    assert_eq!(take(&b), [0]);
    assert_eq!(readahead::pending(), 0);

    readahead::set_active(true);
    let b = Arc::new(Pages::default());
    let c = PageCache::with_backend(b.clone(), 64 * P);
    // A miss at the file start: the page itself, then a first window of 4 queued.
    assert_eq!(byte(&c, 0), 0x5A);
    assert_eq!(take(&b), [0]);
    assert_eq!(readahead::run_pending(), 1);
    assert_eq!(take(&b), [1, 2, 3, 4]);
    // Reading on: page 1 is the mark, the next window (8) follows the last.
    assert_eq!(byte(&c, 1), 1 ^ 0x5A);
    assert!(take(&b).is_empty(), "page 1 was read ahead");
    assert_eq!(readahead::run_pending(), 1);
    assert_eq!(take(&b), (5..13).collect::<Vec<_>>());
    // The window doubles up to the backend's maximum.
    assert_eq!(byte(&c, 5), 5 ^ 0x5A);
    assert_eq!(readahead::run_pending(), 1);
    assert_eq!(take(&b), (13..29).collect::<Vec<_>>());
    // One job per file; a reader that gets ahead of it reads the pages itself and the job skips them. Page 13
    // (the mark) queues [29, 45); 29 and 30 are read by the reader before it runs.
    for pg in 6..30 {
        assert_eq!(byte(&c, pg), pg as u8 ^ 0x5A);
    }
    assert_eq!(take(&b), [29], "29 missed: read by the reader");
    assert_eq!(byte(&c, 30), 30 ^ 0x5A);
    // The reader reached the mark (29) while the job was still queued: that job queues the next window when
    // it ends (the pipeline keeps going for a reader faster than the disk).
    assert_eq!(readahead::run_pending(), 2);
    assert_eq!(take(&b), [30].into_iter().chain(31..61).collect::<Vec<_>>());
    // A random miss resets the window and queues nothing.
    assert_eq!(byte(&c, 63), 63 ^ 0x5A);
    assert_eq!(take(&b), [63]);
    assert_eq!(readahead::run_pending(), 0);
    // The window ends at the file end.
    let b2 = Arc::new(Pages::default());
    let c2 = PageCache::with_backend(b2.clone(), 3 * P);
    assert_eq!(byte(&c2, 0), 0x5A);
    assert_eq!(readahead::run_pending(), 1);
    assert_eq!(take(&b2), [0, 1, 2]);

    // A truncate while a job waits: the job inserts nothing.
    let b = Arc::new(Pages::default());
    let c = PageCache::with_backend(b.clone(), 16 * P);
    assert_eq!(byte(&c, 0), 0x5A);
    c.truncate(2 * P).unwrap();
    c.truncate(16 * P).unwrap();
    assert_eq!(readahead::run_pending(), 1);
    assert!(c.cached(1).is_none(), "a page read before the truncate is not inserted");
    // A dropped file: the job stops (page 4 is a random miss, 5 a sequential one).
    assert_eq!(byte(&c, 4), 4 ^ 0x5A);
    assert_eq!(readahead::run_pending(), 0);
    assert_eq!(byte(&c, 5), 5 ^ 0x5A);
    drop(c);
    take(&b);
    assert_eq!(readahead::run_pending(), 1);
    assert!(take(&b).is_empty(), "nothing read for a dropped file");

    // posix_fadvise(WILLNEED): the range is queued for the read-ahead thread, the call reads nothing itself.
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
    readahead::set_active(false);
}
