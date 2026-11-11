//! BL-0 host tests: POSIX_FADV_DONTNEED (`PageCache::dont_need`, linux-7/mm/fadvise.c:113), drop_caches' page-cache
//! part (`Reclaimer::drop_clean`) and the per-CPU counters (`pcpu`). What goes: clean whole pages only the index
//! holds. What stays: partial edge pages, mapped pages, pages a reader holds, dirty pages whose write failed,
//! pages being read in, tmpfs pages.
use crate::pagecache::{Backend, CachePage, Invalidated, PageCache};
use crate::reclaim::Reclaimer;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use ksync::SpinLock;

const P: u64 = 4096;

/// Every page holds its number at offset 0; counts reads and written pages; writes fail while `fail` is set.
struct Pat {
    reads: AtomicU64,
    written: AtomicU64,
    fail: AtomicBool,
    /// Called inside the first `read_pages` (a DONTNEED while a read is under way).
    during: SpinLock<Option<Weak<PageCache>>>,
}

impl Default for Pat {
    fn default() -> Pat {
        Pat { reads: AtomicU64::new(0), written: AtomicU64::new(0), fail: AtomicBool::new(false), during: SpinLock::new(None) }
    }
}

impl Backend for Pat {
    fn read_page(&self, pgoff: u64, page: &Arc<CachePage>) -> Result<(), i64> {
        self.read_pages(pgoff, core::slice::from_ref(page))
    }
    fn read_pages(&self, first: u64, pages: &[Arc<CachePage>]) -> Result<(), i64> {
        self.reads.fetch_add(pages.len() as u64, Ordering::Relaxed);
        for (i, p) in pages.iter().enumerate() {
            p.write(0, &(first + i as u64).to_le_bytes());
        }
        let during = self.during.lock().take();
        if let Some(c) = during.and_then(|w| w.upgrade()) {
            let r = c.dont_need(0, 0);
            assert_eq!(r.dropped, 0, "nothing is in the index yet: the pages being read are not dropped: {r:?}");
        }
        Ok(())
    }
    fn write_run(&self, _first: u64, pages: &[Arc<CachePage>], _size: u64) -> Result<(), i64> {
        if self.fail.load(Ordering::Relaxed) {
            return Err(uapi::errno::EIO);
        }
        self.written.fetch_add(pages.len() as u64, Ordering::Relaxed);
        Ok(())
    }
}

fn file(size: u64) -> (PageCache, Arc<Pat>) {
    let b = Arc::new(Pat::default());
    (PageCache::with_backend(b.clone(), size), b)
}

fn read_all(c: &PageCache) {
    for i in 0..c.size().div_ceil(P) {
        let mut v = [0u8; 8];
        assert_eq!(c.read_at(i * P, &mut v), Ok(8));
        assert_eq!(u64::from_le_bytes(v), i, "page {i}");
    }
}

fn cached(c: &PageCache, n: u64) -> Vec<u64> {
    (0..n).filter(|&i| c.cached(i).is_some()).collect()
}

#[test]
fn the_whole_file_goes_and_reads_back() {
    let (c, b) = file(16 * P);
    read_all(&c);
    assert_eq!(c.dont_need(0, 0), Invalidated { dropped: 16, kept: 0 });
    assert_eq!(c.pages(), 0);
    let before = b.reads.load(Ordering::Relaxed);
    read_all(&c);
    assert_eq!(b.reads.load(Ordering::Relaxed) - before, 16, "every page is read from the backend again");
}

/// fadvise.c:120-145: only the whole pages inside the range; the last page also when the range ends at a page end
/// or at the file's end.
#[test]
fn partial_edge_pages_stay() {
    let (c, _b) = file(16 * P);
    read_all(&c);
    // [100, 100 + 3 pages): page 0 and page 3 are partial.
    assert_eq!(c.dont_need(100, 3 * P).dropped, 2);
    assert_eq!(cached(&c, 5), [0, 3, 4]);
    // A range ending on a page end takes its last page.
    assert_eq!(c.dont_need(4 * P, P).dropped, 1);
    assert!(c.cached(4).is_none() && c.cached(5).is_some());
    // Inside one page, not at its end: nothing.
    assert_eq!(c.dont_need(0, 10).dropped, 0);
    assert!(c.cached(0).is_some());
    // Offset 0, a length below a page, not the file's end: end_index 0 is not taken (the `end_index == 0` break).
    assert_eq!(c.dont_need(0, P - 1).dropped, 0);
    // Past the end: nothing, no error.
    assert_eq!(c.dont_need(100 * P, 0), Invalidated::default());
}

#[test]
fn a_range_to_the_file_end_takes_the_last_partial_page() {
    let size = 10 * P + 100;
    let (c, _b) = file(size);
    read_all(&c);
    assert_eq!(c.pages(), 11);
    // endbyte = size - 1 = i_size - 1: the last (partial) page goes.
    assert_eq!(c.dont_need(9 * P, P + 100).dropped, 2);
    assert_eq!(c.pages(), 9);
    // Huge lengths count as "to the end" (offset + len past LLONG_MAX).
    assert_eq!(c.dont_need(0, i64::MAX as u64).dropped, 9);
    assert_eq!(c.pages(), 0);
}

#[test]
fn mapped_and_held_pages_stay() {
    let (c, _b) = file(8 * P);
    read_all(&c);
    let pin = c.page_for_map(1, false).unwrap();
    let held = c.cached(2).unwrap();
    assert_eq!(c.dont_need(0, 0), Invalidated { dropped: 6, kept: 2 });
    assert!(c.cached(1).is_some_and(|p| Arc::ptr_eq(&p, &pin)), "the mapping keeps its page in the cache");
    assert!(c.cached(2).is_some_and(|p| Arc::ptr_eq(&p, &held)));
    assert!(!pin.wipe.load(Ordering::Acquire) && !held.wipe.load(Ordering::Acquire), "DONTNEED never wipes");
    drop((pin, held));
    assert_eq!(c.dont_need(0, 0).dropped, 2, "let go: now they go");
}

/// Dirty pages are written back first (`filemap_flush_range`), then go; a page whose write fails stays dirty.
#[test]
fn dirty_pages_are_written_then_go_unless_the_write_fails() {
    let (c, b) = file(8 * P);
    read_all(&c);
    c.write_at(3 * P, &[9u8; P as usize]).unwrap();
    b.fail.store(true, Ordering::Relaxed);
    assert_eq!(c.dont_need(0, 0), Invalidated { dropped: 7, kept: 1 });
    assert!(c.cached(3).is_some_and(|p| p.is_dirty()), "unwritten data stays");
    b.fail.store(false, Ordering::Relaxed);
    assert_eq!(c.dont_need(0, 0).dropped, 1);
    assert_eq!(b.written.load(Ordering::Relaxed), 1, "the dirty page reached the backend before it went");
    // Outside the range a dirty page is not written.
    c.write_at(5 * P, &[1u8; 10]).unwrap();
    c.dont_need(0, 2 * P);
    assert_eq!(b.written.load(Ordering::Relaxed), 1);
    assert!(c.cached(5).is_some_and(|p| p.is_dirty()));
}

#[test]
fn pages_being_read_are_not_lost() {
    let b = Arc::new(Pat::default());
    let c = Arc::new(PageCache::with_backend(b.clone(), 4 * P));
    *b.during.lock() = Some(Arc::downgrade(&c));
    let mut v = [0u8; 8];
    assert_eq!(c.read_at(2 * P, &mut v), Ok(8));
    assert_eq!(u64::from_le_bytes(v), 2);
    assert!(c.cached(2).is_some(), "the page read while DONTNEED ran is in the cache afterwards");
}

#[test]
fn tmpfs_keeps_its_pages() {
    let t = PageCache::tmpfs(Arc::new(crate::pool::Pool::new("bl0-tmpfs")));
    t.write_at(0, &[5u8; 3 * P as usize]).unwrap();
    assert_eq!(t.dont_need(0, 0), Invalidated::default());
    assert_eq!(t.pages(), 3);
}

/// drop_caches 1 over a private ring: clean pages of every listed cache go; dirty and mapped ones stay.
#[test]
fn drop_clean_takes_every_clean_page() {
    let (a, _) = file(8 * P);
    let (d, _) = file(8 * P);
    read_all(&a);
    read_all(&d);
    d.write_at(0, &[1u8; 5]).unwrap();
    let pin = a.page_for_map(7, false).unwrap();
    let r = Reclaimer::of(&[&a, &d]);
    assert_eq!(r.drop_clean(), Invalidated { dropped: 14, kept: 2 });
    assert_eq!(cached(&a, 8), [7]);
    assert!(d.cached(0).is_some_and(|p| p.is_dirty()), "drop_caches never writes back: the dirty page stays");
    drop(pin);
    assert_eq!(r.drop_clean().dropped, 1);
}

#[test]
fn per_cpu_counters_are_exact_and_fold_in_batches() {
    crate::pcpu_counter!(T, "vfs.test.bl0_pcpu", 100);
    T.add(5);
    assert_eq!(T.sum(), 5, "without slots it counts in the shared value");
    T.prepare();
    for _ in 0..30 {
        T.add(3);
    }
    assert_eq!(T.sum(), 95);
    T.add(10);
    assert_eq!(T.sum(), 105, "the slot folded at the batch; nothing lost");
    assert_eq!(T.name(), "vfs.test.bl0_pcpu");
    assert!(klog::metrics::snapshot().any(|(n, v)| n == "vfs.test.bl0_pcpu" && v <= 105 && v >= 5));
}

/// PF-5's grace rule under DONTNEED and drop_caches 1 (the checker's case "a page a lockless reader still copies
/// from"): lockless readers check each page's number, twice, inside their section, while another thread drops the
/// pages and reuses the freed memory for poison pages. A page freed before its readers left would show the poison.
#[test]
fn lockless_readers_survive_dontneed_and_drop_caches() {
    extern crate std;
    const PAGES: u64 = 16;
    let (c, b) = file(PAGES * P);
    let c = Arc::new(c);
    let stop = Arc::new(AtomicBool::new(false));
    let readers: Vec<_> = (0..4u64)
        .map(|t| {
            let (c, stop) = (c.clone(), stop.clone());
            std::thread::spawn(move || {
                let mut reads = 0u64;
                while !stop.load(Ordering::Relaxed) || reads < 100 {
                    let i = (reads * 7 + t) % PAGES;
                    let mut v = [0u8; 8];
                    let _ = c.read_quick(i * P, 8, &mut |p, off, k, _| {
                        let Some(p) = p else { return k };
                        p.read(off, &mut v);
                        for _ in 0..64 {
                            core::hint::spin_loop();
                        }
                        assert_eq!(u64::from_le_bytes(v), i, "page {i} inside the section");
                        p.read(off, &mut v);
                        assert_eq!(u64::from_le_bytes(v), i, "page {i} changed under the section");
                        k
                    });
                    let mut w = [0u8; 8];
                    assert_eq!(c.read_at(i * P, &mut w), Ok(8));
                    assert_eq!(u64::from_le_bytes(w), i, "page {i} by read_at");
                    reads += 1;
                }
                reads
            })
        })
        .collect();
    let r = Reclaimer::of(&[&*c]);
    let (mut dropped, mut round) = (0, 0u32);
    let t0 = std::time::Instant::now();
    while t0.elapsed() < std::time::Duration::from_millis(200) || round < 200 {
        round += 1;
        dropped += if round % 2 == 0 { c.dont_need(0, 0).dropped } else { r.drop_clean().dropped };
        mm_core::grace::drain();
        let junk = PageCache::tmpfs(Arc::new(crate::pool::Pool::new("bl0-junk")));
        junk.write_at(0, &[0xFFu8; 4 * P as usize]).unwrap();
        drop(junk);
    }
    stop.store(true, Ordering::Relaxed);
    let reads: u64 = readers.into_iter().map(|h| h.join().unwrap()).sum();
    assert!(dropped > 0 && reads > 0, "dropped {dropped}, reads {reads}");
    assert!(b.reads.load(Ordering::Relaxed) > PAGES, "dropped pages were read again");
}
