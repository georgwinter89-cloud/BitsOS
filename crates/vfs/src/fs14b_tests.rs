//! FS-14b host tests: a missed page brings its window into the cache in one backend call and every page read
//! stays cached (`PageCache::fill_with`); a page another thread is reading is waited for, not read again.

extern crate std;

use crate::pagecache::{Backend, CachePage, PageCache, PageFault, FILL_WAIT_NS, MAX_WINDOW};
use alloc::sync::Arc;
use alloc::vec::Vec;
use ksync::SpinLock;
use uapi::errno::*;

const P: u64 = 4096;

type Hook = Arc<dyn Fn(u64) + Send + Sync>;

/// Records every backend call as (first page, pages); `slow_ms` widens race windows; a run holding page
/// `bad` fails with EIO; `hook` runs inside each read (first page) before the bytes are written.
struct Runs {
    calls: SpinLock<Vec<(u64, u64)>>,
    window: u64,
    slow_ms: u64,
    bad: SpinLock<Option<u64>>,
    hook: SpinLock<Option<Hook>>,
}

impl Runs {
    fn new(window: u64, slow_ms: u64) -> Arc<Runs> {
        Arc::new(Runs {
            calls: SpinLock::new(Vec::new()),
            window,
            slow_ms,
            bad: SpinLock::new(None),
            hook: SpinLock::new(None),
        })
    }
    fn take(&self) -> Vec<(u64, u64)> {
        core::mem::take(&mut *self.calls.lock())
    }
}

impl Backend for Runs {
    fn read_page(&self, pgoff: u64, page: &Arc<CachePage>) -> Result<(), i64> {
        self.read_pages(pgoff, core::slice::from_ref(page))
    }
    fn read_pages(&self, first: u64, pages: &[Arc<CachePage>]) -> Result<(), i64> {
        self.calls.lock().push((first, pages.len() as u64));
        if self.slow_ms > 0 {
            std::thread::sleep(core::time::Duration::from_millis(self.slow_ms));
        }
        let hook = self.hook.lock().clone();
        if let Some(h) = hook {
            h(first);
        }
        if self.bad.lock().is_some_and(|b| b >= first && b < first + pages.len() as u64) {
            return Err(EIO);
        }
        for (i, p) in pages.iter().enumerate() {
            p.write(0, &[(first + i as u64) as u8 ^ 0x3C]);
        }
        Ok(())
    }
    fn write_run(&self, _first: u64, _pages: &[Arc<CachePage>], _size: u64) -> Result<(), i64> {
        Ok(())
    }
    fn read_window(&self) -> u64 {
        self.window
    }
}

fn byte(c: &PageCache, pg: u64) -> u8 {
    let mut one = [0u8; 1];
    assert_eq!(c.read_at(pg * P, &mut one), Ok(1));
    one[0]
}

#[test]
fn windows_are_kept_and_never_read_twice() {
    let b = Runs::new(8, 0);
    let c = PageCache::with_backend(b.clone(), 1000 * P);
    // A fault reads around its page (linux-6.6/mm/filemap.c:3195): 4 before, 8 in all.
    c.page_for_map(10, false).unwrap();
    assert_eq!(b.take(), [(6, 8)]);
    assert_eq!(c.pages(), 8);
    c.page_for_map(13, false).unwrap();
    assert!(b.take().is_empty(), "a page of the window is cached");
    // The next window stops at cached pages on both sides.
    c.page_for_map(5, false).unwrap();
    assert_eq!(b.take(), [(1, 5)]);
    c.page_for_map(0, false).unwrap();
    assert_eq!(b.take(), [(0, 1)]);
    // read(): the page and the window after it; a larger read gets what it asks for in one call, up to the cap.
    assert_eq!(byte(&c, 20), 20 ^ 0x3C);
    assert_eq!(b.take(), [(20, 8)]);
    let mut big = alloc::vec![0u8; (300 * P) as usize];
    assert_eq!(c.read_at(40 * P, &mut big), Ok(big.len()));
    assert_eq!(b.take(), [(40, MAX_WINDOW), (40 + MAX_WINDOW, 300 - MAX_WINDOW)]);
    assert!((0..300u64).all(|i| big[(i * P) as usize] == (40 + i) as u8 ^ 0x3C), "bytes of the run");
    // The window never runs past the end of the file.
    c.page_for_map(998, false).unwrap();
    assert_eq!(b.take(), [(994, 6)]);
    // A backend without a window (the default 1) reads only the faulted page.
    let one = Runs::new(1, 0);
    let c1 = PageCache::with_backend(one.clone(), 50 * P);
    c1.page_for_map(7, false).unwrap();
    assert_eq!(one.take(), [(7, 1)]);
}

#[test]
fn a_page_being_read_is_waited_for() {
    let b = Runs::new(16, 50);
    let c = Arc::new(PageCache::with_backend(b.clone(), 64 * P));
    std::thread::scope(|s| {
        for _ in 0..4 {
            let c = c.clone();
            s.spawn(move || {
                c.page_for_map(20, false).unwrap();
                c.page_for_map(21, false).unwrap();
            });
        }
    });
    assert_eq!(b.take(), [(12, 16)], "four threads faulting the same pages read them once");
    assert_eq!(byte(&c, 21), 21 ^ 0x3C);
}

#[test]
fn a_bad_neighbour_does_not_fail_a_good_page() {
    let b = Runs::new(16, 0);
    *b.bad.lock() = Some(25);
    let c = PageCache::with_backend(b.clone(), 64 * P);
    // The window [12, 28) holds the bad page: the faulted page is read alone and only it is cached.
    c.page_for_map(20, false).unwrap();
    assert_eq!(b.take(), [(12, 16), (20, 1)]);
    assert_eq!(c.pages(), 1);
    assert_eq!(byte(&c, 20), 20 ^ 0x3C);
    // The bad page itself fails, for a fault (SIGBUS) and for read().
    assert!(matches!(c.page_for_map(25, false), Err(PageFault::Io)));
    assert_eq!(b.take().last(), Some(&(25, 1)));
    let mut one = [0u8; 1];
    assert_eq!(c.read_at(25 * P, &mut one), Err(EIO));
    b.take();
    assert_eq!(c.pages(), 1, "no page of a failed read is cached");
    // No note of the failed reads is left: the next miss reads its window at once, not alone after a wait.
    let t = std::time::Instant::now();
    c.page_for_map(40, false).unwrap();
    assert_eq!(b.take(), [(32, 16)]);
    assert!(t.elapsed().as_nanos() < FILL_WAIT_NS as u128);
}

#[test]
fn a_waiter_reads_alone_after_the_timeout() {
    let slow = FILL_WAIT_NS / 1_000_000 * 2;
    let b = Runs::new(16, slow);
    let c = Arc::new(PageCache::with_backend(b.clone(), 64 * P));
    std::thread::scope(|s| {
        let c1 = c.clone();
        s.spawn(move || c1.page_for_map(20, false).unwrap());
        std::thread::sleep(core::time::Duration::from_millis(slow / 4));
        // Page 21 is in the first read's window; that read takes longer than the wait.
        c.page_for_map(21, false).unwrap();
    });
    assert_eq!(b.take(), [(12, 16), (21, 1)]);
    assert_eq!(byte(&c, 21), 21 ^ 0x3C);
    assert_eq!(c.pages(), 16);
}

#[test]
fn a_truncate_during_a_fill_leaves_nothing_past_the_end() {
    let b = Runs::new(16, 0);
    let c = Arc::new(PageCache::with_backend(b.clone(), 64 * P));
    let weak = Arc::downgrade(&c);
    *b.hook.lock() = Some(Arc::new(move |_| {
        if let Some(c) = weak.upgrade() {
            c.truncate(18 * P).unwrap();
        }
    }) as Hook);
    // The truncate ends the fill's epoch: none of its pages goes in, the fault then sees the new end.
    assert!(matches!(c.page_for_map(20, false), Err(PageFault::Bus)));
    *b.hook.lock() = None;
    assert_eq!(c.pages(), 0);
    let mut buf = alloc::vec![0u8; (20 * P) as usize];
    assert_eq!(c.read_at(0, &mut buf), Ok((18 * P) as usize));
    assert!(b.take().iter().all(|&(f, n)| f == 12 && n == 16 || f + n <= 18), "reads stop at the new end");
    assert_eq!(c.pages(), 18);
}

#[test]
fn a_window_keeps_pages_already_in_the_cache() {
    let b = Runs::new(16, 0);
    let c = PageCache::with_backend(b.clone(), 64 * P);
    // Whole-page writes need no read: page 15 dirty, page 17 a written page of zeros (a hole).
    assert_eq!(c.write_at(15 * P, &[0xAA; P as usize]), Ok(P as usize));
    assert_eq!(c.write_at(17 * P, &[0u8; P as usize]), Ok(P as usize));
    assert!(b.take().is_empty());
    c.page_for_map(16, false).unwrap();
    // A window never covers a cached page: it stops at page 15 on the left and at page 17 on the right.
    assert_eq!(b.take(), [(16, 1)]);
    assert_eq!(byte(&c, 15), 0xAA, "the dirty page keeps its bytes");
    assert_eq!(byte(&c, 17), 0, "the hole keeps its zeros");
    assert_eq!(byte(&c, 16), 16 ^ 0x3C);
    let mut buf = alloc::vec![0u8; (8 * P) as usize];
    assert_eq!(c.read_at(12 * P, &mut buf), Ok(buf.len()));
    assert_eq!(buf[(3 * P) as usize], 0xAA);
    assert_eq!(buf[(5 * P) as usize], 0);
    assert_eq!(buf[(7 * P) as usize], 19 ^ 0x3C);
    assert_eq!(c.dirty_pages(), 2);
}
