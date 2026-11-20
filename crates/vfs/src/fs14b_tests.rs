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
    // A fault reads around its page (linux-7.3.1/mm/filemap.c:3435): 4 before, 8 in all.
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
    use core::sync::atomic::{AtomicBool, Ordering};
    let b = Runs::new(16, 0);
    let c = Arc::new(PageCache::with_backend(b.clone(), 64 * P));
    // Judged by order: the first read (window 12..28) is inside the backend before page 21 is asked for, and stays
    // there until the waiter gave up and read page 21 alone (60 s at most, then the calls show the wrong order).
    let (begun, alone) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
    let (b1, a1) = (begun.clone(), alone.clone());
    *b.hook.lock() = Some(Arc::new(move |first: u64| {
        if first == 21 {
            a1.store(true, Ordering::SeqCst);
            return;
        }
        b1.store(true, Ordering::SeqCst);
        let t0 = std::time::Instant::now();
        while !a1.load(Ordering::SeqCst) && t0.elapsed() < core::time::Duration::from_secs(60) {
            std::thread::sleep(core::time::Duration::from_millis(1));
        }
    }) as Hook);
    std::thread::scope(|s| {
        let c1 = c.clone();
        s.spawn(move || c1.page_for_map(20, false).unwrap());
        while !begun.load(Ordering::SeqCst) {
            std::thread::sleep(core::time::Duration::from_millis(1));
        }
        // Page 21 is in the first read's window; that read lasts longer than the wait.
        let t0 = std::time::Instant::now();
        c.page_for_map(21, false).unwrap();
        assert!(t0.elapsed().as_nanos() >= FILL_WAIT_NS as u128 / 2, "the waiter read alone before the wait was over");
    });
    *b.hook.lock() = None;
    assert!(alone.load(Ordering::SeqCst));
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

/// L7-VFS row #50, decision in plan/spec/storage.md §3.1: code faults on a backend without read-ahead (FAT's
/// window of 32, and a window of 8 for comparison) read around the page (`Reach::Around`, what `fault_ra` does)
/// or, as Linux 7.3.1's VM_EXEC rule (mm/filemap.c:3407), forward from it inside the VMA. A 512-page code VMA at
/// file page 16, about 400 faults per pattern (LCG seed 12345: random pages, ascending, 50 clusters of 8 pages
/// ascending, the same clusters shuffled). Synthetic patterns, not a recorded trace. The device request count is
/// the measure, a proxy for time (on a USB stick one request costs far more than a 4 KiB page, FS-14b); no time
/// is measured. A plain test so the desk can re-run it (`cargo test -p vfs --lib
/// exec_faults_around_against_forward -- --nocapture` prints the table); it fails if forward ever needs fewer
/// requests than around at FAT's window.
#[test]
fn exec_faults_around_against_forward() {
    const FIRST: u64 = 16;
    const LEN: u64 = 512;
    let mut seed = 12345u64;
    let mut next = move || {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (seed >> 33) % LEN
    };
    let random: Vec<u64> = (0..400).map(|_| next()).collect();
    let ascending: Vec<u64> = (0..400).collect();
    let clustered: Vec<u64> = (0..50).flat_map(|_| {
        let at = next().min(LEN - 8);
        at..at + 8
    }).collect();
    // The same clusters' pages in a shuffled order inside each cluster (calls into a function's neighbours).
    let shuffled: Vec<u64> = clustered.chunks(8).flat_map(|c| {
        let mut c = c.to_vec();
        for i in (1..c.len()).rev() {
            c.swap(i, (next() as usize) % (i + 1));
        }
        c
    }).collect();
    let hint = crate::pagecache::MapHint { random: false, sequential: false, exec: true, first: FIRST, end: FIRST + LEN };
    std::println!("window | pattern | around: requests / pages / outside | forward: requests / pages / outside");
    for window in [8u64, 32] {
        for (name, faults) in [("random", &random), ("ascending", &ascending), ("clustered", &clustered), ("shuffled", &shuffled)] {
            let mut got = [(0u64, 0u64, 0u64); 2];
            for (k, slot) in got.iter_mut().enumerate() {
                let b = Runs::new(window, 0);
                let c = PageCache::with_backend(b.clone(), 1000 * P);
                for &f in faults.iter() {
                    let pg = FIRST + f;
                    if c.cached(pg).is_some() {
                        continue;
                    }
                    if k == 0 {
                        c.fault_ra(pg, &hint);
                    } else {
                        c.fill_with(pg, crate::pagecache::Reach::Window(pg, window.min(FIRST + LEN - pg))).unwrap();
                    }
                }
                let calls = b.take();
                let pages = calls.iter().map(|&(_, n)| n).sum();
                let outside = calls.iter().flat_map(|&(s, n)| s..s + n).filter(|&p| !(FIRST..FIRST + LEN).contains(&p)).count() as u64;
                *slot = (calls.len() as u64, pages, outside);
            }
            let [a, f] = got;
            std::println!("{window} | {name} | {} / {} / {} | {} / {} / {}", a.0, a.1, a.2, f.0, f.1, f.2);
            assert_eq!(f.2, 0, "forward stays inside the VMA");
            // FAT's window (the backend this rule is about): around never needs more requests.
            assert!(window != 32 || a.0 <= f.0, "{window} {name}: around {} requests, forward {}", a.0, f.0);
        }
    }
}
