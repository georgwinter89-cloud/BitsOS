//! ktest `mm.migrate.writeback` (desk review of MM-2 x FS-W2, finding 1): page migration during a device write.
//! A backend whose `write_run` checksums its pages, parks on a gate (the device at work) and checksums them again
//! after it copied them to its storage. While it is parked, every page of the batch must stay where it is
//! (`migrate_frame` answers Stays, compaction of the whole machine moves none of them); after the gate opens, the
//! checksums and the storage hold exactly what was written and the clean pages move again. The second half does
//! the same inside `write_direct`, whose way-making writes the range's dirty pages first, and checks that the
//! cache holds nothing of the range while the direct write runs.

use crate::migrate;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering::SeqCst};
use ktest::{kassert, ktest, TestResult};
use mm_core::compact::Moved;
use mm_core::Kind;
use vfs::pagecache::{Backend, CachePage, PageCache, SyncKind};

const P: u64 = 4096;
const PAGES: u64 = 16;
const SEC: u64 = 1_000_000_000;

/// The "device": storage per page, a gate `write_run` parks on, checksums before and after the park.
struct Gated {
    store: ksync::Mutex<Vec<Vec<u8>>>,
    open: AtomicBool,
    parked: AtomicU64,
    before: ksync::Mutex<Vec<(u64, u64)>>,
    after: ksync::Mutex<Vec<(u64, u64)>>,
}

fn sum(b: &[u8]) -> u64 {
    b.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &x| (h ^ u64::from(x)).wrapping_mul(0x100_0000_01b3))
}

fn page_bytes(p: &CachePage) -> Vec<u8> {
    let mut v = alloc::vec![0u8; P as usize];
    p.read(0, &mut v);
    v
}

/// Page `i`'s bytes in generation `g`.
fn pattern(i: u64, g: u8) -> Vec<u8> {
    (0..P).map(|o| (o as u8) ^ (i as u8).wrapping_mul(37) ^ g).collect()
}

impl Backend for Gated {
    fn read_page(&self, pgoff: u64, page: &Arc<CachePage>) -> Result<(), vfs::Errno> {
        if let Some(b) = self.store.lock().get(pgoff as usize) {
            page.write(0, b);
        }
        Ok(())
    }
    fn write_run(&self, first: u64, pages: &[Arc<CachePage>], _size: u64) -> Result<(), vfs::Errno> {
        for (i, p) in pages.iter().enumerate() {
            self.before.lock().push((first + i as u64, sum(&page_bytes(p))));
        }
        self.parked.fetch_add(1, SeqCst);
        let end = time::now_ns() + 30 * SEC;
        while !self.open.load(SeqCst) && time::now_ns() < end {
            sched::sleep_ms(1);
        }
        for (i, p) in pages.iter().enumerate() {
            let b = page_bytes(p);
            self.after.lock().push((first + i as u64, sum(&b)));
            if let Some(s) = self.store.lock().get_mut((first + i as u64) as usize) {
                *s = b;
            }
        }
        Ok(())
    }
}

fn gated() -> Arc<Gated> {
    let store = (0..PAGES).map(|i| pattern(i, 0)).collect();
    Arc::new(Gated {
        store: ksync::Mutex::named("mm.kt.gated", store),
        open: AtomicBool::new(false),
        parked: AtomicU64::new(0),
        before: ksync::Mutex::named("mm.kt.gated", Vec::new()),
        after: ksync::Mutex::named("mm.kt.gated", Vec::new()),
    })
}

fn phys(c: &PageCache, i: u64) -> Option<u64> {
    c.cached(i).and_then(|p| p.phys())
}

/// While the backend is parked: each page in `range` stays where it is, by itself and through compaction.
fn stays_while_parked(c: &PageCache, b: &Gated, range: core::ops::Range<u64>) -> TestResult {
    let end = time::now_ns() + 10 * SEC;
    while b.parked.load(SeqCst) == 0 {
        kassert!(time::now_ns() < end, "the write-back did not reach the device within 10 s");
        sched::sleep_ms(1);
    }
    let at: Vec<Option<u64>> = range.clone().map(|i| phys(c, i)).collect();
    for (i, &p) in range.clone().zip(&at) {
        let Some(p) = p else { return ktest::fail(&format!("page {i} not cached during its write")) };
        let r = migrate::migrate_frame(p, Kind::PageCache);
        kassert!(r == Moved::Stays, "page {i} under the device write: {r:?}");
    }
    let s = crate::compact::compact_memory();
    let now: Vec<Option<u64>> = range.clone().map(|i| phys(c, i)).collect();
    kassert!(now == at, "a page under the device write moved (compaction moved {} pages)", s.moved);
    Ok(())
}

/// The device saw the same bytes before and after its park, every page of `range` and no other, and they are
/// what was written (generation `g`).
fn written_intact(b: &Gated, range: core::ops::Range<u64>, g: u8) -> TestResult {
    // One at a time: two locks of one class held together are a lock-order violation (sc.locks.no_unexpected).
    let before = b.before.lock().clone();
    let after = b.after.lock().clone();
    kassert!(!before.is_empty() && before == after, "checksums changed during the device write: {before:x?} -> {after:x?}");
    for &(i, s) in &after {
        kassert!(range.contains(&i), "page {i} outside {range:?} was written");
        kassert!(s == sum(&pattern(i, g)), "page {i} reached the device with other bytes than were written");
    }
    for i in range {
        kassert!(after.iter().any(|&(j, _)| j == i), "page {i} was not written");
    }
    Ok(())
}

fn spawn_on_other(name: &str, f: impl FnOnce() + Send + 'static) -> Result<sched::JoinHandle, String> {
    let aps = crate::tlb_ktest::aps();
    let Some(&cpu) = aps.first() else { return Err(String::from("one CPU: the writer needs another (run with -smp 2 or more)")) };
    let o = sched::SpawnOpts { cpu: Some(cpu), pin: true, ..sched::SpawnOpts::default() };
    sched::spawn_with(name, sched::Class::Normal, o, f).map_err(|x| format!("{x:?}"))
}

fn writeback() -> TestResult {
    let b = gated();
    let c = Arc::new(PageCache::with_backend(b.clone(), PAGES * P));
    for i in 0..PAGES {
        c.write_at(i * P, &pattern(i, 1)).map_err(|x| format!("write {i}: {x}"))?;
    }
    // 1. A write-back of the whole file, parked at the device.
    let done = Arc::new(AtomicU64::new(u64::MAX));
    let (c2, d2) = (c.clone(), done.clone());
    let h = spawn_on_other("mm-kt-wb", move || {
        d2.store(u64::from(c2.sync_range(0, PAGES - 1, SyncKind::Fsync).is_ok()), SeqCst);
    })?;
    let r = stays_while_parked(&c, &b, 0..PAGES);
    b.open.store(true, SeqCst);
    kassert!(h.join_timeout(30 * SEC), "the write-back did not end");
    r?;
    kassert!(done.load(SeqCst) == 1, "the write-back failed");
    written_intact(&b, 0..PAGES, 1)?;
    // Clean and only the index holds them: they move again, and read back the same.
    for i in 0..PAGES {
        let p = phys(&c, i).ok_or("a page left the cache after its write")?;
        let mut last = Moved::Stays;
        for _ in 0..1000 {
            last = migrate::migrate_frame(p, Kind::PageCache);
            if last == Moved::Moved {
                break;
            }
            sched::yield_now();
        }
        kassert!(last == Moved::Moved, "clean page {i} after its write: {last:?}");
        let mut v = alloc::vec![0u8; P as usize];
        c.read_at(i * P, &mut v).map_err(|x| format!("read {i}: {x}"))?;
        kassert!(v == pattern(i, 1), "page {i} reads wrong after its move");
    }

    // 2. A direct write of pages 4..=11, whose dirty pages go to the device first (parked there).
    let b2 = gated();
    let c = Arc::new(PageCache::with_backend(b2.clone(), PAGES * P));
    for i in 0..PAGES {
        c.write_at(i * P, &pattern(i, 2)).map_err(|x| format!("write {i}: {x}"))?;
    }
    let inside = Arc::new(AtomicU64::new(u64::MAX));
    let (c2, b3, in2, d2) = (c.clone(), b2.clone(), inside.clone(), done.clone());
    d2.store(u64::MAX, SeqCst);
    let h = spawn_on_other("mm-kt-direct", move || {
        let r = c2.write_direct(4, 11, || {
            // The cache holds none of the range now: nothing of it to move.
            in2.store((4..=11).filter(|&i| c2.cached(i).is_some()).count() as u64, SeqCst);
            let mut s = b3.store.lock();
            for i in 4..=11u64 {
                s[i as usize] = pattern(i, 3);
            }
            Ok(())
        });
        d2.store(u64::from(r.is_ok()), SeqCst);
    })?;
    let r = stays_while_parked(&c, &b2, 4..12);
    b2.open.store(true, SeqCst);
    kassert!(h.join_timeout(30 * SEC), "the direct write did not end");
    r?;
    kassert!(done.load(SeqCst) == 1, "the direct write failed");
    kassert!(inside.load(SeqCst) == 0, "{} pages of the range cached during the direct write", inside.load(SeqCst));
    written_intact(&b2, 4..12, 2)?;
    for i in 0..PAGES {
        let mut v = alloc::vec![0u8; P as usize];
        c.read_at(i * P, &mut v).map_err(|x| format!("read {i}: {x}"))?;
        let g = if (4..=11).contains(&i) { 3 } else { 2 };
        kassert!(v == pattern(i, g), "page {i} reads wrong after the direct write");
    }
    Ok(())
}
// QemuOnly: it compacts the whole machine (as `mm.migrate.compact`).
ktest!("mm.migrate.writeback", QemuOnly, writeback);
