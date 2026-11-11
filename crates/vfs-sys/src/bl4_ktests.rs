//! BL-4 ktests: read-ahead in the reader and on the per-CPU workers, mmap read-ahead and file fault-around, on a
//! recording backend (no device). All Safe and rerunnable: each test makes its own files and only waits a bounded
//! time for the workers. Every page read is checked whole (each byte is a pattern of its page and offset). The
//! read-ahead counters are system-wide (other programs read beside a Safe test on the Mini-PC), so the tests ask
//! for at least what they caused, never an exact delta.
//!
//! - `fs.bl4.window`: the window table of linux-7/mm/readahead.c and a 4 KiB reader's windows through the workers
//!   (sync 4, then async 8, 16, 32, 32), each async window at the reader's I/O priority.
//! - `fs.bl4.random`: standalone random reads are read as is, a read after them where the last one ended starts
//!   a window; POSIX_FADV_RANDOM turns windows off, SEQUENTIAL doubles the limit.
//! - `fs.bl4.error`: a failed window, a failed job, a file tail that is no window long.
//! - `mm.as.filefault.seq`: a private mapping touched in order: read-around, the mark's fault queues async
//!   windows, fault-around maps the cached neighbours (fewer faults than pages), the counters match a walk.
//! - `mm.as.filefault.random`: MADV_RANDOM: each fault reads its page alone, no window, no job.
//! - `mm.as.filefault.hints`: MADV_SEQUENTIAL, MADV_WILLNEED, the mark left to its fault, a write fault maps
//!   nothing around, code read forward, a fault past the end of the file.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use ktest::{kassert, ktest, TestResult};
use mm::{AddressSpace, Fill};
use vfs::inode::{Data, InodeOps};
use vfs::pagecache::{Advice, Backend, CachePage, PageCache};
use vfs::readahead::{self, Window};
use vfs::{Errno, OpenFile};

const P: u64 = 4096;
const PF_U: u64 = 4;
const PF_W: u64 = 2;
static RUN: AtomicU64 = AtomicU64::new(0);

fn e<T: core::fmt::Debug>(x: T) -> String {
    format!("{x:?}")
}

/// Byte `i` of file page `pg` on the recording device.
fn pat(pg: u64, i: usize) -> u8 {
    (pg.wrapping_mul(131).wrapping_add(i as u64)) as u8
}

/// A device that is not there: page `pg` holds `pat(pg, i)`; every backend call is recorded with the I/O
/// priority it ran at, also a call that fails (one that touches a page of `fail`: EIO).
struct Rec {
    calls: ksync::Mutex<Vec<(u64, u64, block::prio::IoPriority)>>,
    fail: ksync::Mutex<Vec<u64>>,
}

impl Backend for Rec {
    fn read_page(&self, pgoff: u64, page: &Arc<CachePage>) -> Result<(), Errno> {
        self.read_pages(pgoff, core::slice::from_ref(page))
    }
    fn read_pages(&self, first: u64, pages: &[Arc<CachePage>]) -> Result<(), Errno> {
        {
            let mut c = self.calls.lock();
            c.try_reserve(1).map_err(|_| uapi::errno::ENOMEM)?;
            c.push((first, pages.len() as u64, block::prio::current()));
        }
        let end = first + pages.len() as u64;
        if self.fail.lock().iter().any(|&f| first <= f && f < end) {
            return Err(uapi::errno::EIO);
        }
        let mut buf = Vec::new();
        buf.try_reserve_exact(P as usize).map_err(|_| uapi::errno::ENOMEM)?;
        buf.resize(P as usize, 0u8);
        for (k, p) in pages.iter().enumerate() {
            let pg = first + k as u64;
            for (i, b) in buf.iter_mut().enumerate() {
                *b = pat(pg, i);
            }
            p.write(0, &buf);
        }
        Ok(())
    }
    fn write_run(&self, _first: u64, _pages: &[Arc<CachePage>], _size: u64) -> Result<(), Errno> {
        Ok(())
    }
    fn read_ahead_pages(&self) -> u64 {
        readahead::DEFAULT_PAGES
    }
}

impl Rec {
    fn take(&self) -> Vec<(u64, u64)> {
        core::mem::take(&mut *self.calls.lock()).into_iter().map(|(f, n, _)| (f, n)).collect()
    }
    fn take_prio(&self) -> Vec<(u64, u64, block::prio::IoPriority)> {
        core::mem::take(&mut *self.calls.lock())
    }
    fn set_fail(&self, pages: &[u64]) {
        let mut f = self.fail.lock();
        f.clear();
        f.extend_from_slice(pages);
    }
}

fn cache(pages: u64) -> (PageCache, Arc<Rec>) {
    sized(pages * P)
}

fn sized(bytes: u64) -> (PageCache, Arc<Rec>) {
    let b = Arc::new(Rec { calls: ksync::Mutex::named("bl4.rec", Vec::new()), fail: ksync::Mutex::named("bl4.fail", Vec::new()) });
    (PageCache::with_backend(b.clone(), bytes), b)
}

fn until(ms: u64, mut f: impl FnMut() -> bool) -> bool {
    for _ in 0..ms {
        if f() {
            return true;
        }
        sched::sleep_ms(1);
    }
    f()
}

/// Checks `buf` against file pages from `pg` on (whole pages; a last part page as far as it goes).
fn same(buf: &[u8], pg: u64, what: &str) -> TestResult {
    for (k, chunk) in buf.chunks(P as usize).enumerate() {
        let p = pg + k as u64;
        if let Some(i) = chunk.iter().enumerate().position(|(i, &b)| b != pat(p, i)) {
            return Err(format!("{what}: page {p} byte {i} is {:#x}, want {:#x}", chunk[i], pat(p, i)));
        }
    }
    Ok(())
}

/// A 4 KiB read(2) of page `pg`, every byte checked.
fn byte(c: &PageCache, pg: u64) -> Result<(), String> {
    pages(c, pg, 1)
}

/// A read(2) of `n` pages from `pg`, every byte checked.
fn pages(c: &PageCache, pg: u64, n: u64) -> Result<(), String> {
    let mut w = Vec::new();
    w.try_reserve_exact((n * P) as usize).map_err(e)?;
    w.resize((n * P) as usize, 0u8);
    let got = c.read_at(pg * P, &mut w).map_err(|x| format!("read page {pg}: {x}"))?;
    kassert!(got == w.len(), "read page {pg}: {got} bytes");
    same(&w, pg, "read")
}

/// The exact value of a counter: per-CPU ones summed over their slots (`vfs::pcpu`), the others as listed.
fn counter(name: &str) -> u64 {
    if let Some(c) = vfs::pcpu::PCPU_COUNTERS.entries().iter().find(|c| c.0.name() == name) {
        return c.0.sum();
    }
    klog::metrics::snapshot().find(|(n, _)| *n == name).map_or(0, |(_, v)| v)
}

/// Counters before a step; `grew` asks each for at least its delta.
struct Ctr(Vec<(&'static str, u64)>);

impl Ctr {
    fn now(names: &[&'static str]) -> Ctr {
        Ctr(names.iter().map(|&n| (n, counter(n))).collect())
    }
    fn grew(&self, want: &[(&'static str, u64)]) -> TestResult {
        for &(n, d) in want {
            let before = self.0.iter().find(|x| x.0 == n).map_or(0, |x| x.1);
            let got = counter(n).wrapping_sub(before);
            kassert!(got >= d, "{n} grew by {got}, want at least {d}");
        }
        Ok(())
    }
}

const RA_CTRS: &[&str] = &[
    "vfs.readahead.sync",
    "vfs.readahead.async",
    "vfs.readahead.windows",
    "vfs.readahead.random",
    "vfs.readahead.pages",
    "vfs.readahead.stopped",
    "vfs.readahead.mmap_sync",
    "vfs.readahead.mmap_async",
    "mm.as.file_around",
];

fn workers() -> TestResult {
    if !readahead::active() {
        return ktest::fail("no read-ahead worker in this kernel");
    }
    Ok(())
}

// ---- fs.bl4.window

fn window() -> TestResult {
    // The arithmetic (readahead.c:390 get_init_ra_size, :408 get_next_ra_size, :690-717 the async window).
    for (req, want) in [(1u64, 4u64), (2, 4), (3, 8), (4, 8), (5, 16), (8, 16), (9, 32), (32, 32), (100, 32)] {
        kassert!(readahead::init_size(req, 32) == want, "init {req}: {}", readahead::init_size(req, 32));
    }
    for (cur, max, want) in [(1u64, 32u64, 4u64), (4, 32, 8), (8, 32, 16), (16, 32, 32), (32, 32, 32), (4, 128, 16), (64, 128, 128)] {
        kassert!(readahead::next_size(cur, max) == want, "next {cur} max {max}: {}", readahead::next_size(cur, max));
    }
    let mut w = Window::sync_seq(0, 1, 32);
    kassert!(w == Window { start: 0, size: 4, async_size: 3 }, "first window {w:?}");
    for (start, size) in [(4u64, 8u64), (12, 16), (28, 32), (60, 32)] {
        w = w.next(32);
        kassert!(w == Window { start, size, async_size: size }, "window {w:?}, want {start}+{size}");
    }
    kassert!(Window { start: 65, size: 6, async_size: 1 }.next(32).size == 9, "aligned end");
    workers()?;
    // A 4 KiB reader of a 128-page file, in a Normal thread: the miss reads [0, 4) itself, each mark queues the
    // next window on a worker, at Normal.
    let (c, b) = cache(128);
    let c = Arc::new(c);
    let cc = c.clone();
    let bb = b.clone();
    let k = Ctr::now(RA_CTRS);
    let r = in_class(sched::Class::Normal, 10_000, move || -> TestResult {
        byte(&cc, 0)?;
        kassert!(bb.take() == [(0, 4)], "the first window is the reader's");
        for (mark, first, end) in [(1u64, 4u64, 12u64), (4, 12, 28), (12, 28, 60), (28, 60, 92)] {
            byte(&cc, mark)?;
            kassert!(until(5000, || cc.cached(end - 1).is_some()), "mark {mark}: [{first}, {end}) not read ahead");
            let got = bb.take_prio();
            kassert!(got.len() == 1 && got[0].0 == first && got[0].1 == end - first, "mark {mark}: {got:?}");
            kassert!(got[0].2 == block::prio::IoPriority::Normal, "mark {mark}: a Normal reader's window went {:?}", got[0].2);
        }
        Ok(())
    })?;
    r?;
    // One sync window, four async ones of 8 + 16 + 32 + 32 pages; then every page of them, whole.
    k.grew(&[("vfs.readahead.sync", 1), ("vfs.readahead.windows", 4), ("vfs.readahead.async", 4), ("vfs.readahead.pages", 88)])?;
    // (That read reaches the last mark, 60: only the window after it, from 92, may go to the device.)
    pages(&c, 0, 92)?;
    let calls = b.take();
    kassert!(calls.iter().all(|c| c.0 >= 92), "the windows were cached: a read of them went to the device: {calls:?}");
    // A Background reader's window stays Background.
    let (c, b) = cache(64);
    let c = Arc::new(c);
    let (cc, bb) = (c.clone(), b.clone());
    in_class(sched::Class::Background, 10_000, move || -> TestResult {
        byte(&cc, 0)?;
        byte(&cc, 1)?;
        kassert!(until(5000, || cc.cached(11).is_some()), "no window");
        let got = bb.take_prio();
        kassert!(got.last().map(|x| x.2) == Some(block::prio::IoPriority::Background), "{got:?}");
        Ok(())
    })??;
    Ok(())
}
ktest!("fs.bl4.window", Safe, window);

fn in_class<T: Send + 'static>(class: sched::Class, ms: u64, f: impl FnOnce() -> T + Send + 'static) -> Result<T, String> {
    let out: Arc<ksync::Mutex<Option<T>>> = Arc::new(ksync::Mutex::named("bl4.out", None));
    let o = out.clone();
    let h = sched::spawn("bl4-ktest", class, move || {
        let v = f();
        *o.lock() = Some(v);
    })
    .map_err(e)?;
    if !h.join_timeout(ms * 1_000_000) {
        return Err(format!("the {class:?} thread did not finish in {ms} ms"));
    }
    out.lock().take().ok_or_else(|| String::from("the thread gave no result"))
}

// ---- fs.bl4.random

fn random() -> TestResult {
    workers()?;
    let k = Ctr::now(RA_CTRS);
    let (c, b) = cache(256);
    for pg in [200u64, 17, 90, 140, 33] {
        byte(&c, pg)?;
    }
    kassert!(b.take() == [(200, 1), (17, 1), (90, 1), (140, 1), (33, 1)], "random reads as asked");
    kassert!(readahead::pending() == 0 || until(1000, || readahead::pending() == 0), "jobs left");
    kassert!(b.take().is_empty(), "a random read queued a window");
    // Where the last read ended: sequential again.
    byte(&c, 34)?;
    kassert!(b.take() == [(34, 4)], "a read after the last one starts a window");
    // POSIX_FADV_RANDOM: even the file start reads only what is asked; SEQUENTIAL ramps past 32.
    let (c, b) = cache(512);
    c.advise(Advice::Random);
    byte(&c, 0)?;
    byte(&c, 1)?;
    kassert!(b.take() == [(0, 1), (1, 1)], "FADV_RANDOM");
    c.advise(Advice::Sequential);
    byte(&c, 2)?;
    kassert!(b.take() == [(2, 4)], "FADV_SEQUENTIAL: a window again");
    let mut mark = 3u64;
    let mut last = 0;
    for _ in 0..6 {
        byte(&c, mark)?;
        let w = until(5000, || !b.calls.lock().is_empty());
        kassert!(w, "no window after mark {mark}");
        let got = b.take();
        kassert!(got.len() == 1, "{got:?}");
        last = got[0].1;
        mark = got[0].0;
        kassert!(until(5000, || c.cached(mark + last - 1).is_some()), "window {got:?} not cached");
    }
    kassert!(last == 64, "the window grew to twice the backend's ({last})");
    // Five standalone reads and two after FADV_RANDOM read as asked.
    k.grew(&[("vfs.readahead.random", 7), ("vfs.readahead.windows", 6)])?;
    pages(&c, 0, mark + last)?;
    Ok(())
}
ktest!("fs.bl4.random", Safe, random);

// ---- fs.bl4.error

/// A read(2) of page `pg` that must fail with `errno` (one byte asked).
fn read_fails(c: &PageCache, pg: u64, errno: Errno) -> TestResult {
    let mut one = [0u8; 1];
    let r = c.read_at(pg * P, &mut one);
    kassert!(r == Err(errno), "read page {pg}: {r:?}, want errno {errno}");
    Ok(())
}

fn error() -> TestResult {
    workers()?;
    let k = Ctr::now(RA_CTRS);
    // (a) The reader's window fails: a bad neighbour does not fail the page (it is read alone, filemap_fault's
    // rule); a bad page answers EIO and leaves nothing in flight, so it reads once the device is good again.
    let (c, b) = cache(64);
    let (c, b) = (Arc::new(c), b);
    let (cc, bb) = (c.clone(), b.clone());
    in_class(sched::Class::Normal, 10_000, move || -> TestResult {
        bb.set_fail(&[2]);
        byte(&cc, 0)?;
        let calls = bb.take();
        kassert!(calls == [(0, 4), (0, 1)], "window, then the page alone: {calls:?}");
        bb.set_fail(&[40]);
        read_fails(&cc, 40, uapi::errno::EIO)?;
        read_fails(&cc, 40, uapi::errno::EIO)?;
        bb.set_fail(&[]);
        byte(&cc, 40)?;
        Ok(())
    })??;
    // (b) A job fails: the readers of its pages do not hang (they read the good ones themselves, the bad one
    // answers EIO), the job's note and the file's `queued` go (a later job runs).
    let (c, b) = cache(128);
    let (c, b) = (Arc::new(c), b);
    let (cc, bb) = (c.clone(), b.clone());
    let stopped = counter("vfs.readahead.stopped");
    in_class(sched::Class::Normal, 20_000, move || -> TestResult {
        byte(&cc, 0)?;
        bb.set_fail(&[6]);
        byte(&cc, 1)?;
        kassert!(until(5000, || counter("vfs.readahead.stopped") > stopped), "the failing job [4, 12) did not stop");
        byte(&cc, 4)?;
        byte(&cc, 5)?;
        read_fails(&cc, 6, uapi::errno::EIO)?;
        bb.set_fail(&[]);
        for pg in 6..12 {
            byte(&cc, pg)?;
        }
        // `queued` is clear again: a WILLNEED job runs (repeated while a job of the reads above still runs).
        kassert!(until(5000, || {
            cc.will_need(64 * P, 8 * P);
            cc.cached(71).is_some()
        }), "no job ran after the failed one");
        pages(&cc, 64, 8)?;
        Ok(())
    })??;
    // (c) A file whose tail is no window long (37 pages and 100 bytes): read to the end in 4 KiB steps, nothing
    // is read or cached past it, the last read gives the 100 bytes, a read at the end gives nothing.
    let (c, b) = sized(37 * P + 100);
    let (c, b) = (Arc::new(c), b);
    let (cc, bb) = (c.clone(), b.clone());
    in_class(sched::Class::Normal, 10_000, move || -> TestResult {
        for pg in 0..37 {
            byte(&cc, pg)?;
        }
        let mut w = [0u8; P as usize];
        kassert!(cc.read_at(37 * P, &mut w) == Ok(100), "the tail");
        same(&w[..100], 37, "tail")?;
        kassert!(cc.read_at(38 * P, &mut w) == Ok(0), "a read at the end");
        until(2000, || readahead::pending() == 0);
        let calls = bb.take();
        kassert!(calls.iter().all(|&(f, n)| f + n <= 38), "a read past the end: {calls:?}");
        kassert!(cc.cached(38).is_none(), "a page past the end is cached");
        Ok(())
    })??;
    k.grew(&[("vfs.readahead.stopped", 1)])?;
    Ok(())
}
ktest!("fs.bl4.error", Safe, error);

// ---- mm.as.filefault.*

/// A regular file on the recording backend, nameless on the memfd mount, open for reading.
struct Ops {
    cache: PageCache,
}

impl InodeOps for Ops {
    fn page_cache(&self) -> Option<&PageCache> {
        Some(&self.cache)
    }
    fn read_cache(&self) -> Option<&PageCache> {
        Some(&self.cache)
    }
    fn size(&self) -> u64 {
        self.cache.size()
    }
    fn read_at(&self, pos: u64, buf: &mut [u8]) -> Result<usize, Errno> {
        self.cache.read_at(pos, buf)
    }
    fn write_at(&self, _pos: u64, _buf: &[u8]) -> Result<usize, Errno> {
        Err(uapi::errno::EROFS)
    }
    fn truncate(&self, _size: u64) -> Result<(), Errno> {
        Err(uapi::errno::EROFS)
    }
}

fn open_file(pages: u64) -> Result<(Arc<OpenFile>, Arc<Rec>), String> {
    let (c, b) = cache(pages);
    let mnt = vfs::memfd::shm_mount();
    let name = format!("bl4-{}", RUN.fetch_add(1, Ordering::Relaxed)).into_bytes();
    let d = mnt.root.nameless_child(name, uapi::S_IFREG | 0o644, vfs::Cred::ROOT, Data::External(Box::new(Ops { cache: c }))).map_err(e)?;
    let path = vfs::Path { mnt, dentry: d };
    let file = vfs::node::file_of(&path.dentry);
    Ok((OpenFile::with_path(file, vfs::abi::O_RDONLY, path), b))
}

fn map(a: &AddressSpace, f: &OpenFile, pages: u64, prot: u64) -> Result<u64, String> {
    let flags = mm::linux::MAP_PRIVATE;
    a.sys_mmap_file(0, pages * P, prot, flags, 0, &mut |len| vfs::mmap::check(f, prot, flags, 0, len)).map_err(e)
}

/// File pages [first, first + n) as the mapping at `m` shows them, every byte checked (16 pages a copy).
fn mapped_same(a: &AddressSpace, m: u64, first: u64, n: u64) -> TestResult {
    let mut buf = Vec::new();
    buf.try_reserve_exact((16 * P) as usize).map_err(e)?;
    buf.resize((16 * P) as usize, 0u8);
    let mut pg = first;
    while pg < first + n {
        let k = (first + n - pg).min(16);
        let b = &mut buf[..(k * P) as usize];
        a.copy_from_user(b, m + pg * P).map_err(|x| format!("copy of page {pg}: {x:?}"))?;
        same(b, pg, "mapping")?;
        pg += k;
    }
    Ok(())
}

fn present(a: &AddressSpace, va: u64) -> bool {
    hal::paging::translate(a.root(), va).is_some()
}

fn file_seq() -> TestResult {
    workers()?;
    const N: u64 = 1024;
    let (f, b) = open_file(N)?;
    let a = AddressSpace::with_fill(Fill::OnFault).map_err(e)?;
    let m = map(&a, &f, N, 1)?;
    let around0 = counter("mm.as.file_around");
    let k = Ctr::now(RA_CTRS);
    let mut faults = 0u64;
    for pg in 0..N {
        if present(&a, m + pg * P) {
            continue;
        }
        a.handle_user_fault(m + pg * P, PF_U).map_err(|x| format!("fault page {pg}: {x:?}"))?;
        faults += 1;
        // The async window of a mark is in flight: wait for it, so the next faults find it (bounded).
        let calls = b.calls.lock().len();
        if calls > 0 {
            until(2000, || readahead::pending() == 0);
        }
    }
    let calls = b.take();
    let read: u64 = calls.iter().map(|c| c.1).sum();
    kassert!(read >= N && read <= N + 64, "{read} pages read for {N}: {calls:?}");
    kassert!(faults * 4 <= N, "{faults} faults for {N} pages touched in order (fault-around maps 16 at a time)");
    let async_windows = calls.iter().filter(|c| c.1 == readahead::DEFAULT_PAGES).count();
    kassert!(async_windows >= 8, "windows {calls:?}");
    kassert!(counter("mm.as.file_around") - around0 >= N / 2, "fault-around mapped {} pages", counter("mm.as.file_around") - around0);
    // Most windows after the first come from `follow` (the mark's fault finds the last job still running and the
    // job queues the next one when it ends), so `mmap_async` counts only the few the fault queued itself.
    k.grew(&[("vfs.readahead.mmap_sync", 1), ("vfs.readahead.mmap_async", 1), ("vfs.readahead.windows", 8), ("vfs.readahead.async", 8)])?;
    // Every page of the mapping, fault-around's too, whole.
    mapped_same(&a, m, 0, N)?;
    let (k, w) = (a.mem_stats(), a.mem_stats_walked());
    kassert!(k == w, "kept {k:?}, walked {w:?}");
    Ok(())
}
ktest!("mm.as.filefault.seq", Safe, file_seq);

fn file_random() -> TestResult {
    let (f, b) = open_file(512)?;
    let a = AddressSpace::with_fill(Fill::OnFault).map_err(e)?;
    let m = map(&a, &f, 512, 1)?;
    a.sys_madvise(m, 512 * P, mm::linux::madv::RANDOM).map_err(e)?;
    let k = Ctr::now(RA_CTRS);
    let pages = [300u64, 7, 100, 101, 102, 450];
    for &pg in &pages {
        a.handle_user_fault(m + pg * P, PF_U).map_err(|x| format!("fault page {pg}: {x:?}"))?;
    }
    sched::sleep_ms(20);
    let calls = b.take();
    kassert!(calls == pages.iter().map(|&p| (p, 1)).collect::<Vec<_>>(), "MADV_RANDOM reads each page alone: {calls:?}");
    kassert!(readahead::pending() == 0, "a job was queued");
    k.grew(&[("vfs.readahead.random", pages.len() as u64)])?;
    for &pg in &pages {
        mapped_same(&a, m, pg, 1)?;
    }
    // MADV_NORMAL again: the next miss reads around.
    a.sys_madvise(m, 512 * P, mm::linux::madv::NORMAL).map_err(e)?;
    a.handle_user_fault(m + 200 * P, PF_U).map_err(e)?;
    let calls = b.take();
    kassert!(calls == [(184, 32)], "read-around after MADV_NORMAL: {calls:?}");
    k.grew(&[("vfs.readahead.mmap_sync", 1)])?;
    mapped_same(&a, m, 184, 32)?;
    Ok(())
}
ktest!("mm.as.filefault.random", Safe, file_random);

fn file_hints() -> TestResult {
    workers()?;
    // MADV_SEQUENTIAL: a first touch with nothing before it reads ra_pages forward (readahead.c:640), no
    // read-around behind the page.
    let (f, b) = open_file(512)?;
    let a = AddressSpace::with_fill(Fill::OnFault).map_err(e)?;
    let m = map(&a, &f, 512, 1)?;
    a.sys_madvise(m, 512 * P, mm::linux::madv::SEQUENTIAL).map_err(e)?;
    a.handle_user_fault(m + 100 * P, PF_U).map_err(|x| format!("seq fault: {x:?}"))?;
    let calls = b.take();
    kassert!(calls == [(100, 32)], "MADV_SEQUENTIAL first touch: {calls:?}");
    mapped_same(&a, m, 100, 32)?;
    // MADV_WILLNEED: the range is read ahead (a job), the call does not wait (madvise.c:290).
    kassert!(until(5000, || {
        let _ = a.sys_madvise(m + 300 * P, 16 * P, mm::linux::madv::WILLNEED);
        b.calls.lock().iter().any(|c| c.0 == 300)
    }), "MADV_WILLNEED read nothing");
    kassert!(until(5000, || readahead::pending() == 0), "jobs left");
    let calls = b.take();
    kassert!(calls == [(300, 16)], "MADV_WILLNEED: {calls:?}");

    // Read-around leaves the mark to its own fault (filemap.c next_uptodate_folio), which queues the next window.
    let (f, b) = open_file(512)?;
    let a = AddressSpace::with_fill(Fill::OnFault).map_err(e)?;
    let m = map(&a, &f, 512, 3)?;
    a.handle_user_fault(m + 100 * P, PF_U).map_err(|x| format!("read fault: {x:?}"))?;
    let calls = b.take();
    kassert!(calls == [(84, 32)], "read-around: {calls:?}");
    for pg in 96..112u64 {
        kassert!(present(&a, m + pg * P) == (pg != 108), "fault-around at page {pg}: present {}", present(&a, m + pg * P));
    }
    kassert!(!present(&a, m + 112 * P) && !present(&a, m + 95 * P), "fault-around left its 64 KiB block");
    a.handle_user_fault(m + 108 * P, PF_U).map_err(|x| format!("mark fault: {x:?}"))?;
    kassert!(until(5000, || b.calls.lock().iter().any(|c| c.0 == 116)), "the mark's fault queued no window");
    kassert!(until(5000, || readahead::pending() == 0), "jobs left");
    kassert!(until(5000, || f.inode().and_then(|i| i.page_cache()).is_some_and(|c| c.cached(147).is_some())), "window [116, 148) not cached");
    b.take();
    // A write fault (a private copy) maps nothing around, though the block is cached.
    a.handle_user_fault(m + 120 * P, PF_U | PF_W).map_err(|x| format!("write fault: {x:?}"))?;
    for pg in 112..128u64 {
        kassert!(present(&a, m + pg * P) == (pg == 120), "write fault, page {pg}: present {}", present(&a, m + pg * P));
    }
    // (Not page 116: the new window's mark, whose fill would queue the next window.)
    mapped_same(&a, m, 84, 32)?;
    mapped_same(&a, m, 120, 1)?;
    kassert!(b.take().is_empty(), "the cached pages were read again");

    // Code (PROT_EXEC): ra_pages forward from the fault, no read-around behind it, no mark.
    let (f, b) = open_file(512)?;
    let a = AddressSpace::with_fill(Fill::OnFault).map_err(e)?;
    let m = map(&a, &f, 512, 5)?;
    a.handle_user_fault(m + 100 * P, PF_U).map_err(|x| format!("code fault: {x:?}"))?;
    let calls = b.take();
    kassert!(calls == [(100, 32)], "PROT_EXEC: {calls:?}");
    mapped_same(&a, m, 100, 32)?;

    // A fault past the end of the file: refused (SIGBUS), nothing read past it.
    let (f, b) = open_file(4)?;
    let a = AddressSpace::with_fill(Fill::OnFault).map_err(e)?;
    let m = map(&a, &f, 8, 1)?;
    kassert!(a.handle_user_fault(m + 6 * P, PF_U).is_err(), "a fault past the end of the file was served");
    a.handle_user_fault(m, PF_U).map_err(|x| format!("fault page 0: {x:?}"))?;
    mapped_same(&a, m, 0, 4)?;
    let calls = b.take();
    kassert!(calls.iter().all(|&(f, n)| f + n <= 4), "a read past the end: {calls:?}");
    Ok(())
}
ktest!("mm.as.filefault.hints", Safe, file_hints);
