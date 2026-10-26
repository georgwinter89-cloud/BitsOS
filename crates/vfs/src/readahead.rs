//! FS-12: asynchronous read-ahead of page caches whose backend asks for it (`Backend::read_ahead_pages`; the
//! whole-disk `block::bdev::DiskFile`). A reader that misses a page reads it itself, at its own priority; the
//! pages after it are queued here and read by one Background thread (vfs-sys `vfs.readahead`), so their disk
//! requests go as Background (`block::prio::current`) and never sit before an Interactive read.
//!
//! Windows as linux-6.6/mm/readahead.c in short: a sequential miss (at the file start, right after the last
//! read, or inside one read) queues a window after it, starting at 4 pages and doubling up to the backend's
//! maximum; reaching the first page of the last window (`mark`, Linux's PG_readahead) queues the next one (if
//! the job of the last window is still under way, that job queues the next one when it ends: `Ra::follow`); a
//! random miss resets the window. One job per file at a time, so the queue holds at most one job per file
//! being read (no fixed limit, R1). A reader that gets to a page before the job does reads it itself (both
//! reads may run, the first to insert wins, as in `fill`); a truncate or hole punch while a job runs makes it
//! stop without inserting (`Inner::epoch`). Without the thread (host tests, before Service init) nothing is
//! queued.

use crate::pagecache::{Backend, CachePage, Inner, MAX_WINDOW};
use crate::pool::pages_for;
use alloc::collections::VecDeque;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};
use ksync::{Mutex, SpinLock, WaitQueue};

/// Linux VM_READAHEAD_PAGES: 128 KiB.
pub const DEFAULT_PAGES: u64 = 32;
/// The first window (Linux `get_init_ra_size` for small reads).
pub(crate) const INIT_PAGES: u64 = 4;

klog::counter!(PAGES_READ, "vfs.readahead.pages");
klog::counter!(JOBS_STOPPED, "vfs.readahead.stopped");

pub(crate) struct Job {
    pub(crate) inner: Weak<Mutex<Inner>>,
    pub(crate) backend: Arc<dyn Backend>,
    pub(crate) first: u64,
    pub(crate) count: u64,
    /// `Inner::epoch` when queued.
    pub(crate) epoch: u64,
}

static QUEUE: SpinLock<VecDeque<Job>> = SpinLock::named("vfs.readahead", VecDeque::new());
static WQ: WaitQueue = WaitQueue::new();
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// Whether a reader thread takes jobs (else read-ahead is off).
pub fn active() -> bool {
    ACTIVE.load(Ordering::Acquire)
}

/// Turns queuing on or off (the reader thread at start; host tests drive it with `run_pending`).
pub fn set_active(on: bool) {
    ACTIVE.store(on, Ordering::Release);
}

/// Queues a job; false when the heap refuses (the caller clears its file's `queued` mark).
pub(crate) fn queue(job: Job) -> bool {
    let mut q = QUEUE.lock();
    if q.try_reserve(1).is_err() {
        drop(q);
        drop(job); // its backend may be the last reference: never dropped under the spin lock
        return false;
    }
    q.push_back(job);
    drop(q);
    WQ.wake_one();
    true
}

/// Jobs waiting now.
pub fn pending() -> usize {
    QUEUE.lock().len()
}

/// Runs the queued jobs in the calling thread; returns how many ran.
pub fn run_pending() -> usize {
    let mut n = 0;
    loop {
        // The queue's lock is a spin lock: taken only to pop, never while the job reads.
        let next = QUEUE.lock().pop_front();
        let Some(job) = next else { break };
        run(job);
        n += 1;
    }
    n
}

/// The reader thread's body: waits for jobs (a timeout so a lost wake-up costs one second at most) and runs them.
pub fn reader_loop() -> ! {
    set_active(true);
    klog::event!(Info, "vfs", "readahead.started", max_pages = DEFAULT_PAGES);
    loop {
        WQ.wait_until_timeout(|| !QUEUE.lock().is_empty(), 1_000_000_000);
        run_pending();
    }
}

fn run(job: Job) {
    let mut read = 0u64;
    let mut stop: Option<&'static str> = None;
    let last = job.first.saturating_add(job.count);
    let mut idx = job.first;
    // FS-14b: each run of missing pages is one backend call (`Backend::read_pages`: one request for a disk),
    // not one per page; pages a fill is reading now are left to it.
    while idx < last {
        let Some(inner) = job.inner.upgrade() else {
            stop = Some("file gone");
            break;
        };
        let end = {
            let g = inner.lock();
            if g.epoch != job.epoch {
                stop = Some("truncated");
                break;
            }
            let n = pages_for(g.size).min(last);
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
        let n = pages_for(g.size);
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
        let (start, pages, max) = (g.ra.end, pages_for(g.size), job.backend.read_ahead_pages());
        let (first, count) = g.ra.open(start, pages, max)?;
        Some(Job { inner: job.inner.clone(), backend: job.backend.clone(), first, count, epoch: g.epoch })
    });
    PAGES_READ.add(read);
    if let Some(why) = stop {
        JOBS_STOPPED.add(1);
        klog::event!(Debug, "vfs", "readahead.stopped", first = job.first, count = job.count, read = read, why = why);
    }
    if let Some(n) = next {
        let inner = n.inner.clone();
        if !queue(n)
            && let Some(i) = inner.upgrade()
        {
            i.lock().ra.queued = false;
        }
    }
}
