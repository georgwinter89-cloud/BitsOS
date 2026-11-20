//! A counting front for the host's system allocator, for host test binaries that measure how many heap blocks
//! one call makes (BIGDIR) or the largest block asked for under a spin lock (ATOMIC-3). It lives here because a test outside a trusted crate's src/ may not use `unsafe`
//! (forbid-unsafe gate), and implementing `GlobalAlloc` needs it. Only built with the `host-count` feature, which
//! only dev-dependencies turn on; the kernel never sees it.

extern crate std;

use core::alloc::{GlobalAlloc, Layout};
use core::cell::Cell;
use std::alloc::System;

std::thread_local! {
    static BLOCKS: Cell<u64> = const { Cell::new(0) };
    static UNDER_SPIN: Cell<usize> = const { Cell::new(0) };
}

/// Register as `#[global_allocator]`; `alloc` and `realloc` count one block each on the calling thread and note
/// its size when the thread holds a spin lock (ksync's preempt count).
pub struct Counting;

fn note(size: usize) {
    let _ = BLOCKS.try_with(|c| c.set(c.get() + 1));
    if ksync::preempt_count() > 0 {
        let _ = UNDER_SPIN.try_with(|c| c.set(c.get().max(size)));
    }
}

// SAFETY: every call goes to the system allocator unchanged; the counter is a plain thread-local cell, and a
// thread-local whose storage is gone (thread exit) is skipped rather than touched.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        note(l.size());
        // SAFETY: the caller keeps GlobalAlloc's contract, which is System's.
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        // SAFETY: as alloc.
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        note(n);
        // SAFETY: as alloc.
        unsafe { System.realloc(p, l, n) }
    }
}

/// The blocks the calling thread has asked for so far.
pub fn heap_blocks() -> u64 {
    BLOCKS.with(|c| c.get())
}

/// The largest block the calling thread asked for under a spin lock while `f` ran.
pub fn largest_under_spin<T>(f: impl FnOnce() -> T) -> (usize, T) {
    UNDER_SPIN.with(|c| c.set(0));
    let r = f();
    (UNDER_SPIN.with(|c| c.get()), r)
}
