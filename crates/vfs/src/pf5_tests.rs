//! PF-5 host tests: lockless page-cache reads (no lock, no reference), the evict race of Linux 4c0f369 (readers
//! check every byte while another thread evicts the pages and reuses their memory), and a hole-punch loop in
//! which memory that only waits for a grace period never turns into a false out-of-space answer.

extern crate std;

use crate::abi::{FALLOC_FL_KEEP_SIZE, FALLOC_FL_PUNCH_HOLE};
use crate::cache_view::Stop;
use crate::frame::PAGE;
use crate::pagecache::{Backend, CachePage, PageCache};
use crate::pool::Pool;
use crate::reclaim::{Mode, Reclaimer};
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

const P: u64 = PAGE as u64;

/// The byte at offset `o` of the backed file: page number and offset mixed, never 0 and never `POISON`.
fn pat(o: u64) -> u8 {
    let v = ((o / P).wrapping_mul(31) ^ (o % P).wrapping_mul(7)) % 250;
    v as u8 + 1
}
const POISON: u8 = 0xFF;

/// A block device that holds `pat`.
struct Disk {
    reads: AtomicU64,
}

impl Backend for Disk {
    fn read_page(&self, pgoff: u64, page: &Arc<CachePage>) -> Result<(), i64> {
        self.read_pages(pgoff, core::slice::from_ref(page))
    }
    fn read_pages(&self, first: u64, pages: &[Arc<CachePage>]) -> Result<(), i64> {
        self.reads.fetch_add(pages.len() as u64, Ordering::Relaxed);
        for (k, p) in pages.iter().enumerate() {
            let base = (first + k as u64) * P;
            let b: Vec<u8> = (0..P).map(|i| pat(base + i)).collect();
            p.write(0, &b);
        }
        Ok(())
    }
    fn write_run(&self, _first: u64, _pages: &[Arc<CachePage>], _size: u64) -> Result<(), i64> {
        Ok(())
    }
    fn read_window(&self) -> u64 {
        8
    }
}

#[test]
fn a_cached_read_takes_no_reference_and_no_lock() {
    let c = PageCache::tmpfs(Arc::new(Pool::new("pf5-quick")));
    c.write_at(0, &vec![7u8; 3 * PAGE]).unwrap();
    let page = c.cached(1).unwrap();
    let refs = Arc::strong_count(&page);
    // The index lock's write side held by "a writer" elsewhere would block a locked read; the lockless one
    // does not take it (checked by holding the data lock, which a writer holds while it changes the index).
    let g = c.inner_arc();
    let held = g.lock();
    let mut seen = Vec::new();
    let (n, stop) = c.read_quick(10, 2 * PAGE, &mut |p, off, k, done| {
        if let Some(p) = p {
            seen.push((Arc::strong_count(&page), p.reads.load(Ordering::Relaxed), off, k, done));
        }
        k
    });
    drop(held);
    assert_eq!((n, stop), (2 * PAGE, Stop::More));
    assert_eq!(seen.len(), 3, "pages 0, 1 and 2 (from offset 10)");
    for s in &seen {
        assert_eq!((s.0, s.1), (refs, 0), "no reference and no read count taken: {s:?}");
    }
    let (n, stop) = c.read_quick(3 * P - 5, 100, &mut |_, _, k, _| k);
    assert_eq!((n, stop), (5, Stop::Eof));
}

#[test]
fn a_page_not_cached_ends_the_lockless_part() {
    let d = Arc::new(Disk { reads: AtomicU64::new(0) });
    let c = PageCache::with_backend(d.clone(), 8 * P);
    let mut b = vec![0u8; 8 * PAGE];
    // Nothing cached: the quick path stops at once, `read_at` reads through the backend.
    let (n, stop) = c.read_quick(0, PAGE, &mut |_, _, k, _| k);
    assert_eq!((n, stop), (0, Stop::Missing));
    assert_eq!(c.read_at(0, &mut b), Ok(8 * PAGE));
    assert!(b.iter().enumerate().all(|(i, &v)| v == pat(i as u64)));
    let (n, stop) = c.read_quick(0, 8 * PAGE, &mut |_, _, k, _| k);
    assert_eq!((n, stop), (8 * PAGE, Stop::Eof), "all cached now: lockless to the end");
}

/// Linux 4c0f369's evict race: readers check every byte of a backed file while another thread evicts its pages
/// (reclaim), waits for their grace period, and reuses the freed memory for pages full of `POISON`. A page freed
/// while a lockless reader still copied from it would show the poison (or another page's bytes).
#[test]
fn readers_check_every_byte_while_pages_are_evicted_and_reused() {
    const PAGES: u64 = 24;
    let d = Arc::new(Disk { reads: AtomicU64::new(0) });
    let c = Arc::new(PageCache::with_backend(d.clone(), PAGES * P));
    let len = (PAGES * P) as usize;
    let stop = Arc::new(AtomicBool::new(false));
    let readers: Vec<_> = (0..6u64)
        .map(|t| {
            let (c, stop) = (c.clone(), stop.clone());
            std::thread::spawn(move || {
                let mut b = vec![0u8; 5 * PAGE];
                let (mut reads, mut k) = (0u64, t + 1);
                while !stop.load(Ordering::Relaxed) || reads < 100 {
                    k = k.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                    let pos = (k >> 33) % len as u64;
                    let want = ((k >> 11) as usize % b.len()).max(1);
                    if t % 2 == 0 {
                        let n = c.read_at(pos, &mut b[..want]).unwrap();
                        assert_eq!(n, want.min(len - pos as usize));
                        for (i, &v) in b[..n].iter().enumerate() {
                            assert_eq!(v, pat(pos + i as u64), "byte {i} of a read of {want} at {pos}");
                        }
                    } else {
                        // A slow lockless copy: each byte checked inside the section, a page at a time, so a page
                        // freed and reused during the section shows.
                        let mut one = [0u8; PAGE];
                        let _ = c.read_quick(pos, want, &mut |p, off, k, done| {
                            let Some(p) = p else { return k };
                            p.read(off, &mut one[..k]);
                            for (i, &v) in one[..k].iter().enumerate() {
                                for _ in 0..8 {
                                    std::hint::spin_loop();
                                }
                                assert_eq!(v, pat(pos + (done + i) as u64), "lockless byte {} at {pos}", done + i);
                            }
                            p.read(off, &mut one[..k]);
                            assert!(one[..k].iter().enumerate().all(|(i, &v)| v == pat(pos + (done + i) as u64)), "page changed under the section");
                            k
                        });
                    }
                    reads += 1;
                }
                reads
            })
        })
        .collect();
    let r = Reclaimer::of(&[&c]);
    let t0 = Instant::now();
    let (mut evicted, mut rounds) = (0u64, 0u64);
    while t0.elapsed() < Duration::from_millis(400) || rounds < 50 {
        rounds += 1;
        evicted += r.shrink(PAGES, Mode::Background, true).freed;
        // What passed its grace period is freed now (no wait: readers may still be in their sections).
        mm_core::grace::drain();
        // Reuse: new pages in another file take the freed memory, filled with poison.
        let junk = PageCache::tmpfs(Arc::new(Pool::new("pf5-junk")));
        junk.write_at(0, &vec![POISON; 8 * PAGE]).unwrap();
        drop(junk);
    }
    stop.store(true, Ordering::Relaxed);
    let reads: u64 = readers.into_iter().map(|h| h.join().unwrap()).sum();
    assert!(evicted > 0 && reads > 0, "evicted {evicted} pages, {reads} reads");
    assert!(d.reads.load(Ordering::Relaxed) > PAGES, "evicted pages were read again");
}

/// A hole-punch loop on a lazily charged tmpfs file in a small pool, with readers holding sections all the
/// time: every round fills the whole pool again. The punched pages wait for their grace period, but their
/// charge and their accounting left with them, so no write ever sees ENOSPC; what waits stays bounded (the
/// freer waits for its own batch above the bound).
#[test]
fn a_punch_loop_never_runs_out_while_memory_only_waits() {
    const LIMIT: u64 = 32;
    let pool = Arc::new(Pool::new("pf5-punch"));
    pool.set_size(Some(LIMIT));
    let c = Arc::new(PageCache::tmpfs(pool.clone()));
    let stop = Arc::new(AtomicBool::new(false));
    let readers: Vec<_> = (0..3)
        .map(|_| {
            let (c, stop) = (c.clone(), stop.clone());
            std::thread::spawn(move || {
                // Lockless reads only (sections, no references): what they delay only waits.
                let mut n = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    let (got, _) = c.read_quick(0, 16 * PAGE, &mut |_, _, k, _| {
                        std::hint::spin_loop();
                        k
                    });
                    assert!(got <= 16 * PAGE);
                    n += 1;
                }
                n
            })
        })
        .collect();
    let bound = mm_core::grace::bound_bytes();
    let mut max_pending = 0u64;
    let frames_before = crate::pagecache::frames_held();
    for round in 0..400u64 {
        let v = (round % 200) as u8 + 1;
        let n = c.write_at(0, &vec![v; (LIMIT * P) as usize]);
        assert_eq!(n, Ok((LIMIT * P) as usize), "round {round}: the pool is full of pages that only wait");
        c.fallocate(FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE, 0, LIMIT * P).unwrap();
        assert_eq!(pool.used(), 0, "round {round}: the punch gave the charge back at once");
        max_pending = max_pending.max(mm_core::grace::pending_bytes());
    }
    stop.store(true, Ordering::Relaxed);
    let reads: u64 = readers.into_iter().map(|h| h.join().unwrap()).sum();
    assert!(reads > 0);
    // Other tests share the queue; this file alone retires LIMIT pages per round.
    assert!(max_pending <= bound + 2 * LIMIT * P + (64 << 20), "pending {max_pending} above the bound {bound}");
    mm_core::grace::expedite();
    assert!(crate::pagecache::frames_held() <= frames_before + LIMIT, "the punched pages left the cache's numbers");
}

/// The bound: with a small bound, a freer that retires more waits for its batch (`throttled` grows) and pending
/// stays near the bound.
#[test]
fn above_the_bound_the_freer_waits_for_its_batch() {
    let before = mm_core::grace::stats();
    let c = PageCache::tmpfs(Arc::new(Pool::new("pf5-bound")));
    let small = 8 * P;
    {
        // The guard restores the computed bound when it drops, also if an unwrap below fails: the other tests
        // of this binary share the bound (they would throttle needlessly, never fail, under a small one).
        let _small = mm_core::grace::bound_for_test(small);
        for _ in 0..20 {
            c.write_at(0, &vec![3u8; 32 * PAGE]).unwrap();
            c.truncate(0).unwrap();
        }
    }
    let after = mm_core::grace::stats();
    assert!(after.throttled > before.throttled, "{before:?} -> {after:?}");
}
