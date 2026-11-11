//! FS-12, BL-4: read-ahead of page caches whose backend asks for it (`Backend::read_ahead_pages`: ext4, the
//! whole-disk `block::bdev::DiskFile`), as linux-7/mm/readahead.c (7.3.1) `page_cache_sync_ra` and
//! `page_cache_async_ra`, and mm/filemap.c `do_sync_mmap_readahead` / `do_async_mmap_readahead` for mappings.
//!
//! - A reader's miss reads its window **in the reader** (`PageCache::fill_with`, at the reader's own priority):
//!   the initial window (`init_size`) for a sequential miss, the cached history for a miss after cached pages, the
//!   request alone for a standalone random read (`Window::sync_seq`, `Window::sync_history`).
//! - The window's mark (Linux's PG_readahead page, `Window::mark_in`) starts the next window when a reader
//!   reaches it (`Window::next`: `get_next_ra_size` up to the device's maximum, the end aligned as Linux does for
//!   a mapping without large folios). That window is a `Job` on the reader's CPU's lane.
//!
//! Why the async window leaves the caller: `Backend::read_pages` waits until the device is done (the backends
//! have no submit-without-wait), so reading it in the reader would make the reader wait for pages it has not
//! asked for, which is what async read-ahead exists to avoid. It goes to a per-CPU worker (`worker`, one per
//! CPU, class Background; vfs-sys starts them), never one system thread, so readers on different CPUs never
//! queue behind one another's windows. The worker runs the job at the queuing reader's I/O priority (`IoPrio`,
//! Linux: the reader submits the async window itself), never higher. The pages of a running job are noted as
//! being read (`Inner::reading`): a reader that gets there waits for them as for another reader's fill (a
//! Background reader's job only `BG_FILL_WAIT_NS`, FS-13's rule, B4), so nothing is read twice.
//!
//! One job per file at a time (the queues hold at most one job per file being read: no fixed limit, R1); a
//! reader that reaches the mark while its file's job still runs makes the job open the next window when it
//! ends (`Ra::follow`). A truncate or hole punch while a job runs makes it stop without inserting
//! (`Inner::epoch`). Without workers (host tests, before Service init) no job is queued; the reader's own
//! windows still follow these rules.

use crate::pagecache::{Backend, CachePage, FillDone, Inner, MAX_WINDOW};
use crate::pool::pages_for;
use alloc::collections::VecDeque;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use hal::percpu::{this_cpu, PerCpu};
use ksync::{Mutex, OnceRef, SpinLock, WaitQueue};

/// Linux VM_READAHEAD_PAGES: 128 KiB (include/linux/pagemap.h:1429).
pub const DEFAULT_PAGES: u64 = 32;

// BL-0: pages the jobs brought in, and their backend reads (Linux `page_cache_async_ra`); a reader's own miss read
// is `vfs.readahead.sync` (pagecache.rs). Per CPU, as every counter of the read path.
crate::pcpu_counter!(PAGES_READ, "vfs.readahead.pages", crate::pcpu::EVENTS_BATCH);
crate::pcpu_counter!(ASYNC_READS, "vfs.readahead.async", crate::pcpu::EVENTS_BATCH);
// BL-4: async windows queued (a reader reached a mark).
crate::pcpu_counter!(pub(crate) WINDOWS, "vfs.readahead.windows", crate::pcpu::EVENTS_BATCH);
klog::counter!(JOBS_STOPPED, "vfs.readahead.stopped");

/// `get_init_ra_size` (readahead.c:390): the first window of a sequential stream for a request of `req` pages:
/// rounded up to a power of two, x4 when small, x2 when medium, else `max` (for max 32: 1-2 pages give 4, 3-4
/// give 8, 5-8 give 16, more give 32).
pub fn init_size(req: u64, max: u64) -> u64 {
    let n = req.max(1).checked_next_power_of_two().unwrap_or(u64::MAX);
    if n <= max / 32 {
        n * 4
    } else if n <= max / 4 {
        n * 2
    } else {
        max
    }
}

/// `get_next_ra_size` (readahead.c:408): the window after one of `cur` pages: x4 while small, x2 up to half
/// of `max`, then `max`.
pub fn next_size(cur: u64, max: u64) -> u64 {
    if cur < max / 16 {
        4 * cur
    } else if cur <= max / 2 {
        2 * cur
    } else {
        max
    }
}

/// `ractl_max_pages` (readahead.c:562): the window's limit for a request of `req` pages: `ra_pages`, or up to
/// the device's largest request (`io_pages`) for a request above it. At most `MAX_WINDOW` (one fill's reach).
pub fn max_pages(ra_pages: u64, io_pages: u64, req: u64) -> u64 {
    let m = if req > ra_pages && io_pages > ra_pages { req.min(io_pages) } else { ra_pages };
    m.min(MAX_WINDOW)
}

/// Linux `file_ra_state`'s window: `start`, `size`, `async_size` (pages).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Window {
    pub start: u64,
    pub size: u64,
    pub async_size: u64,
}

impl Window {
    /// `page_cache_sync_ra` for a start of file, an oversized read or a sequential miss (readahead.c:615-620).
    pub fn sync_seq(index: u64, req: u64, max: u64) -> Window {
        let size = init_size(req, max);
        let async_size = if size > req { size - req } else { size >> 1 };
        Window { start: index, size, async_size }
    }

    /// `page_cache_sync_ra` after `contig` cached pages before `index` (`from_start`: all of them back to page
    /// 0, readahead.c:625-650). None: a standalone small random read, read as is (the state stays).
    pub fn sync_history(index: u64, req: u64, contig: u64, from_start: bool, max: u64) -> Option<Window> {
        if contig <= req {
            return None;
        }
        let contig = if from_start { contig.saturating_mul(2) } else { contig };
        Some(Window { start: index, size: contig.saturating_add(req).min(max), async_size: 1 })
    }

    /// `page_cache_async_ra` at the expected mark (readahead.c:690-697, :710-717): the next window after this one,
    /// ramped up; its end is aligned down to `1 << min(order, ffs(max) - 1)` pages where `order` is 2 (BitsOS's
    /// caches hold 4 KiB pages only: Linux's `page_cache_ra_order` sets `ra->order` back to 0 for a mapping
    /// without large folios, and the async path adds 2). All of it is the async part, so its mark is its start.
    pub fn next(&self, max: u64) -> Window {
        let start = self.start.saturating_add(self.size);
        let mut size = self.size.max(next_size(self.size, max));
        let align = 1u64 << 2u32.min(max.max(1).trailing_zeros());
        let end = start.saturating_add(size);
        let aligned = end & !(align - 1);
        if aligned > start {
            size -= end - aligned;
        }
        Window { start, size, async_size: size }
    }

    /// The window grown to at least `pages` (FS-14b: a missed page brings in the backend's window), with its mark
    /// where it was.
    pub fn at_least(self, pages: u64) -> Window {
        if pages <= self.size {
            return self;
        }
        Window { start: self.start, size: pages, async_size: self.async_size + (pages - self.size) }
    }

    /// The PG_readahead page for a file of `pages` pages: none without an async part or when the window reaches
    /// past the end (`do_page_cache_ra`: "We've reached the end, so don't set a readahead marker").
    pub fn mark_in(&self, pages: u64) -> Option<u64> {
        if self.async_size == 0 || self.async_size > self.size || self.start.saturating_add(self.size) > pages {
            return None;
        }
        Some(self.start + self.size - self.async_size)
    }
}

/// The I/O priority a window goes at (block's three classes; vfs knows no block layer).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Prio {
    Interactive,
    Normal,
    Background,
}

/// The block layer's priority of the calling thread, and a way to run a job at a reader's priority (vfs-sys
/// installs `block::prio`; without it jobs run at the worker's own, Background).
pub trait IoPrio: Sync {
    fn current(&self) -> Prio;
    /// Runs `f` with this thread's requests at least at `p`.
    fn run_at(&self, p: Prio, f: &mut dyn FnMut());
}

static IO_PRIO: OnceRef<dyn IoPrio> = OnceRef::new();

/// vfs-sys installs the block layer's priorities once at boot (false: installed already).
pub fn set_io_prio(p: &'static dyn IoPrio) -> bool {
    IO_PRIO.set(p)
}

pub(crate) struct Job {
    pub(crate) inner: Weak<Mutex<Inner>>,
    pub(crate) backend: Arc<dyn Backend>,
    pub(crate) done: Arc<FillDone>,
    pub(crate) first: u64,
    pub(crate) count: u64,
    /// `Inner::epoch` when queued.
    pub(crate) epoch: u64,
    /// The window's limit (`max_pages`), for the next window a `follow` opens.
    pub(crate) max: u64,
    /// The queuing reader's I/O priority (None: no block layer, host tests).
    pub(crate) prio: Option<Prio>,
}

impl Job {
    /// A job of pages [first, first + count) at the calling reader's priority.
    pub(crate) fn new(inner: Weak<Mutex<Inner>>, backend: Arc<dyn Backend>, done: Arc<FillDone>, (first, count): (u64, u64), epoch: u64, max: u64) -> Job {
        Job { inner, backend, done, first, count, epoch, max, prio: IO_PRIO.get().map(|p| p.current()) }
    }
}

/// One CPU's queue and its worker.
struct Lane {
    jobs: SpinLock<VecDeque<Job>>,
    wq: WaitQueue,
    /// A worker takes this lane's jobs.
    live: AtomicBool,
}

fn new_lane(_cpu: usize) -> Lane {
    Lane { jobs: SpinLock::named("vfs.readahead.lane", VecDeque::new()), wq: WaitQueue::new(), live: AtomicBool::new(false) }
}

static LANES: PerCpu<Lane> = PerCpu::new(new_lane);
/// Lanes with a worker (or the host tests' switch).
static LIVE: AtomicUsize = AtomicUsize::new(0);

/// Whether a worker takes jobs (else no async window is queued).
pub fn active() -> bool {
    LIVE.load(Ordering::Acquire) > 0
}

fn mark_live(l: &Lane, on: bool) {
    if l.live.swap(on, Ordering::AcqRel) != on {
        if on {
            LIVE.fetch_add(1, Ordering::AcqRel);
        } else {
            LIVE.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

/// Turns this CPU's lane on or off without a worker (host tests drive it with `run_pending`).
pub fn set_active(on: bool) {
    mark_live(LANES.get(), on);
}

/// The lane for a job queued now: this CPU's, else the first one with a worker. Never grows the lanes (no
/// allocation on the read path: the workers made them).
fn lane_for_queue() -> Option<&'static Lane> {
    let live = |l: &&Lane| l.live.load(Ordering::Acquire);
    LANES.find_for(this_cpu()).filter(live).or_else(|| LANES.iter_existing().find(live))
}

/// Queues a job; false when no lane takes it or the heap refuses (the caller clears its file's `queued` mark).
pub(crate) fn queue(job: Job) -> bool {
    let Some(lane) = lane_for_queue() else {
        drop(job);
        return false;
    };
    let mut q = lane.jobs.lock();
    if q.try_reserve(1).is_err() {
        drop(q);
        drop(job); // its backend may be the last reference: never dropped under the spin lock
        return false;
    }
    q.push_back(job);
    drop(q);
    WINDOWS.inc();
    lane.wq.wake_one();
    true
}

/// Jobs waiting now (all lanes).
pub fn pending() -> usize {
    LANES.iter_existing().map(|l| l.jobs.lock().len()).sum()
}

/// Runs the queued jobs of every lane in the calling thread; returns how many ran.
pub fn run_pending() -> usize {
    let mut n = 0;
    loop {
        let mut ran = false;
        for l in LANES.iter_existing() {
            // The queue's lock is a spin lock: taken only to pop, never while the job reads.
            let next = l.jobs.lock().pop_front();
            if let Some(job) = next {
                run(job);
                n += 1;
                ran = true;
            }
        }
        if !ran {
            return n;
        }
    }
}

/// The worker of CPU `cpu`'s lane: waits for jobs (a timeout so a lost wake-up costs one second at most) and
/// runs them.
pub fn worker(cpu: usize) -> ! {
    let lane = LANES.get_for(cpu).unwrap_or_else(|| LANES.get());
    mark_live(lane, true);
    klog::event!(Debug, "vfs", "readahead.worker", cpu = cpu, max_pages = DEFAULT_PAGES);
    loop {
        lane.wq.wait_until_timeout(|| !lane.jobs.lock().is_empty(), 1_000_000_000);
        loop {
            let next = lane.jobs.lock().pop_front();
            let Some(job) = next else { break };
            run(job);
        }
    }
}

/// A job's pages being read, noted in `Inner::reading` (readers that miss them wait); taken out on every path.
struct Note<'a> {
    inner: &'a Mutex<Inner>,
    done: &'a FillDone,
    range: Option<(u64, u64, bool)>,
}

impl Note<'_> {
    fn end_locked(&mut self, g: &mut Inner) {
        if let Some(r) = self.range.take() {
            g.end_reading(r);
            self.done.ended();
        }
    }
}

impl Drop for Note<'_> {
    fn drop(&mut self) {
        if self.range.is_some() {
            let mut g = self.inner.lock();
            self.end_locked(&mut g);
            drop(g);
            self.done.wake();
        }
    }
}

fn run(job: Job) {
    let mut next = None;
    match (IO_PRIO.get(), job.prio) {
        (Some(h), Some(p)) => h.run_at(p, &mut || next = run_job(&job)),
        _ => next = run_job(&job),
    }
    drop(job);
    if let Some(n) = next {
        let inner = n.inner.clone();
        if !queue(n)
            && let Some(i) = inner.upgrade()
        {
            i.lock().ra.queued = false;
        }
    }
}

/// Reads the job's window; returns the next job when a reader reached the mark meanwhile (`Ra::follow`).
fn run_job(job: &Job) -> Option<Job> {
    let mut read = 0u64;
    let mut stop: Option<&'static str> = None;
    let last = job.first.saturating_add(job.count);
    let mut idx = job.first;
    let bg = job.prio == Some(Prio::Background);
    // FS-14b: each run of missing pages is one backend call (`Backend::read_pages`: one request for a disk),
    // not one per page; pages a fill is reading now are left to it.
    while idx < last {
        let Some(inner) = job.inner.upgrade() else {
            stop = Some("file gone");
            break;
        };
        let mut note = Note { inner: &inner, done: &job.done, range: None };
        let end = {
            let mut g = inner.lock();
            if g.epoch != job.epoch {
                stop = Some("truncated");
                break;
            }
            let n = pages_for(g.size()).min(last);
            if idx >= n {
                break;
            }
            if !g.free(idx) {
                idx += 1;
                continue;
            }
            let mut end = idx + 1;
            while end < n && end - idx < MAX_WINDOW && g.free(end) {
                end += 1;
            }
            // Noted only if the list can grow; without the note a reader may read the same pages.
            if g.note_reading((idx, end, bg)) {
                note.range = Some((idx, end, bg));
            }
            end
        };
        let mut pages = Vec::new();
        if pages.try_reserve_exact((end - idx) as usize).is_err() {
            stop = Some("no memory");
            break;
        }
        for _ in idx..end {
            match CachePage::new(None) {
                Some(p) => pages.push(p),
                None => break,
            }
        }
        if pages.is_empty() {
            stop = Some("no frame");
            break;
        }
        ASYNC_READS.inc();
        if let Err(e) = job.backend.read_pages(idx, &pages) {
            klog::event!(Debug, "vfs", "readahead.read_failed", page = idx, errno = e);
            stop = Some("read failed");
            break;
        }
        let mut g = inner.lock();
        if g.epoch != job.epoch {
            stop = Some("truncated");
            break;
        }
        let n = pages_for(g.size());
        let got = pages.len() as u64;
        for (k, p) in pages.into_iter().enumerate() {
            let i = idx + k as u64;
            if i < n && !g.pages.contains(i) {
                if g.pages.insert(i, p).is_err() {
                    stop = Some("no memory");
                    break;
                }
                read += 1;
            }
        }
        note.end_locked(&mut g);
        drop(g);
        job.done.wake();
        if stop.is_some() {
            break;
        }
        idx += got;
    }
    let next = job.inner.upgrade().and_then(|inner| {
        let mut g = inner.lock();
        g.ra.queued = false;
        if !core::mem::take(&mut g.ra.follow) || stop.is_some() {
            return None;
        }
        let pages = pages_for(g.size());
        let w = g.open_next(pages, job.max)?;
        Some(Job { inner: job.inner.clone(), backend: job.backend.clone(), done: job.done.clone(), first: w.0, count: w.1, epoch: g.epoch, max: job.max, prio: job.prio })
    });
    PAGES_READ.add(read);
    if let Some(why) = stop {
        JOBS_STOPPED.add(1);
        klog::event!(Debug, "vfs", "readahead.stopped", first = job.first, count = job.count, read = read, why = why);
    }
    next
}
