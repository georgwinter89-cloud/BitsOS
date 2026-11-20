//! The lists an unmap gathers frames and emptied page tables into before its shootdown round, as Linux's mmu_gather
//! keeps them (linux-7.3.1/mm/mmu_gather.c:20 `tlb_next_batch`): a batch on the stack first, then batches of one
//! page each taken without waiting (Linux `__get_free_page(GFP_NOWAIT)`; here a frame from the hook the frame
//! allocator installs, `set_batch_frames`, which never reclaims, compacts or sleeps). With no batch to be had the
//! list is full, and its owner flushes what it gathered and goes on (Linux `tlb_flush_mmu`), never the heap.
//! MEMCRASH (Mini-PC 13.11.2026): the frame list was a `Vec` that grew through the heap while `mm.as` was held; the
//! growth went into the heap's reclaim and stood there behind `mm.grace`.

use super::PageSize;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering::*};

/// A gathered entry: a frame's address with its size in the low bits, and the caller's tag.
type Entry = [u64; 2];

/// Entries on the stack (Linux's `local` batch is 8; 32 here, as before).
const LOCAL: usize = 32;
/// Entries of one batch page: the page less its header.
const PER_PAGE: usize = (4096 - 24) / core::mem::size_of::<Entry>();

/// One batch page, reached through the direct map (it is a frame, not a heap object).
#[repr(C)]
struct Batch {
    next: *mut Batch,
    n: usize,
    phys: u64,
    items: [Entry; PER_PAGE],
}

const _: () = assert!(core::mem::size_of::<Batch>() <= 4096);

static ALLOC: AtomicUsize = AtomicUsize::new(0);
static FREE: AtomicUsize = AtomicUsize::new(0);
/// Batches asked for that the hook could not give (the list flushed early instead).
static NO_BATCH: AtomicU64 = AtomicU64::new(0);

/// Installs where batch pages come from and go back to: `alloc` must never wait or reclaim (it is called with
/// `mm.as`, page-table and vmap locks held). Without it the lists keep to the stack batch.
pub fn set_batch_frames(alloc: fn() -> Option<u64>, free: fn(u64)) {
    FREE.store(free as usize, Release);
    ALLOC.store(alloc as usize, Release);
}

/// Batch pages refused so far (`TlbStats::gather_nobatch`).
pub(super) fn no_batch() -> u64 {
    NO_BATCH.load(Relaxed)
}

fn batch_frame() -> Option<u64> {
    let f = ALLOC.load(Acquire);
    if f == 0 {
        return None;
    }
    // SAFETY: only `set_batch_frames` stores here, a `fn() -> Option<u64>`.
    let f: fn() -> Option<u64> = unsafe { core::mem::transmute::<usize, fn() -> Option<u64>>(f) };
    f()
}

fn free_frame(p: u64) {
    let f = FREE.load(Acquire);
    if f != 0 {
        // SAFETY: only `set_batch_frames` stores here, a `fn(u64)`, before the matching alloc hook.
        let f: fn(u64) = unsafe { core::mem::transmute::<usize, fn(u64)>(f) };
        f(p);
    }
}

fn size_code(s: PageSize) -> u64 {
    match s {
        PageSize::Size4K => 0,
        PageSize::Size2M => 1,
        PageSize::Size1G => 2,
    }
}

fn size_of_code(c: u64) -> PageSize {
    match c {
        1 => PageSize::Size2M,
        2 => PageSize::Size1G,
        _ => PageSize::Size4K,
    }
}

/// A list of (frame, size, tag): the stack batch, then batch pages up to `max` entries.
pub(super) struct List {
    local: [Entry; LOCAL],
    n_local: usize,
    /// The first batch page (kept until the list drops: a list that is flushed and filled again asks for none).
    head: *mut Batch,
    /// The batch page being filled (null: the stack batch).
    cur: *mut Batch,
    len: usize,
    max: usize,
}

impl List {
    pub(super) fn new(max: usize) -> List {
        List { local: [[0; 2]; LOCAL], n_local: 0, head: core::ptr::null_mut(), cur: core::ptr::null_mut(), len: 0, max }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn batch(p: *mut Batch) -> Option<&'static mut Batch> {
        // SAFETY: null or a batch page this list took from the hook and still owns (`room`); only this list
        // reaches it, through `&mut self`.
        unsafe { p.as_mut() }
    }

    /// Room for one more entry, linking the next batch page if it must: false when the list is at `max` or no
    /// page could be had without waiting.
    pub(super) fn room(&mut self) -> bool {
        if self.len >= self.max {
            return false;
        }
        if self.cur.is_null() {
            if self.n_local < LOCAL {
                return true;
            }
            if self.head.is_null() {
                return self.link(None);
            }
            self.cur = self.head;
            return true;
        }
        let Some(b) = Self::batch(self.cur) else { return false };
        if b.n < PER_PAGE {
            return true;
        }
        if !b.next.is_null() {
            self.cur = b.next;
            return true;
        }
        let at = self.cur;
        self.link(Some(at))
    }

    /// A new batch page after `after` (None: the first).
    fn link(&mut self, after: Option<*mut Batch>) -> bool {
        let Some(phys) = batch_frame() else {
            NO_BATCH.fetch_add(1, Relaxed);
            return false;
        };
        let p = super::phys_to_virt(phys) as *mut Batch;
        // SAFETY: a fresh frame from the hook, direct-mapped and 4 KiB (the header and entries fit, asserted
        // above); nobody else knows it. The header is written before the page is linked.
        unsafe {
            (&raw mut (*p).next).write(core::ptr::null_mut());
            (&raw mut (*p).n).write(0);
            (&raw mut (*p).phys).write(phys);
        }
        match after.and_then(Self::batch) {
            Some(b) => b.next = p,
            None => self.head = p,
        }
        self.cur = p;
        true
    }

    /// Adds an entry; the caller asked `room` first (an entry without room is dropped and must not happen).
    pub(super) fn push(&mut self, phys: u64, size: PageSize, tag: u32) {
        let e = [phys | size_code(size), u64::from(tag)];
        match Self::batch(self.cur) {
            None if self.n_local < LOCAL => {
                self.local[self.n_local] = e;
                self.n_local += 1;
            }
            Some(b) if b.n < PER_PAGE => {
                b.items[b.n] = e;
                b.n += 1;
            }
            _ => return,
        }
        self.len += 1;
    }

    /// Hands every entry to `f` in the order gathered and empties the list (its batch pages stay for reuse).
    pub(super) fn drain(&mut self, f: &mut dyn FnMut(u64, PageSize, u32)) {
        let mut give = |e: &Entry| f(e[0] & !0xFFF, size_of_code(e[0] & 0xFFF), e[1] as u32);
        self.local[..self.n_local].iter().for_each(&mut give);
        let mut p = self.head;
        while let Some(b) = Self::batch(p) {
            b.items[..b.n].iter().for_each(&mut give);
            b.n = 0;
            p = b.next;
        }
        self.n_local = 0;
        self.cur = core::ptr::null_mut();
        self.len = 0;
    }
}

impl Drop for List {
    fn drop(&mut self) {
        let mut p = core::mem::replace(&mut self.head, core::ptr::null_mut());
        while let Some(b) = Self::batch(p) {
            p = b.next;
            free_frame(b.phys);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn without_batch_pages_the_stack_batch_is_the_list() {
        let mut l = List::new(GATHER_TEST_MAX);
        for i in 0..LOCAL as u64 {
            assert!(l.room(), "room for entry {i}");
            l.push(i << 12, if i % 2 == 0 { PageSize::Size4K } else { PageSize::Size2M }, i as u32);
        }
        // No hook installed on the host: no batch page, the owner flushes (`tlb_flush_mmu`).
        assert!(!l.room());
        assert!(no_batch() >= 1);
        let mut got = alloc::vec::Vec::new();
        l.drain(&mut |p, s, t| got.push((p, s, t)));
        assert_eq!(got.len(), LOCAL);
        for (i, (p, s, t)) in got.iter().enumerate() {
            assert_eq!((*p, *t), ((i as u64) << 12, i as u32), "entry {i} in the order gathered");
            assert_eq!(*s, if i % 2 == 0 { PageSize::Size4K } else { PageSize::Size2M });
        }
        assert!(l.is_empty() && l.room(), "a drained list takes entries again");
    }

    #[test]
    fn the_bound_holds_before_the_stack_batch_is_full() {
        let mut l = List::new(3);
        for _ in 0..3 {
            assert!(l.room());
            l.push(0x1000, PageSize::Size4K, 0);
        }
        assert!(!l.room());
    }

    const GATHER_TEST_MAX: usize = 10_000;
}
