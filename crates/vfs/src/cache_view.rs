//! PF-4/PF-5: a file's page index and size as readers see them, apart from `vfs.inode.data`.
//!
//! Every change still happens with `vfs.inode.data` held (the Mutex keeps writers, truncate, fills, reclaim and
//! read-ahead in one order), and under the write side of the index's own lock `vfs.inode.index` too.
//!
//! PF-5 (Linux 4c0f369's idea, plan/notes/pf-scout-memory.md section 4): the index is an `RcuIndex`, so a read
//! finds and copies cached pages with no lock and no reference at all (`Shared::quick`): inside a
//! `grace::Section` it loads the size and the pages and copies them through a sink that never sleeps or
//! faults (a kernel buffer, or a user copy that turns a fault into "stop here"); a page that leaves the index
//! meanwhile is dropped only after a grace period (`Gone`), so its bytes stay valid until the section ends.
//! A sequence count (`seq`, odd while a writer takes pages out or shrinks the size) tells a step that raced a
//! truncate or a punch: it is copied again on the locked path. That path (`Shared::snap`, PF-4's) takes
//! references under the read side and serves everything the quick one cannot: missing pages, user copies that
//! fault, a step that raced.
//!
//! A read sees the size and the pages of one moment: growth inserts the pages before it sets the size, a
//! shrinking truncate sets the size before it takes the pages out (`truncate_setsize` before
//! `truncate_pagecache`), both inside the count.
//!
//! The reader rule as a type: a `Section` is entered only by `Shared::quick` on a borrowed `Shared` (the open
//! file's page cache) and every page it finds borrows that section, so no page reference outlives it (Linux's
//! "a lockless reader holds the file open").
//!
//! Lock order: `vfs.inode.data` > `vfs.inode.index` > `mm.grace` > heap > frames. Nothing else is taken under
//! the index lock; a page that leaves under it goes to the grace queue (`mm.grace`), never waits there and never
//! logs; it is never held across a user copy, a backend call, a wait or a klog event.

use crate::frame::PAGE;
use crate::index::NoRoom;
use crate::pagecache::CachePage;
use alloc::sync::Arc;
use core::ops::Deref;
use core::sync::atomic::{fence, AtomicU64, Ordering};
use mm_core::grace::{self, Retire};
use mm_core::rcu_index::{Entry, RcuIndex};

/// `Shared::ra_mark` when the file has no read-ahead mark.
pub(crate) const NO_MARK: u64 = u64::MAX;

/// Pages a read takes from the index in one look-up (the copies run after it; a longer read looks again), and
/// pages one lockless section copies at most (the length of a read section). Only the length of one step,
/// not a limit of anything (R1).
pub(crate) const BATCH: usize = 16;

/// A page as the index shows it under its lock (its reference count as the index sees it).
pub(crate) type PageRef<'a> = Entry<'a, CachePage>;

pub(crate) struct Shared {
    index: RcuIndex<CachePage>,
    /// The size readers see (the data lock's `Inner::size` is set with it).
    size: AtomicU64,
    /// PF-5: odd while a writer takes pages out or shrinks the size (a seqcount; growth needs none: pages go in
    /// before the size grows).
    seq: AtomicU64,
    /// FS-12: the page after the last one a read touched (read-ahead's "sequential" test), kept outside the data
    /// lock so a read of cached pages does not take it. A race between two readers only weakens the guess.
    pub(crate) ra_next: AtomicU64,
    /// FS-12: `Ra::mark` as a reader without the data lock sees it (`NO_MARK`: none): a read that reaches it
    /// takes the data lock for the read-ahead bookkeeping (`ra_note`), every other cached read does not.
    pub(crate) ra_mark: AtomicU64,
}

/// One look-up of `read_direct`'s locked path: the size then, and the pages from `first` on (None: not cached).
pub(crate) struct Snap {
    pub(crate) size: u64,
    pub(crate) pages: [Option<ReadHold>; BATCH],
    /// Slots filled (pages from `first`; a missing page of a file with a backend ends the run and is not one).
    pub(crate) len: usize,
    /// The first page not cached that a backend must read (None: none in this step).
    pub(crate) missing: Option<u64>,
}

/// A locked read's reference to a page for its copy, counted in `CachePage::reads` (taken under the read side,
/// so a punch under the write side sees every one). Its count goes before its reference (`Drop` runs before the
/// field's), so a punch that sees the reference gone sees the count gone too: a stale count never hides a
/// mapping's pin (`Pages::take_unpinned`). Lockless reads hold neither: the grace period keeps their pages.
pub(crate) struct ReadHold(Arc<CachePage>);

impl ReadHold {
    fn new(p: &PageRef<'_>) -> ReadHold {
        p.reads.fetch_add(1, Ordering::Relaxed);
        ReadHold(p.arc())
    }
}

impl Deref for ReadHold {
    type Target = CachePage;
    fn deref(&self) -> &CachePage {
        &self.0
    }
}

impl Drop for ReadHold {
    fn drop(&mut self) {
        self.0.reads.fetch_sub(1, Ordering::Release);
        // PF-5: a page that left the index while this read held it leaves the cache's numbers with the last
        // holder (`CachePage::release` does the check; the reference itself drops with the field).
        if self.0.retired.load(Ordering::Acquire) && Arc::strong_count(&self.0) == 2 {
            self.0.account_out();
        }
    }
}

/// A page that left the index. Lockless readers may still copy from it, so its reference is dropped only
/// after a grace period: `Drop` hands it to the grace queue. A page nobody else holds leaves the cache's
/// accounting at once (`CachePage::account_out`: frames held, tmpfs, its pool charge) and waits as pending
/// memory (`grace::pending_bytes`), which MemAvailable, kswapd and the allocator count as reusable.
pub(crate) struct Gone(Option<Retire<CachePage>>);

impl Gone {
    /// References now (this one included).
    pub(crate) fn strong_count(&self) -> usize {
        self.0.as_ref().map_or(0, |r| r.strong_count())
    }
}

impl Deref for Gone {
    type Target = CachePage;
    fn deref(&self) -> &CachePage {
        // Some until `drop`.
        self.0.as_deref().unwrap_or_else(|| unreachable!("Gone is emptied only by its drop"))
    }
}

impl Gone {
    /// The page's numbers leave now when it is alone (no new reference can appear: it left the index), so its
    /// frame waits as pending and is freed by the deferred drop; else the last holder accounts it out.
    fn settle(r: &Retire<CachePage>) -> u64 {
        if r.strong_count() == 1 {
            r.account_out()
        } else {
            r.retired.store(true, Ordering::Release);
            0
        }
    }
    /// Drops it without paying the back-pressure (reclaim, under a cache's lock other reclaimers try).
    pub(crate) fn drop_nowait(mut self) {
        if let Some(r) = self.0.take() {
            let bytes = Gone::settle(&r);
            r.defer_nowait(bytes);
        }
    }
}

impl Drop for Gone {
    fn drop(&mut self) {
        if let Some(r) = self.0.take() {
            let bytes = Gone::settle(&r);
            r.defer(bytes);
        }
    }
}

/// What ended a lockless step (`Shared::quick`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stop {
    /// The step copied what it was asked for or reached its length; more may follow.
    More,
    /// The end of the file.
    Eof,
    /// A page not cached (a backend reads it: the locked path).
    Missing,
    /// The sink took fewer bytes than offered (a full buffer, a user fault): the caller goes on outside.
    Sink,
    /// A writer took pages out or shrank the file during the step, or no section could be entered: nothing of
    /// this step counts (the locked path copies it again).
    Retry,
}

/// A lockless read's consumer: (page or None for a hole, offset in the page, bytes, bytes of the read before
/// them) -> bytes it took. It runs inside a read section: it must not sleep, fault, take a lock or log.
pub type QuickSink<'a> = dyn FnMut(Option<&CachePage>, usize, usize, usize) -> usize + 'a;

/// The index and the size of one file; every method takes the index lock for itself.
pub(crate) struct Pages(Arc<Shared>);

impl Pages {
    pub(crate) fn new(size: u64) -> Pages {
        Pages(Arc::new(Shared {
            index: RcuIndex::named("vfs.inode.index"),
            size: AtomicU64::new(size),
            seq: AtomicU64::new(0),
            ra_next: AtomicU64::new(0),
            ra_mark: AtomicU64::new(NO_MARK),
        }))
    }
    /// The shared part, for a reader that holds no data lock.
    pub(crate) fn shared(&self) -> Arc<Shared> {
        self.0.clone()
    }
    pub(crate) fn ra(&self) -> &Shared {
        &self.0
    }
    pub(crate) fn len(&self) -> usize {
        self.0.index.len()
    }
    pub(crate) fn contains(&self, i: u64) -> bool {
        self.0.index.read().contains(i)
    }
    pub(crate) fn get(&self, i: u64) -> Option<Arc<CachePage>> {
        self.0.get(i)
    }
    pub(crate) fn prepare(&self, i: u64) -> Result<(), NoRoom> {
        self.0.index.write().prepare(i).map_err(|_| NoRoom)
    }
    /// Inserts `v` at `i`; the page that was there, if any, leaves after the lock (through a grace period).
    pub(crate) fn insert(&self, i: u64, v: Arc<CachePage>) -> Result<(), NoRoom> {
        let old = self.0.index.write().insert(i, v).map_err(|_| NoRoom)?;
        drop(old.map(|r| Gone(Some(r))));
        Ok(())
    }
    pub(crate) fn remove(&self, i: u64) -> Option<Gone> {
        let mut w = self.0.index.write();
        self.0.changing(|| w.remove(i)).map(|r| Gone(Some(r)))
    }
    pub(crate) fn next_from(&self, from: u64) -> Option<(u64, Arc<CachePage>)> {
        self.0.index.read().next_from(from).map(|(i, p)| (i, p.arc()))
    }
    /// `next_from` that looks at the page in place, under the read side (`f` takes no lock).
    pub(crate) fn next_with<R>(&self, from: u64, f: impl FnOnce(u64, &PageRef<'_>) -> R) -> Option<(u64, R)> {
        self.0.index.read().next_from(from).map(|(i, p)| (i, f(i, &p)))
    }
    /// Takes page `i` out only while the index holds its only reference, checked under the write side (a
    /// locked reader's new reference is taken under the read side, so none can appear in between): the page
    /// then, for the caller to drop (its frame goes back after a grace period); None when it is missing or held
    /// (a mapping, a locked read). Lockless readers hold no reference: the grace period covers them.
    pub(crate) fn take_alone(&self, i: u64) -> Option<Gone> {
        let mut w = self.0.index.write();
        if !w.get(i).is_some_and(|p| p.strong_count() == 1) {
            return None;
        }
        self.0.changing(|| w.remove(i)).map(|r| Gone(Some(r)))
    }
    /// Takes page `i` out unless something besides the index and locked reads holds it (a mapping's pin, a
    /// probe, a write-back batch), checked under the write side: Some((page, read_held)) for the caller to drop
    /// (`read_held`: locked reads still copy from it, its frame goes back when the last one ends); None when it
    /// is missing or pinned. A count that changes meanwhile only ever counts a read as a pin (keep it in place).
    pub(crate) fn take_unpinned(&self, i: u64) -> Option<(Gone, bool)> {
        let mut w = self.0.index.write();
        let (refs, reads) = {
            let p = w.get(i)?;
            // The reference count first, then the reads (each read drops its count before its reference, with
            // Release; the fence pairs with it): no read seen in the count is missing from the references.
            let refs = p.strong_count();
            fence(Ordering::Acquire);
            (refs, p.reads.load(Ordering::Relaxed) as usize)
        };
        if refs > 1 + reads {
            return None;
        }
        let held = refs > 1;
        self.0.changing(|| w.remove(i)).map(|r| (Gone(Some(r)), held))
    }
    /// `f` runs under the read side: it must only look at the pages (no lock, no wait, no drop of a page).
    pub(crate) fn for_each(&self, f: &mut dyn FnMut(u64, &PageRef<'_>) -> bool) {
        self.0.index.read().for_each(f)
    }
    /// Takes every page from `from` on out; `f` runs under the write side (keep or count them, take no lock;
    /// a `Gone` dropped there only goes to the grace queue).
    pub(crate) fn drain_from(&self, from: u64, f: &mut dyn FnMut(u64, Gone)) {
        let mut w = self.0.index.write();
        self.0.changing(|| w.drain_from(from, &mut |i, r| f(i, Gone(Some(r)))))
    }
    /// The size readers see (the data lock's `Inner::size` is set with it, `Inner::set_size`).
    pub(crate) fn set_size(&self, size: u64) {
        let _w = self.0.index.write();
        if size < self.0.size.load(Ordering::Relaxed) {
            self.0.changing(|| self.0.size.store(size, Ordering::Release));
        } else {
            self.0.size.store(size, Ordering::Release);
        }
    }
}

impl Shared {
    /// A change a lockless step must not mix with (pages out, size down), under the write side: the count is odd
    /// while it runs (`Shared::quick` compares it before and after its step).
    fn changing<R>(&self, f: impl FnOnce() -> R) -> R {
        self.seq.fetch_add(1, Ordering::Relaxed);
        fence(Ordering::Release);
        let r = f();
        self.seq.fetch_add(1, Ordering::Release);
        r
    }

    /// PF-5: one lockless step of a read of up to `len` bytes at `pos`: at most `BATCH` pages, found and handed
    /// to `sink` inside one read section, no lock, no reference. `backend`: a missing page is read from it (the
    /// step ends, `Stop::Missing`); without one it is a hole (zeros). Returns the bytes the sink took and what
    /// ended the step; after `Stop::Retry` none of them count.
    pub(crate) fn quick(&self, pos: u64, len: usize, backend: bool, sink: &mut QuickSink<'_>) -> (usize, Stop) {
        let Some(sec) = grace::enter() else { return (0, Stop::Retry) };
        let s1 = self.seq.load(Ordering::Acquire);
        if s1 & 1 != 0 {
            return (0, Stop::Retry);
        }
        let size = self.size.load(Ordering::Acquire);
        if pos >= size {
            return (0, Stop::Eof);
        }
        let end = pos.saturating_add(len as u64).min(size);
        let (mut done, mut stop, mut pages) = (0usize, Stop::More, 0usize);
        while pos + (done as u64) < end && pages < BATCH {
            let here = pos + done as u64;
            let off = (here % PAGE as u64) as usize;
            let k = ((PAGE - off) as u64).min(end - here) as usize;
            let page = self.index.get(&sec, here / PAGE as u64);
            if page.is_none() && backend {
                stop = Stop::Missing;
                break;
            }
            if let Some(p) = page {
                p.touch();
            }
            let got = sink(page, off, k, done).min(k);
            done += got;
            pages += 1;
            if got < k {
                stop = Stop::Sink;
                break;
            }
        }
        fence(Ordering::Acquire);
        if self.seq.load(Ordering::Relaxed) != s1 {
            return (0, Stop::Retry);
        }
        drop(sec);
        if stop == Stop::More && pos + done as u64 >= size {
            stop = Stop::Eof;
        }
        (done, stop)
    }

    /// The size and up to `BATCH` pages from `first` on that a read of the bytes up to `end` (exclusive, clipped
    /// to the size) needs, in one look-up under the read side (the locked path). `backend`: a missing page is
    /// read from it (it ends the run); without one it is a hole (an empty slot, read as zeros).
    pub(crate) fn snap(&self, first: u64, end: u64, backend: bool) -> Snap {
        let mut s = Snap { size: 0, pages: [const { None }; BATCH], len: 0, missing: None };
        let v = self.index.read();
        s.size = self.size.load(Ordering::Acquire);
        let last = end.min(s.size).div_ceil(PAGE as u64);
        let mut i = first;
        while i < last && s.len < BATCH {
            match v.get(i) {
                Some(p) => s.pages[s.len] = Some(ReadHold::new(&p)),
                None if backend => {
                    s.missing = Some(i);
                    break;
                }
                None => {}
            }
            s.len += 1;
            i += 1;
        }
        drop(v);
        s
    }
    pub(crate) fn get(&self, i: u64) -> Option<Arc<CachePage>> {
        self.index.read().get(i).map(|p| p.arc())
    }
    pub(crate) fn mark(&self) -> u64 {
        self.ra_mark.load(Ordering::Acquire)
    }
}
