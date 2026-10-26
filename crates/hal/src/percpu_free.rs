//! Small lock-free helpers for hal-internal state that is written once at init.

use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, Ordering};

/// A set of physical ranges [start, end), written once at kernel init.
pub struct Ranges {
    set: AtomicBool,
    v: UnsafeCell<Vec<(u64, u64)>>,
}

// SAFETY: written once (before `set` is published), read-only afterwards.
unsafe impl Sync for Ranges {}

impl Ranges {
    pub const fn new() -> Self {
        Ranges { set: AtomicBool::new(false), v: UnsafeCell::new(Vec::new()) }
    }
    pub fn set(&self, ranges: Vec<(u64, u64)>) {
        if self.set.load(Ordering::Acquire) {
            return;
        }
        // SAFETY: called once during single-threaded init, before any reader saw `set == true`.
        unsafe { *self.v.get() = ranges };
        self.set.store(true, Ordering::Release);
    }
    /// True if [start, end) overlaps any range. Before `set` everything counts as RAM (refuse).
    pub fn overlaps(&self, start: u64, end: u64) -> bool {
        if !self.set.load(Ordering::Acquire) {
            return true;
        }
        // SAFETY: published and never written again.
        let v = unsafe { &*self.v.get() };
        v.iter().any(|&(s, e)| start < e && s < end)
    }
}
