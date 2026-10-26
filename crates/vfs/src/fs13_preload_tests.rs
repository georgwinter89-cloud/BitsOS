//! FS-13 (preload) host tests: page ranges merge, the record's format (round trip, damage ignored, bounds,
//! decay), the recorder's sessions, and `PageCache::prefetch` over a test backend.

extern crate std;

use crate::pagecache::{Backend, CachePage, PageCache};
use crate::preload::record::{self, Bad, FileRec, Ident, Record, FADE, ONE};
use crate::preload::recorder::{Finished, Keep, Recorder};
use crate::preload::ranges::{Ranges, GAP};
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};
use ksync::SpinLock;

const P: u64 = 4096;

fn fr(path: Vec<u8>, ranges: Ranges) -> FileRec {
    FileRec { path, id: Ident::default(), ranges }
}

/// The finished sessions as the service gets them (taken under the lock, turned over after it).
fn fin<K: Keep>(rec: &mut Recorder<K>, now: u64, all: bool) -> Vec<Finished> {
    rec.finish(now, all).into_iter().filter_map(|t| t.into_finished().unwrap()).collect()
}

fn r(v: &[(u64, u64)]) -> Ranges {
    let mut x = Ranges::new();
    for &(a, b) in v {
        assert!(x.add(a, b));
    }
    x
}

#[test]
fn ranges_merge_overlaps_neighbours_and_small_gaps() {
    let mut x = Ranges::new();
    assert!(x.add(100, 110));
    assert!(x.add(0, 4));
    assert!(x.add(105, 120)); // overlap
    assert!(x.add(120, 121)); // touching
    assert!(x.add(121 + GAP, 130)); // a gap of GAP pages is read anyway
    assert!(x.add(130 + GAP + 1, 150)); // one more page apart stays apart
    assert!(x.add(5, 5)); // empty: nothing
    assert_eq!(x.as_slice(), &[(0, 4), (100, 130), (139, 150)]);
    assert!(x.add(2, 140)); // swallows the middle and joins the last
    assert_eq!(x.as_slice(), &[(0, 150)]);
    assert_eq!(x.pages(), 150);
}

#[test]
fn ranges_cut_keeps_the_lowest_pages() {
    let mut x = r(&[(0, 10), (100, 110), (200, 210)]);
    assert_eq!(x.cut_to(15), 15);
    assert_eq!(x.as_slice(), &[(0, 10), (100, 105)]);
    assert_eq!(x.cut_to(0), 15);
    assert!(x.is_empty());
}

fn sample() -> Record {
    let mut rec = Record::new();
    assert!(rec.add_start(b"/boot/BITSOS2/brave/brave", vec![fr(b"/boot/BITSOS2/brave/brave".to_vec(), r(&[(0, 64), (1000, 2000)])), fr(b"/boot/BITSOS2/brave/resources.pak".to_vec(), r(&[(5, 6)]))], u64::MAX));
    assert!(rec.add_start(b"/bin/cat", vec![fr(b"/boot/big".to_vec(), r(&[(0, 300)]))], u64::MAX));
    assert!(rec.add_start(b"/boot/BITSOS2/brave/brave", Vec::new(), u64::MAX));
    rec
}

#[test]
fn record_round_trip_most_used_first() {
    let rec = sample();
    let b = rec.encode(1 << 20).unwrap();
    let back = Record::decode(&b).unwrap();
    let names: Vec<&[u8]> = back.programs.iter().map(|p| &p.exe[..]).collect();
    assert_eq!(names, [&b"/boot/BITSOS2/brave/brave"[..], b"/bin/cat"]);
    assert_eq!(back.programs[0].score, 2 * ONE);
    assert_eq!(back.programs[0].files[0].ranges.as_slice(), &[(0, 64), (1000, 2000)]);
    assert_eq!(back.programs[0].files[1].path, b"/boot/BITSOS2/brave/resources.pak");
    assert_eq!(back.pages(), rec.pages());
    assert_eq!(record::crc32(b"123456789"), 0xCBF4_3926);
}

#[test]
fn damaged_records_are_refused_never_a_panic() {
    let b = sample().encode(1 << 20).unwrap();
    assert_eq!(Record::decode(&[]), Err(Bad::Short));
    assert_eq!(Record::decode(&b[..b.len() - 1]).err(), Some(Bad::Checksum));
    let mut m = b.clone();
    m[0] = b'X';
    assert_eq!(Record::decode(&m), Err(Bad::Magic));
    let mut v = b.clone();
    v[4] = 9;
    assert_eq!(Record::decode(&v), Err(Bad::Version(9)));
    let mut f = b.clone();
    let mid = f.len() / 2;
    f[mid] ^= 0x40;
    assert_eq!(Record::decode(&f), Err(Bad::Checksum));
    // A program count far past the bytes with a good checksum: Length, no huge allocation.
    let mut h = b[..b.len() - 4].to_vec();
    h[12..16].copy_from_slice(&u32::MAX.to_le_bytes());
    let c = record::crc32(&h);
    h.extend_from_slice(&c.to_le_bytes());
    assert_eq!(Record::decode(&h), Err(Bad::Length));
    // Every cut and every single flipped bit fails cleanly.
    for n in 0..b.len() {
        assert!(Record::decode(&b[..n]).is_err());
    }
    for i in 0..b.len() {
        let mut x = b.clone();
        x[i] ^= 1;
        assert!(Record::decode(&x).is_err(), "bit flip at {i} accepted");
    }
}

#[test]
fn decay_lets_old_habits_fade() {
    let mut rec = sample();
    rec.decay();
    assert_eq!(rec.boots, 1);
    assert_eq!(rec.programs[1].score, ONE * 3 / 4);
    // cat started once: forgotten after a few boots without a start; brave started twice lasts longer.
    let mut boots = 1;
    while rec.programs.iter().any(|p| p.exe == b"/bin/cat") {
        rec.decay();
        boots += 1;
    }
    assert_eq!(boots, 8);
    assert!(rec.programs.iter().any(|p| p.exe == b"/boot/BITSOS2/brave/brave"));
    assert!(rec.programs.iter().all(|p| p.score >= FADE));
    // A program used every boot settles near four starts' worth and stays first.
    let mut daily = Record::new();
    for _ in 0..50 {
        daily.decay();
        daily.add_start(b"/a", vec![fr(b"/f".to_vec(), r(&[(0, 1)]))], u64::MAX);
    }
    assert!(daily.programs[0].score > 3 * ONE && daily.programs[0].score <= 4 * ONE);
}

#[test]
fn record_bounds_pages_per_program_and_bytes() {
    let mut rec = Record::new();
    rec.add_start(b"/p", vec![fr(b"/a".to_vec(), r(&[(0, 100)])), fr(b"/b".to_vec(), r(&[(0, 100)]))], 150);
    assert_eq!(rec.programs[0].pages(), 150);
    assert_eq!(rec.programs[0].files[1].ranges.as_slice(), &[(0, 50)]);
    // A later start cannot grow it past the bound; a file past it goes.
    rec.add_start(b"/p", vec![fr(b"/c".to_vec(), r(&[(0, 10)]))], 150);
    assert_eq!(rec.programs[0].pages(), 150);
    assert_eq!(rec.programs[0].files.len(), 2);
    // Byte bound: many programs, the most-used ones are kept.
    let mut big = Record::new();
    for i in 0..400u32 {
        let name = std::format!("/prog/{i:04}").into_bytes();
        let ranges = r(&(0..20u64).map(|k| (k * 100, k * 100 + 1)).collect::<Vec<_>>());
        for _ in 0..(i % 5) + 1 {
            big.add_start(&name, vec![fr(name.clone(), ranges.clone())], u64::MAX);
        }
    }
    let limit = record::bytes_for(0);
    assert_eq!(limit, 64 * 1024);
    let b = big.encode(limit).unwrap();
    assert!(b.len() <= limit);
    let back = Record::decode(&b).unwrap();
    assert!(back.programs.len() < 400 && !back.programs.is_empty());
    let lowest_kept = back.programs.iter().map(|p| p.score).min().unwrap();
    let kept: Vec<&[u8]> = back.programs.iter().map(|p| &p.exe[..]).collect();
    assert!(big.programs.iter().filter(|p| !kept.contains(&&p.exe[..])).all(|p| p.score <= lowest_kept));
    // A program without pages is not written; an empty record is valid.
    let mut warm = Record::new();
    warm.add_start(b"/warm", Vec::new(), u64::MAX);
    assert!(Record::decode(&warm.encode(limit).unwrap()).unwrap().programs.is_empty());
    assert_eq!(record::bytes_for(1024), 1 << 20);
}

/// A keep-alive that counts its drops (the recorder must hand keeps out, never drop them under its lock).
struct Kp(Arc<AtomicUsize>);

impl Drop for Kp {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

impl Keep for Kp {
    fn ident(&self) -> Option<Ident> {
        Some(Ident { size: 7, mtime_sec: 1, mtime_nsec: 2 })
    }
}

#[test]
fn sessions_join_fork_and_finish_in_reading_order() {
    let drops = Arc::new(AtomicUsize::new(0));
    let k = || Kp(drops.clone());
    let mut rec: Recorder<Kp> = Recorder::new();
    let w = 20;
    assert!(rec.exec(10, b"/brave", 100, w));
    assert!(!rec.exec(11, b"/brave", 105, w)); // a child exec of the same program joins
    rec.fork(11, 12);
    assert!(rec.exec(20, b"/other", 110, w));
    assert_eq!(rec.stats.starts, 2);
    assert_eq!(rec.stats.execs, 3);
    for (pid, cache, path) in [(10u32, 1usize, &b"/brave"[..]), (11, 2, b"/lib.so"), (12, 3, b"/font")] {
        assert!(rec.wants(pid, cache));
        assert!(rec.open(pid, cache, path.to_vec(), k()).is_none());
        assert!(!rec.wants(pid, cache));
    }
    // Not stored (known file, a process that records nothing): the keep comes back, nothing dropped inside.
    let back = rec.open(10, 1, b"/brave".to_vec(), k());
    assert!(back.is_some());
    let back2 = rec.open(99, 5, b"/y".to_vec(), k());
    assert!(back2.is_some());
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    drop((back, back2));
    rec.fill(12, 3, 0, 4, u64::MAX); // the forked child, first
    rec.fill(10, 1, 0, 32, u64::MAX);
    rec.fill(11, 1, 32, 64, u64::MAX); // another process of the session, the same file
    rec.fill(10, 9, 0, 1, u64::MAX); // a file nobody of the session opened
    rec.fill(99, 1, 0, 1, u64::MAX); // a process that records nothing
    assert_eq!(rec.stats.unknown, 1);
    assert!(rec.open(20, 7, b"/x".to_vec(), k()).is_none());
    assert!(fin(&mut rec, 119, false).is_empty());
    let before = drops.load(Ordering::SeqCst);
    let taken = rec.finish(120, false);
    assert_eq!(drops.load(Ordering::SeqCst), before, "keeps dropped inside finish (under the lock)");
    let done: Vec<Finished> = taken.into_iter().filter_map(|t| t.into_finished().unwrap()).collect();
    assert_eq!(drops.load(Ordering::SeqCst), before + 3);
    let id = Ident { size: 7, mtime_sec: 1, mtime_nsec: 2 };
    let f = |p: &[u8], v: &[(u64, u64)]| FileRec { path: p.to_vec(), id, ranges: r(v) };
    assert_eq!(done, vec![Finished { exe: b"/brave".to_vec(), files: vec![f(b"/font", &[(0, 4)]), f(b"/brave", &[(0, 64)])] }]);
    // The session's processes record nothing any more; the other program still does.
    rec.fill(10, 1, 100, 101, u64::MAX);
    assert_eq!(rec.active(), 1);
    rec.fill(20, 7, 5, 6, u64::MAX);
    let done = fin(&mut rec, 130, false);
    assert_eq!(done[0].files, vec![f(b"/x", &[(5, 6)])]);
    assert_eq!(rec.active(), 0);
}

#[test]
fn a_session_stops_at_its_page_budget_and_clear_drops_all() {
    let mut rec: Recorder<()> = Recorder::new();
    rec.exec(1, b"/p", 0, 10);
    assert!(rec.open(1, 1, b"/f".to_vec(), ()).is_none());
    rec.fill(1, 1, 0, 8, 10);
    rec.fill(1, 1, 8, 16, 10); // below the budget when it came: taken
    rec.fill(1, 1, 16, 20, 10); // past it
    assert_eq!(rec.stats.dropped, 1);
    let cleared = rec.clear();
    assert_eq!(cleared.len(), 1);
    assert!(cleared.into_iter().all(|t| t.into_finished() == Ok(None)), "a cleared session is handed over empty");
    assert_eq!(rec.active(), 0);
    assert!(fin(&mut rec, u64::MAX, true).is_empty());
}

#[test]
fn a_reused_pid_records_nothing_for_the_old_program() {
    let mut rec: Recorder<()> = Recorder::new();
    rec.exec(5, b"/brave", 0, 100);
    assert!(rec.open(5, 1, b"/brave".to_vec(), ()).is_none());
    // Pid 5 exits; an unrelated parent forks a child that gets pid 5 again.
    rec.fork(77, 5);
    assert!(!rec.wants(5, 2));
    rec.fill(5, 1, 0, 4, u64::MAX);
    // The exit hook does the same for a pid that is not forked again.
    rec.exec(6, b"/brave", 1, 100);
    assert!(rec.wants(6, 3));
    rec.exited(6);
    assert!(!rec.wants(6, 3));
    assert_eq!(rec.pids().count(), 0);
    let done = fin(&mut rec, 200, false);
    assert_eq!(done.len(), 1);
    assert!(done[0].files.is_empty(), "the new process's fill was recorded for /brave: {:?}", done[0].files);
}

#[test]
fn a_changed_file_replaces_its_old_ranges() {
    let mut rec = Record::new();
    let old = Ident { size: 100, mtime_sec: 1, mtime_nsec: 0 };
    let new = Ident { size: 120, mtime_sec: 2, mtime_nsec: 0 };
    rec.add_start(b"/p", vec![FileRec { path: b"/f".to_vec(), id: old, ranges: r(&[(0, 10), (50, 60)]) }], u64::MAX);
    rec.add_start(b"/p", vec![FileRec { path: b"/f".to_vec(), id: old, ranges: r(&[(80, 90)]) }], u64::MAX);
    assert_eq!(rec.programs[0].files[0].ranges.as_slice(), &[(0, 10), (50, 60), (80, 90)]);
    rec.add_start(b"/p", vec![FileRec { path: b"/f".to_vec(), id: new, ranges: r(&[(0, 5)]) }], u64::MAX);
    assert_eq!(rec.programs[0].files[0], FileRec { path: b"/f".to_vec(), id: new, ranges: r(&[(0, 5)]) });
    // The identity survives the stored form; a file can be forgotten.
    let back = Record::decode(&rec.encode(1 << 20).unwrap()).unwrap();
    assert_eq!(back.programs[0].files[0].id, new);
    rec.forget_file(b"/f");
    assert_eq!(rec.pages(), 0);
}

/// A record body as `encode` writes it, with a correct checksum: (score, exe, [(path, [(first, len)])]).
fn hand_made(programs: &[(u32, &[u8], &[(&[u8], &[(u64, u32)])])]) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(record::MAGIC);
    b.extend_from_slice(&record::VERSION.to_le_bytes());
    b.extend_from_slice(&0u16.to_le_bytes());
    b.extend_from_slice(&3u32.to_le_bytes());
    b.extend_from_slice(&(programs.len() as u32).to_le_bytes());
    for &(score, exe, files) in programs {
        b.extend_from_slice(&score.to_le_bytes());
        b.extend_from_slice(&(exe.len() as u16).to_le_bytes());
        b.extend_from_slice(exe);
        b.extend_from_slice(&(files.len() as u32).to_le_bytes());
        for &(path, ranges) in files {
            b.extend_from_slice(&(path.len() as u16).to_le_bytes());
            b.extend_from_slice(path);
            b.extend_from_slice(&[0u8; 20]);
            b.extend_from_slice(&(ranges.len() as u32).to_le_bytes());
            for &(a, n) in ranges {
                b.extend_from_slice(&a.to_le_bytes());
                b.extend_from_slice(&n.to_le_bytes());
            }
        }
    }
    let c = record::crc32(&b);
    b.extend_from_slice(&c.to_le_bytes());
    b
}

#[test]
fn hostile_contents_with_a_good_checksum_are_sanitised() {
    // Unsorted, overlapping, empty and huge ranges: merged and kept in bounds, no panic.
    let b = hand_made(&[(ONE, b"/p", &[(b"/f", &[(100, 10), (0, 5), (3, 50), (7, 0), (u64::MAX - 2, u32::MAX)])])]);
    let rec = Record::decode(&b).unwrap();
    assert_eq!(rec.programs[0].files[0].ranges.as_slice(), &[(0, 53), (100, 110), (u64::MAX - 2, u64::MAX)]);
    // Many huge ranges in many files: the page sums saturate instead of overflowing.
    let many: Vec<(u64, u32)> = (0..64u64).map(|k| (k << 40, u32::MAX)).collect();
    let files: Vec<(&[u8], &[(u64, u32)])> = (0..4).map(|_| (&b"/g"[..], &many[..])).collect();
    let b = hand_made(&[(ONE, b"/q", &files)]);
    let rec = Record::decode(&b).unwrap();
    assert!(rec.pages() > 0);
    // Relative paths, NULs and empty paths are left out; the rest stays.
    let b = hand_made(&[(ONE, b"/p", &[(b"rel/x", &[(0, 1)]), (b"/a\0b", &[(0, 1)]), (b"", &[(0, 1)]), (b"/ok", &[(0, 1)])])]);
    let rec = Record::decode(&b).unwrap();
    let paths: Vec<&[u8]> = rec.programs[0].files.iter().map(|f| &f.path[..]).collect();
    assert_eq!(paths, [&b"/ok"[..]]);
    // A range count past the bytes, a path length past the bytes: Length, no allocation from the count.
    let mut b = hand_made(&[(ONE, b"/p", &[(b"/f", &[(0, 1)])])]);
    b.truncate(b.len() - 4);
    let at = b.len() - 12 - 4;
    b[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    let c = record::crc32(&b);
    b.extend_from_slice(&c.to_le_bytes());
    assert_eq!(Record::decode(&b), Err(Bad::Length));
    let mut b = hand_made(&[(ONE, b"/p", &[])]);
    b.truncate(b.len() - 4);
    let at = 16 + 4;
    b[at..at + 2].copy_from_slice(&u16::MAX.to_le_bytes());
    let c = record::crc32(&b);
    b.extend_from_slice(&c.to_le_bytes());
    assert_eq!(Record::decode(&b), Err(Bad::Length));
}

#[test]
fn a_large_record_loads_whatever_the_budget_is_now() {
    // Written with max_mb 1024 (1 MiB of record), it stays readable under the fixed load ceiling.
    let mut big = Record::new();
    for i in 0..2000u32 {
        let name = std::format!("/prog/{i:05}").into_bytes();
        let ranges = r(&(0..30u64).map(|k| (k * 100, k * 100 + 1)).collect::<Vec<_>>());
        big.add_start(&name, vec![fr(name.clone(), ranges)], u64::MAX);
    }
    let b = big.encode(record::bytes_for(1024)).unwrap();
    assert!(b.len() > record::bytes_for(1) && b.len() <= record::bytes_for(1024));
    assert!(b.len() <= record::LOAD_MAX_BYTES);
    assert!(Record::decode(&b).is_ok());
}

#[test]
fn the_yield_waits_while_foreground_waits_and_is_bounded() {
    use core::cell::Cell;
    let clock = Cell::new(0u64);
    let queued = Cell::new(3u32);
    let mut sleeps = 0;
    // Three steps until the queue is empty.
    let waited = crate::preload::yield_to(
        &|| queued.get() > 0,
        &|| clock.get(),
        &mut |ns| {
            sleeps += 1;
            clock.set(clock.get() + ns);
            queued.set(queued.get() - 1);
        },
        2,
        250,
    );
    assert!(waited);
    assert_eq!((sleeps, clock.get()), (3, 6));
    // Never empty: it gives up at the bound.
    clock.set(0);
    let mut n = 0;
    assert!(crate::preload::yield_to(&|| true, &|| clock.get(), &mut |ns| {
        n += 1;
        clock.set(clock.get() + ns)
    }, 2, 250));
    assert_eq!(n, 125);
    // Nothing queued: no wait.
    assert!(!crate::preload::yield_to(&|| false, &|| 0, &mut |_| panic!("slept"), 2, 250));
}

/// Counts backend calls; pages hold their number.
struct Count {
    calls: SpinLock<Vec<(u64, u64)>>,
    window: u64,
}

impl Backend for Count {
    fn read_page(&self, pgoff: u64, page: &Arc<CachePage>) -> Result<(), i64> {
        self.read_pages(pgoff, core::slice::from_ref(page))
    }
    fn read_pages(&self, first: u64, pages: &[Arc<CachePage>]) -> Result<(), i64> {
        self.calls.lock().push((first, pages.len() as u64));
        for (i, p) in pages.iter().enumerate() {
            p.write(0, &[(first + i as u64) as u8]);
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

#[test]
fn prefetch_fills_missing_pages_and_stops_when_told() {
    let b = Arc::new(Count { calls: SpinLock::new(Vec::new()), window: 8 });
    let c = PageCache::with_backend(b.clone(), 1000 * P);
    let mut one = [0u8; 1];
    assert_eq!(c.read_at(20 * P, &mut one), Ok(1)); // pages 20.. cached by a reader
    b.calls.lock().clear();
    let mut calls = 0;
    let (ins, reads) = c.prefetch(0, 40, &mut |_| {
        calls += 1;
        true
    })
    .unwrap();
    assert_eq!(c.pages(), 40);
    assert_eq!(ins + 8, 40, "only pages not cached before count");
    assert_eq!(reads, calls);
    assert!(b.calls.lock().iter().all(|&(f, n)| f + n <= 20 || f >= 28));
    assert_eq!(c.read_at(39 * P, &mut one), Ok(1));
    assert_eq!(one[0], 39);
    // Cached already: no read, no question.
    let (ins, reads) = c.prefetch(0, 40, &mut |_| panic!("asked with nothing to read")).unwrap();
    assert_eq!((ins, reads), (0, 0));
    // Past the end of the file: clamped.
    let (ins, _) = c.prefetch(990, 5000, &mut |_| true).unwrap();
    assert_eq!(ins, 10);
    // `go` false: nothing read.
    let n = c.pages();
    let (ins, reads) = c.prefetch(500, 600, &mut |_| false).unwrap();
    assert_eq!((ins, reads, c.pages()), (0, 0, n));
    // A tmpfs file has nothing to preload.
    let t = PageCache::tmpfs(Arc::new(crate::pool::Pool::new("fs13-preload")));
    assert_eq!(t.prefetch(0, 10, &mut |_| true), Ok((0, 0)));
}

/// A backend that takes `ms` per call.
struct Slow {
    ms: u64,
    calls: SpinLock<Vec<(u64, u64)>>,
}

impl Backend for Slow {
    fn read_page(&self, pgoff: u64, page: &Arc<CachePage>) -> Result<(), i64> {
        self.read_pages(pgoff, core::slice::from_ref(page))
    }
    fn read_pages(&self, first: u64, pages: &[Arc<CachePage>]) -> Result<(), i64> {
        self.calls.lock().push((first, pages.len() as u64));
        std::thread::sleep(core::time::Duration::from_millis(self.ms));
        for (i, p) in pages.iter().enumerate() {
            p.write(0, &[(first + i as u64) as u8]);
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

#[test]
fn a_fault_does_not_wait_long_for_the_preloads_read() {
    use crate::pagecache::{BG_FILL_WAIT_NS, FILL_WAIT_NS};
    // The preload's read takes longer than a foreground miss's whole wait for another reader.
    let ms = FILL_WAIT_NS / 1_000_000 * 3;
    let b = Arc::new(Slow { ms, calls: SpinLock::new(Vec::new()) });
    let c = Arc::new(PageCache::with_backend(b.clone(), 64 * P));
    std::thread::scope(|s| {
        let c1 = c.clone();
        let pre = s.spawn(move || c1.prefetch(0, 64, &mut |_| true).unwrap());
        while b.calls.lock().is_empty() {
            std::thread::sleep(core::time::Duration::from_millis(1));
        }
        let t0 = std::time::Instant::now();
        c.page_for_map(5, false).unwrap();
        let took = t0.elapsed().as_nanos() as u64;
        // Its own read (ms) after the short wait, not FILL_WAIT_NS first.
        assert!(took < BG_FILL_WAIT_NS + ms * 1_000_000 + FILL_WAIT_NS / 2, "the fault took {} ms", took / 1_000_000);
        pre.join().unwrap();
    });
    let calls = b.calls.lock().clone();
    assert_eq!(calls[0], (0, 64), "the preload's read: {calls:?}");
    assert!(calls[1].0 <= 5 && calls[1].0 + calls[1].1 > 5, "the fault read its own window: {calls:?}");
    assert_eq!(c.pages(), 64);
    assert_eq!(c.dirty_pages(), 0);
}

std::thread_local! {
    /// The test's stand-in for "which process runs this thread" (other test threads: none).
    static PID: core::cell::Cell<Option<u32>> = const { core::cell::Cell::new(None) };
}

fn test_pid() -> Option<u32> {
    PID.with(|p| p.get())
}

/// The tests that drive the global recorder (`take_finished(true)` takes every running session) run one at a time.
fn global() -> std::sync::MutexGuard<'static, ()> {
    static G: std::sync::Mutex<()> = std::sync::Mutex::new(());
    G.lock().unwrap_or_else(|e| e.into_inner())
}

fn as_pid<R>(pid: u32, f: impl FnOnce() -> R) -> R {
    PID.with(|p| p.set(Some(pid)));
    let r = f();
    PID.with(|p| p.set(None));
    r
}

/// The hooks AB calls (`exec_started`, `forked`) and the ones in vfs (`Ctx::open`, the fill path) end to end,
/// over a file whose cache has a backend, in a view of its own: what the exec'd program and its forked child
/// read is recorded for the program, other processes' reads and a chroot'ed exec are not.
#[test]
fn exec_and_fork_hooks_record_the_programs_reads() {
    use crate::inode::{Cred, Data, Inode};
    use crate::mount_ns::{MountNs, Path};
    use crate::preload;
    let _g = global();
    preload::set_current_pid(test_pid);
    let ns = MountNs::new_empty();
    let root = ns.root_path();
    let link = |name: &[u8], ino: u64| {
        let b = Arc::new(Count { calls: SpinLock::new(Vec::new()), window: 4 });
        let inode = Inode::new(ino, uapi::S_IFREG | 0o755, Cred::ROOT, Data::Reg(PageCache::with_backend(b, 256 * P)));
        let d = root.dentry.link_child(name, inode).unwrap();
        Path { mnt: root.mnt.clone(), dentry: d }
    };
    let exe = link(b"fs13-prog", 913_001);
    let _lib = link(b"fs13-lib", 913_002);
    // A file under /run (a snapshot mount there in the kernel) is not kept: its Path would pin the mount.
    let run = root.dentry.link_child(b"run", Inode::new(913_004, uapi::S_IFDIR | 0o755, Cred::ROOT, Data::Dir)).unwrap();
    let rb = Arc::new(Count { calls: SpinLock::new(Vec::new()), window: 4 });
    run.link_child(b"fs13-snap", Inode::new(913_005, uapi::S_IFREG | 0o644, Cred::ROOT, Data::Reg(PageCache::with_backend(rb, 16 * P)))).unwrap();
    fn cache(p: &crate::mount_ns::Path) -> &PageCache {
        p.dentry.inode.page_cache().unwrap()
    }
    let (pid, child, other) = (913_000u32, 913_001u32, 913_002u32);
    // A chroot'ed exec (root is not the view's): nothing starts.
    let dir = root.dentry.link_child(b"fs13-dir", Inode::new(913_003, uapi::S_IFDIR | 0o755, Cred::ROOT, Data::Dir)).unwrap();
    let jail = Path { mnt: root.mnt.clone(), dentry: dir };
    let starts = preload::stats().starts;
    preload::exec_started(pid, &ns, &jail, &exe, None);
    assert_eq!(preload::stats().starts, starts, "a chroot'ed exec began a session");
    preload::exec_started(pid, &ns, &root, &exe, None);
    assert!(preload::active() >= 1);
    preload::forked(pid, child);
    let mut one = [0u8; 1];
    as_pid(pid, || cache(&exe).read_at(0, &mut one).unwrap());
    let held_before = (Arc::strong_count(&_lib.dentry), Arc::strong_count(&_lib.dentry.inode));
    as_pid(child, || {
        let f = crate::ops::Ctx::kernel(&ns).open(None, b"/fs13-lib", crate::abi::O_RDONLY, 0).unwrap();
        f.read(&mut one).unwrap();
        let f = crate::ops::Ctx::kernel(&ns).open(None, b"/run/fs13-snap", crate::abi::O_RDONLY, 0).unwrap();
        f.read(&mut one).unwrap();
    });
    // INT-W13: the session holds no strong reference (the file, its mount and its clusters stay free to go).
    assert_eq!((Arc::strong_count(&_lib.dentry), Arc::strong_count(&_lib.dentry.inode)), held_before, "the session holds the file open");
    as_pid(other, || cache(&exe).read_at(100 * P, &mut one).unwrap());
    let done: Vec<Finished> = preload::take_finished(true).into_iter().filter(|f| f.exe == b"/fs13-prog").collect();
    assert_eq!(done.len(), 1, "{done:?}");
    let id = preload::ident_of(&exe.dentry.inode);
    assert_eq!(id.size, 256 * P);
    assert_eq!(done[0].files, vec![FileRec { path: b"/fs13-prog".to_vec(), id, ranges: r(&[(0, 4)]) }, FileRec { path: b"/fs13-lib".to_vec(), id: preload::ident_of(&_lib.dentry.inode), ranges: r(&[(0, 4)]) }]);
}

/// FS-15: exec reads the program's and the interpreter's head (`prepare_binprm`) before AB's hook, while the
/// process records nothing yet (or for its old program): the hook adds that head to the new session. Fills no
/// record can hold are counted by kind: a file the session never opened, another process, a kernel thread.
#[test]
fn exec_records_the_head_it_read_before_the_hook_and_counts_what_no_record_holds() {
    use crate::inode::{Cred, Data, Inode};
    use crate::mount_ns::{MountNs, Path};
    use crate::preload;
    let _g = global();
    preload::set_current_pid(test_pid);
    let ns = MountNs::new_empty();
    let root = ns.root_path();
    let link = |name: &[u8], ino: u64| {
        let b = Arc::new(Count { calls: SpinLock::new(Vec::new()), window: 4 });
        let inode = Inode::new(ino, uapi::S_IFREG | 0o755, Cred::ROOT, Data::Reg(PageCache::with_backend(b, 256 * P)));
        let d = root.dentry.link_child(name, inode).unwrap();
        Path { mnt: root.mnt.clone(), dentry: d }
    };
    fn cache(p: &crate::mount_ns::Path) -> &PageCache {
        p.dentry.inode.page_cache().unwrap()
    }
    let (exe, ld, unopened) = (link(b"fs15-prog", 915_001), link(b"fs15-ld", 915_002), link(b"fs15-unopened", 915_003));
    let (pid, other) = (915_000u32, 915_010u32);
    let mut head = [0u8; 256];
    // prepare_binprm and the interpreter's headers: before the hook, by a process in no session.
    as_pid(pid, || {
        cache(&exe).read_at(0, &mut head).unwrap();
        cache(&ld).read_at(0, &mut head).unwrap();
    });
    preload::exec_started(pid, &ns, &root, &exe, Some(&ld));
    let s0 = preload::stats();
    let mut one = [0u8; 1];
    as_pid(pid, || cache(&unopened).read_at(0, &mut one).unwrap());
    as_pid(other, || cache(&exe).read_at(100 * P, &mut one).unwrap());
    cache(&exe).read_at(200 * P, &mut one).unwrap();
    let s1 = preload::stats();
    assert!(s1.unknown_pages >= s0.unknown_pages + 4 && s1.unknown > s0.unknown, "{s0:?} {s1:?}");
    assert!(s1.other_pages >= s0.other_pages + 4, "{s0:?} {s1:?}");
    assert!(s1.kthread_pages >= s0.kthread_pages + 4, "{s0:?} {s1:?}");
    let done: Vec<Finished> = preload::take_finished(true).into_iter().filter(|f| f.exe == b"/fs15-prog").collect();
    assert_eq!(done.len(), 1, "{done:?}");
    let id = |p: &Path| preload::ident_of(&p.dentry.inode);
    assert_eq!(
        done[0].files,
        vec![FileRec { path: b"/fs15-prog".to_vec(), id: id(&exe), ranges: r(&[(0, 4)]) }, FileRec { path: b"/fs15-ld".to_vec(), id: id(&ld), ranges: r(&[(0, 4)]) }],
        "the heads exec read before the hook belong to the start"
    );
}

/// FS-15: a changed file the start read whole is read whole again at its new size; one read in part, one that
/// grew far, or an empty one stays stale.
#[test]
fn a_changed_file_read_whole_is_read_whole_again() {
    let rec = |size: u64, v: &[(u64, u64)]| FileRec { path: b"/data/appdata/brave/Default/Preferences".to_vec(), id: Ident { size, mtime_sec: 1, mtime_nsec: 0 }, ranges: r(v) };
    let now = |size: u64| Ident { size, mtime_sec: 2, mtime_nsec: 0 };
    assert_eq!(rec(10 * P, &[(0, 10)]).whole_again(&now(12 * P + 1)), Some((0, 13)));
    assert_eq!(rec(10 * P - 5, &[(0, 12)]).whole_again(&now(3)), Some((0, 1)), "a window past the end is still whole");
    assert_eq!(rec(10 * P, &[(0, 9)]).whole_again(&now(10 * P)), None, "not read to its end");
    assert_eq!(rec(10 * P, &[(0, 4), (20, 30)]).whole_again(&now(10 * P)), None, "read in parts");
    assert_eq!(rec(10 * P, &[(1, 10)]).whole_again(&now(10 * P)), None, "not from its start");
    assert_eq!(rec(10 * P, &[(0, 10)]).whole_again(&now(277 * P)), None, "grew past twice its size and 1 MiB");
    assert_eq!(rec(10 * P, &[(0, 10)]).whole_again(&now(276 * P)), Some((0, 276)));
    assert_eq!(rec(10 * P, &[(0, 10)]).whole_again(&now(0)), None, "empty now");
    assert_eq!(rec(0, &[(0, 1)]).whole_again(&now(P)), None, "was empty");
}

/// FS-15: what a process read of the next program's file while it still recorded for the old one (exec's
/// header read, the loader's bss page) joins the new session once the exec hook noted the file; its other unknown
/// fills go at the exec, and a process that exits or whose session ends leaves nothing behind.
#[test]
fn an_exec_hands_the_fills_before_its_hook_to_the_new_session() {
    let mut rec: Recorder<()> = Recorder::new();
    assert!(rec.exec(30, b"/bin/sh", 0, 100));
    rec.fill(30, 7, 0, 32, u64::MAX); // exec's head read of /prog, unknown to the sh session
    rec.fill(30, 7, 270, 288, u64::MAX); // the loader's page near the end
    rec.fill(30, 9, 0, 4, u64::MAX); // another file sh never opened
    rec.fill(31, 7, 0, 1, u64::MAX); // a process that records nothing: not kept
    assert_eq!((rec.stats.unknown, rec.stats.unknown_pages, rec.stats.other_pages), (3, 54, 1));
    assert!(rec.exec(30, b"/prog", 10, 100));
    assert!(rec.open(30, 7, b"/prog".to_vec(), ()).is_none());
    rec.exec_done(30, u64::MAX);
    assert_eq!(rec.stats.unknown_pages, 4, "the adopted pages are not unknown any more");
    // Gone with the exec: a later open of file 9 by the new program does not get the old program's fill.
    assert!(rec.open(30, 9, b"/other".to_vec(), ()).is_none());
    rec.exec_done(30, u64::MAX);
    // A pid that exits leaves no pending fills.
    rec.exec(40, b"/bin/sh", 10, 100); // joins the running sh session
    rec.fill(40, 11, 0, 2, u64::MAX);
    rec.exited(40);
    assert!(rec.exec(40, b"/prog2", 11, 100));
    assert!(rec.open(40, 11, b"/prog2".to_vec(), ()).is_none());
    rec.exec_done(40, u64::MAX);
    let done = fin(&mut rec, 1000, true);
    let prog = done.iter().find(|f| f.exe == b"/prog").unwrap();
    assert_eq!(prog.files, vec![fr(b"/prog".to_vec(), r(&[(0, 32), (270, 288)]))]);
    assert!(done.iter().find(|f| f.exe == b"/prog2").unwrap().files.is_empty());
}

/// FS-15 fixer F1/F3: the hand-over keeps the session's page budget (what is cut counts as dropped, not unknown),
/// pending fills never hold more than the budget has left, and a hand-over that runs out of memory counts the pages
/// it lost as dropped.
#[test]
fn the_exec_hand_over_keeps_the_budget_and_counts_what_it_loses() {
    let mut rec: Recorder<()> = Recorder::new();
    assert!(rec.exec(50, b"/bin/sh", 0, 100));
    rec.fill(50, 7, 0, 10, 16);
    rec.fill(50, 7, 100, 110, 16); // 20 pages pending: more than the budget of 16 has left
    assert_eq!((rec.stats.unknown_pages, rec.pending_pages()), (20, 10), "the second fill does not fit the budget");
    assert!(rec.exec(50, b"/prog", 1, 100));
    assert!(rec.open(50, 7, b"/prog".to_vec(), ()).is_none());
    rec.fill(50, 7, 200, 210, 16); // the new session reads 10 pages itself: 6 left of its budget
    rec.exec_done(50, 16);
    assert_eq!(rec.pending_pages(), 0);
    assert_eq!((rec.stats.unknown_pages, rec.stats.dropped_pages), (10, 4), "6 of the 10 handed over, 4 cut");
    // A hand-over without memory: its pages count as dropped.
    rec.exec(51, b"/bin/sh", 2, 100); // joins the sh session
    rec.fill(51, 8, 0, 3, u64::MAX);
    rec.fill(51, 8, 50, 53, u64::MAX);
    assert!(rec.exec(51, b"/prog3", 3, 100));
    assert!(rec.open(51, 8, b"/prog3".to_vec(), ()).is_none());
    rec.fill(51, 8, 0, 1, u64::MAX); // the first run then merges; the second needs a new entry and finds no memory
    crate::preload::ranges::fail_new_entries(1);
    rec.exec_done(51, u64::MAX);
    assert_eq!((rec.stats.unknown_pages, rec.stats.dropped_pages), (10, 7), "3 handed over, 3 lost");
    let done = fin(&mut rec, 1000, true);
    let prog = done.iter().find(|f| f.exe == b"/prog").unwrap();
    assert_eq!(prog.files, vec![fr(b"/prog".to_vec(), r(&[(0, 6), (200, 210)]))]);
    assert_eq!(prog.files.iter().map(|f| f.ranges.pages()).sum::<u64>(), 16, "exactly the budget");
    let p3 = done.iter().find(|f| f.exe == b"/prog3").unwrap();
    assert_eq!(p3.files, vec![fr(b"/prog3".to_vec(), r(&[(0, 3)]))]);
    assert_eq!(rec.pending_pages(), 0);
}

/// FS-15 fixer F2: two caches never share a key, also when the first is freed and the second takes its memory.
#[test]
fn a_freed_cache_does_not_lend_its_key_to_the_next() {
    let backend = || Arc::new(Count { calls: SpinLock::new(Vec::new()), window: 8 });
    let a = PageCache::with_backend(backend(), 4 * 4096);
    let ka = a.ident();
    drop(a);
    let b = PageCache::with_backend(backend(), 4 * 4096);
    assert_ne!(ka, b.ident());
}

/// FS-13b: `all` is the default and has no page budget, while what one start records and the record's size stay
/// finite; 0 pauses; an explicit number is a cap; the record never gets larger than the load ceiling.
#[test]
fn all_free_memory_is_the_default_and_the_record_stays_bounded() {
    use crate::preload;
    let _g = global();
    assert_eq!(preload::MAX_MB_DEFAULT, preload::ALL);
    for (text, want) in [("all", Some(preload::ALL)), ("ALL", Some(preload::ALL)), ("0", Some(0)), ("1024", Some(1024)), ("1048576", Some(1 << 20))] {
        assert_eq!(preload::parse_max_mb(text), want, "{text}");
    }
    for bad in ["", "lots", "-1", "1048577", "99999999999", "all1", "18446744073709551615"] {
        assert_eq!(preload::parse_max_mb(bad), None, "{bad}");
    }
    let old = preload::max_mb();
    preload::set_max_mb(preload::ALL);
    assert!(preload::recording() || !preload::enabled());
    assert_eq!(preload::max_pages(), u64::MAX);
    // No frame allocator on the host: the old default bounds one start.
    assert_eq!(preload::record_pages(), 1024 * 256);
    assert_eq!(preload::record_mb(), 1024);
    preload::set_max_mb(2048);
    assert_eq!((preload::max_pages(), preload::record_pages()), (2048 * 256, 2048 * 256));
    preload::set_max_mb(0);
    assert!(!preload::recording());
    assert_eq!(preload::max_pages(), 0);
    // Out of range other than ALL is clamped (parse refuses it first).
    preload::set_max_mb(u64::MAX - 1);
    assert_eq!(preload::max_mb(), preload::MAX_MB_LIMIT);
    preload::set_max_mb(old);
    assert_eq!(record::bytes_for(u64::MAX), record::LOAD_MAX_BYTES);
    assert_eq!(record::bytes_for(32 * 1024), record::LOAD_MAX_BYTES);
    assert_eq!(record::bytes_for(16 * 1024), record::LOAD_MAX_BYTES);
}

#[test]
fn record_caps_follow_the_ram_and_the_stored_record_size() {
    use crate::preload::{self, record_pages_for};
    let (mb, gib) = (256u64, 256 * 1024u64);
    // bytes_for: 64 KiB at least, 1 KiB per MB, LOAD_MAX_BYTES (= 16384 MB) at most.
    assert_eq!(record::bytes_for(0), 64 * 1024);
    assert_eq!(record::bytes_for(1), 64 * 1024);
    assert_eq!(record::bytes_for(64), 64 * 1024);
    assert_eq!(record::bytes_for(65), 65 * 1024);
    assert_eq!(record::bytes_for(16 * 1024 - 1), record::LOAD_MAX_BYTES - 1024);
    assert_eq!(record::bytes_for(16 * 1024), record::LOAD_MAX_BYTES);
    assert_eq!(record::bytes_for(16 * 1024 + 1), record::LOAD_MAX_BYTES);
    // ALL: the RAM, but never more than the stored record holds (16 GiB at its density).
    assert_eq!(record_pages_for(preload::ALL, Some(512 * mb)), 512 * mb);
    assert_eq!(record_pages_for(preload::ALL, Some(8 * gib)), 8 * gib);
    assert_eq!(record_pages_for(preload::ALL, Some(32 * gib)), 16 * gib);
    assert_eq!(record_pages_for(preload::ALL, None), 1024 * mb);
    // An explicit cap: itself, at most the RAM and the same 16 GiB.
    assert_eq!(record_pages_for(2048, Some(32 * gib)), 2048 * mb);
    assert_eq!(record_pages_for(2048, Some(512 * mb)), 512 * mb);
    assert_eq!(record_pages_for(2048, None), 2048 * mb);
    assert_eq!(record_pages_for(preload::MAX_MB_LIMIT, None), 16 * gib);
    assert_eq!(record_pages_for(0, Some(32 * gib)), 0);
    // CONFIG and /sys values with their line ends and spaces.
    for (text, want) in [("all\r\n", Some(preload::ALL)), (" ALL ", Some(preload::ALL)), ("512\n", Some(512)), (" 0 ", Some(0))] {
        assert_eq!(preload::parse_max_mb(text), want, "{text:?}");
    }
    assert_eq!(preload::parse_max_mb(" \n"), None);
}

#[test]
fn a_save_that_cuts_programs_says_how_many_it_kept() {
    let mut big = Record::new();
    for i in 0..400u32 {
        let name = std::format!("/prog/{i:04}").into_bytes();
        let ranges = r(&(0..20u64).map(|k| (k * 100, k * 100 + 1)).collect::<Vec<_>>());
        big.add_start(&name, vec![fr(name.clone(), ranges)], u64::MAX);
    }
    assert_eq!(big.storable(), 400);
    let (all, kept) = big.encode_kept(1 << 20).unwrap();
    assert_eq!(kept, 400);
    assert_eq!(Record::decode(&all).unwrap().programs.len(), 400);
    let (cut, kept) = big.encode_kept(record::bytes_for(0)).unwrap();
    assert!(kept > 0 && kept < 400, "kept {kept}");
    assert_eq!(Record::decode(&cut).unwrap().programs.len(), kept);
}
