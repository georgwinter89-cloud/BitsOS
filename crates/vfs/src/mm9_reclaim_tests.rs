//! MM-9 host tests: page-cache reclaim (clock, preloaded pages first, what must stay, rings, bounds). Each
//! test uses a private `Reclaimer` over its own caches: the global one sees every other test's files.
use crate::pagecache::{Backend, CachePage, PageCache};
use crate::reclaim::{Mode, Reclaimer, MIN_SCAN};
use alloc::sync::Arc;
use alloc::vec;
use core::sync::atomic::{AtomicU64, Ordering};

const P: u64 = 4096;

/// Every page holds its number at offset 0; counts the pages read.
struct Pat {
    reads: AtomicU64,
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
        Ok(())
    }
    fn write_run(&self, _first: u64, _pages: &[Arc<CachePage>], _size: u64) -> Result<(), i64> {
        Ok(())
    }
    fn read_window(&self) -> u64 {
        16
    }
}

fn file(pages: u64) -> (PageCache, Arc<Pat>) {
    let b = Arc::new(Pat { reads: AtomicU64::new(0) });
    (PageCache::with_backend(b.clone(), pages * P), b)
}

/// Reads every page and checks its number.
fn read_all(c: &PageCache, pages: u64) {
    for i in 0..pages {
        let mut v = [0u8; 8];
        assert_eq!(c.read_at(i * P, &mut v), Ok(8));
        assert_eq!(u64::from_le_bytes(v), i, "page {i}");
    }
}

#[test]
fn clean_pages_go_and_read_back() {
    let (c, b) = file(64);
    read_all(&c, 64);
    assert_eq!(c.pages(), 64);
    let r = Reclaimer::of(&[&c]);
    let o = r.shrink(16, Mode::Background, false);
    assert_eq!(o.freed, 16, "{o:?}");
    assert_eq!(c.pages(), 48);
    let before = b.reads.load(Ordering::Relaxed);
    read_all(&c, 64);
    assert_eq!(b.reads.load(Ordering::Relaxed) - before, 16, "only the reclaimed pages are read again");
    assert_eq!(c.pages(), 64);
}

#[test]
fn second_chance_keeps_a_used_page() {
    let (c, _b) = file(8);
    read_all(&c, 8);
    let r = Reclaimer::of(&[&c]);
    // The first turn marks every page cold, the second takes page 0.
    assert_eq!(r.shrink(1, Mode::Background, false).freed, 1);
    assert!(c.cached(0).is_none() && c.cached(1).is_some());
    let mut v = [0u8; 8];
    c.read_at(P, &mut v).unwrap();
    assert_eq!(r.shrink(1, Mode::Background, false).freed, 1);
    assert!(c.cached(1).is_some(), "page 1 was used since the hand passed: it gets a second chance");
    assert!(c.cached(2).is_none(), "the next cold page went instead");
}

#[test]
fn unused_preloaded_pages_go_first() {
    let (a, _) = file(32);
    read_all(&a, 32);
    let (p, _) = file(32);
    assert_eq!(p.prefetch(0, 32, &mut |_| true).map(|x| x.0), Ok(32));
    // Five preloaded pages are used by a program: they are ordinary cache pages now.
    for i in 0..5 {
        let mut v = [0u8; 8];
        p.read_at(i * P, &mut v).unwrap();
    }
    let r = Reclaimer::of(&[&a, &p]);
    assert_eq!(r.listed(), (2, 1));
    let o = r.shrink(10, Mode::Background, false);
    assert_eq!((o.freed, o.preloaded), (10, 10), "{o:?}");
    assert_eq!(a.pages(), 32, "the read file keeps its pages while unused preloaded ones exist");
    let o = r.shrink(17, Mode::Background, false);
    assert_eq!((o.freed, o.preloaded), (17, 17), "the other 17 unused preloaded pages: {o:?}");
    assert_eq!(a.pages(), 32);
    for i in 0..5 {
        assert!(p.cached(i).is_some(), "used preloaded page {i} is not taken first");
    }
    let o = r.shrink(3, Mode::Background, false);
    assert_eq!((o.freed, o.preloaded), (3, 0), "none left: the clock takes over: {o:?}");
    assert_eq!(r.listed().1, 0, "no unused preloaded page left: the cache leaves the preload ring");
}

#[test]
fn mapped_and_dirty_pages_stay() {
    let (c, _b) = file(32);
    read_all(&c, 32);
    let pins: vec::Vec<_> = (0..4).map(|i| c.page_for_map(i, false).unwrap()).collect();
    c.write_at(10 * P, &[1u8; 3 * P as usize]).unwrap();
    let r = Reclaimer::of(&[&c]);
    let o = r.shrink(1000, Mode::Background, true);
    assert_eq!(o.freed, 32 - 4 - 3, "{o:?}");
    assert!(o.held >= 4 && o.dirty >= 3, "{o:?}");
    assert_eq!(c.pages(), 7);
    for (i, p) in pins.iter().enumerate() {
        assert!(c.cached(i as u64).is_some_and(|x| Arc::ptr_eq(&x, p)), "mapped page {i} stays the mapping's page");
    }
    assert!(c.cached(10).is_some_and(|p| p.is_dirty()), "dirty data stays");
    drop(pins);
    assert_eq!(r.shrink(1000, Mode::Background, true).freed, 4, "unmapped now: they go");
}

#[test]
fn direct_reclaim_never_waits_for_a_lock() {
    let (c, _b) = file(8);
    read_all(&c, 8);
    let r = Reclaimer::of(&[&c]);
    let g = c.inner_arc().lock();
    let o = r.shrink(8, Mode::Direct, true);
    assert_eq!((o.freed, o.busy), (0, 1), "a cache whose lock is taken is passed over: {o:?}");
    drop(g);
    assert_eq!(r.shrink(8, Mode::Direct, true).freed, 8);
}

#[test]
fn dead_caches_leave_the_ring() {
    let (c, _b) = file(4);
    read_all(&c, 4);
    let r = Reclaimer::of(&[&c]);
    drop(c);
    assert_eq!(r.shrink(4, Mode::Background, false).freed, 0);
    assert_eq!(r.listed(), (0, 0));
}

#[test]
fn a_light_call_is_bounded_and_a_thorough_one_finds_a_page() {
    let n = 3 * MIN_SCAN;
    let (c, _b) = file(n);
    read_all(&c, n);
    let r = Reclaimer::of(&[&c]);
    let o = r.shrink(1, Mode::Background, false);
    assert_eq!(o.freed, 0, "every page was used: the light budget only marks them");
    assert_eq!(o.scanned, MIN_SCAN);
    let o = r.shrink(1, Mode::Background, true);
    assert_eq!(o.freed, 1, "{o:?}");
    assert!(o.scanned <= 2 * n, "{o:?}");
}

#[test]
fn tmpfs_pages_are_never_listed() {
    let t = PageCache::tmpfs(Arc::new(crate::pool::Pool::new("mm9-tmpfs")));
    t.write_at(0, &[7u8; 2 * P as usize]).unwrap();
    assert!(!t.reclaimable());
    let (c, _b) = file(1);
    assert!(c.reclaimable());
}

/// A device in memory: what was written back is read again (pages never written read as zeros).
struct Disk {
    blocks: ksync::SpinLock<alloc::collections::BTreeMap<u64, vec::Vec<u8>>>,
    writes: AtomicU64,
}

impl Default for Disk {
    fn default() -> Self {
        Disk { blocks: ksync::SpinLock::new(alloc::collections::BTreeMap::new()), writes: AtomicU64::new(0) }
    }
}

impl Backend for Disk {
    fn read_page(&self, pgoff: u64, page: &Arc<CachePage>) -> Result<(), i64> {
        self.read_pages(pgoff, core::slice::from_ref(page))
    }
    fn read_pages(&self, first: u64, pages: &[Arc<CachePage>]) -> Result<(), i64> {
        let g = self.blocks.lock();
        for (i, p) in pages.iter().enumerate() {
            if let Some(b) = g.get(&(first + i as u64)) {
                p.write(0, b);
            }
        }
        Ok(())
    }
    fn write_run(&self, first: u64, pages: &[Arc<CachePage>], _size: u64) -> Result<(), i64> {
        let mut g = self.blocks.lock();
        for (i, p) in pages.iter().enumerate() {
            let mut b = vec![0u8; P as usize];
            p.read(0, &mut b);
            g.insert(first + i as u64, b);
        }
        self.writes.fetch_add(pages.len() as u64, Ordering::Relaxed);
        Ok(())
    }
    fn read_window(&self) -> u64 {
        8
    }
}

fn read8(c: &PageCache, pos: u64) -> [u8; 8] {
    let mut v = [0u8; 8];
    assert_eq!(c.read_at(pos, &mut v), Ok(8));
    v
}

/// Fixer finding 1: write-back cleans a shared mapping's page but does not write-protect it, so a later store
/// through the mapping is not seen; the last pin marks the page dirty again, so reclaim does not take it (and
/// lose the store) after munmap, and the next write-back saves it.
#[test]
fn a_store_after_write_back_survives_munmap_and_reclaim() {
    use crate::inode::{Cred, Data, Inode};
    use crate::mmap::{FileMap, MapBacking};
    use crate::mount_ns::{MountNs, Path};
    use crate::pagecache::SyncKind;
    let disk = Arc::new(Disk::default());
    let ns = MountNs::new_empty();
    let root = ns.root_path();
    let inode = Inode::new(909_001, uapi::S_IFREG | 0o644, Cred::ROOT, Data::Reg(PageCache::with_backend(disk.clone(), 2 * P)));
    let d = root.dentry.link_child(b"mm9-shared", inode).unwrap();
    let path = Path { mnt: root.mnt.clone(), dentry: d };
    let c = path.dentry.inode.page_cache().unwrap();
    let map = FileMap::new(path.clone(), true, true);
    assert!(map.track_dirty());
    // A read mapping (the page stays clean at munmap), and a write fault of page 0.
    let ro = map.get_page(1, false).unwrap();
    let phys = map.get_page(0, true).unwrap();
    c.cached(0).unwrap().write(0, b"first...");
    c.sync_all(SyncKind::Fsync).unwrap();
    assert_eq!(c.dirty_pages(), 0, "written back");
    // A store through the still writable pte: nothing marks the page.
    c.cached(0).unwrap().write(0, b"second..");
    assert!(map.put_page(phys) && map.put_page(ro));
    assert!(c.cached(0).is_some_and(|p| p.is_dirty()), "the last pin of a written page marks it dirty");
    assert!(c.cached(1).is_some_and(|p| !p.is_dirty()), "a page only read through the mapping stays clean");
    let r = Reclaimer::of(&[c]);
    let o = r.shrink(10, Mode::Background, true);
    assert!(o.freed == 1 && o.dirty >= 1, "{o:?}");
    assert!(c.cached(0).is_some(), "the dirty page stays");
    c.sync_all(SyncKind::Fsync).unwrap();
    assert_eq!(r.shrink(10, Mode::Background, true).freed, 1, "clean again: it goes");
    assert!(c.cached(0).is_none());
    assert_eq!(&read8(c, 0), b"second..", "the store reached the device");
}

/// A cache of only dirty pages has nothing to take: the call frees nothing and says so without `cut` (the
/// allocator then stops its rounds; kswapd reports kswapd.nothing_reclaimable); after write-back they go.
#[test]
fn dirty_pages_wait_for_write_back() {
    use crate::pagecache::SyncKind;
    let disk = Arc::new(Disk::default());
    let c = PageCache::with_backend(disk.clone(), 0);
    for i in 0..16u64 {
        c.write_at(i * P, &(i + 1).to_le_bytes()).unwrap();
    }
    let r = Reclaimer::of(&[&c]);
    let o = r.shrink(16, Mode::Direct, true);
    assert_eq!((o.freed, o.dirty, o.cut), (0, 32, false), "two turns over 16 dirty pages: {o:?}");
    c.sync_all(SyncKind::Fsync).unwrap();
    assert_eq!(disk.writes.load(Ordering::Relaxed), 16);
    let o = r.shrink(16, Mode::Direct, true);
    assert_eq!(o.freed, 16, "{o:?}");
    for i in 0..16u64 {
        assert_eq!(read8(&c, i * P), (i + 1).to_le_bytes(), "page {i} reads back from the device");
    }
}

/// Fixer finding 3 (B4): a thorough direct call looks at `DIRECT_SCAN` pages at most and says it was cut; the
/// next call goes on where it stopped, so a few calls find a page that one call cannot reach.
#[test]
fn a_thorough_direct_call_is_bounded() {
    use crate::reclaim::DIRECT_SCAN;
    let n = 3 * DIRECT_SCAN;
    let (c, _b) = file(n);
    read_all(&c, n);
    let r = Reclaimer::of(&[&c]);
    let mut calls = 0;
    loop {
        calls += 1;
        let o = r.shrink(1, Mode::Direct, true);
        assert!(o.scanned <= DIRECT_SCAN, "call {calls}: {o:?}");
        if o.freed == 1 {
            break;
        }
        assert!(o.cut && o.scanned + 64 >= DIRECT_SCAN, "call {calls} stopped early without a page: {o:?}");
        assert!(calls < 5, "the hand does not go on: {o:?}");
    }
    assert!(calls >= 2, "one bounded call cannot pass {n} used pages");
}

/// Fixer finding 4: reclaim racing reads, writes, truncates, mappings and write-back of one file: every read
/// sees the right bytes, the writer's last values survive, and the cache ends empty once everything is clean.
#[test]
fn reclaim_races_the_file_calls() {
    extern crate std;
    use crate::pagecache::SyncKind;
    use core::sync::atomic::AtomicBool;
    const FIXED: u64 = 32;
    const WRITTEN: u64 = 16;
    const ROUNDS: u64 = 2000;
    let disk = Arc::new(Disk::default());
    let c = PageCache::with_backend(disk.clone(), 0);
    for i in 0..FIXED {
        c.write_at(i * P, &i.to_le_bytes()).unwrap();
    }
    c.truncate(64 * P).unwrap();
    c.sync_all(SyncKind::Fsync).unwrap();
    let r = Reclaimer::of(&[&c]);
    let stop = AtomicBool::new(false);
    let taken = AtomicU64::new(0);
    std::thread::scope(|s| {
        s.spawn(|| {
            let mut k = 0u64;
            while !stop.load(Ordering::Relaxed) {
                let mode = if k % 2 == 0 { Mode::Direct } else { Mode::Background };
                taken.fetch_add(r.shrink(8 + k % 64, mode, k % 3 == 0).freed, Ordering::Relaxed);
                k += 1;
                std::thread::yield_now();
            }
        });
        s.spawn(|| {
            while !stop.load(Ordering::Relaxed) {
                c.sync_all(SyncKind::Background).unwrap();
                std::thread::yield_now();
            }
        });
        let readers: vec::Vec<_> = (0..2)
            .map(|t| {
                let c = &c;
                s.spawn(move || {
                    for k in 0..ROUNDS {
                        let i = (k * 7 + t) % FIXED;
                        assert_eq!(read8(c, i * P), i.to_le_bytes(), "read of page {i}");
                    }
                })
            })
            .collect();
        let mapper = s.spawn(|| {
            for k in 0..ROUNDS {
                let i = (k * 5) % FIXED;
                let p = c.page_for_map(i, false).unwrap();
                let mut v = [0u8; 8];
                p.read(0, &mut v);
                assert_eq!(v, i.to_le_bytes(), "mapped page {i}");
            }
        });
        let writer = s.spawn(|| {
            for k in 0..ROUNDS {
                let i = FIXED + k % WRITTEN;
                let v = (k << 8 | i).to_le_bytes();
                c.write_at(i * P, &v).unwrap();
                assert_eq!(read8(&c, i * P), v, "read after write of page {i}");
            }
        });
        let truncater = s.spawn(|| {
            for _ in 0..ROUNDS / 4 {
                c.truncate((FIXED + WRITTEN) * P).unwrap();
                c.truncate(64 * P).unwrap();
                assert_eq!(read8(&c, 60 * P), [0; 8], "past the cut: zeros");
            }
        });
        for h in readers {
            h.join().unwrap();
        }
        for h in [mapper, writer, truncater] {
            h.join().unwrap();
        }
        stop.store(true, Ordering::Relaxed);
    });
    assert!(taken.load(Ordering::Relaxed) > 0, "reclaim took nothing during the race");
    c.sync_all(SyncKind::Fsync).unwrap();
    assert_eq!(c.dirty_pages(), 0);
    let left = c.pages();
    let o = r.shrink(u64::MAX, Mode::Background, true);
    assert_eq!((o.freed, c.pages()), (left, 0), "every page clean and unmapped: all go ({o:?})");
    for i in 0..FIXED {
        assert_eq!(read8(&c, i * P), i.to_le_bytes(), "page {i} after the race");
    }
    for i in FIXED..FIXED + WRITTEN {
        let k = (ROUNDS - WRITTEN..ROUNDS).find(|k| FIXED + k % WRITTEN == i).unwrap();
        assert_eq!(read8(&c, i * P), (k << 8 | i).to_le_bytes(), "the writer's last value of page {i}");
    }
}
