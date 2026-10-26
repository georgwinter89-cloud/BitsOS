//! FS-13: a file's recorded page ranges, sorted and merged. Ranges closer than `GAP` pages are joined: the
//! preload then reads the few pages between them in the same request instead of seeking again (a USB stick
//! answers one 64 KiB command faster than two 4 KiB ones).

use alloc::vec::Vec;
#[cfg(test)]
extern crate std;

/// Pages between two ranges that are read anyway rather than kept apart (32 KiB).
pub const GAP: u64 = 8;

/// `[first, end)` page ranges, sorted, never closer than `GAP` to each other.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Ranges {
    v: Vec<(u64, u64)>,
}

impl Ranges {
    pub const fn new() -> Ranges {
        Ranges { v: Vec::new() }
    }

    pub fn as_slice(&self) -> &[(u64, u64)] {
        &self.v
    }

    pub fn is_empty(&self) -> bool {
        self.v.is_empty()
    }

    /// Pages covered.
    pub fn pages(&self) -> u64 {
        self.v.iter().map(|&(a, b)| b - a).sum()
    }

    /// Adds `[first, end)`; false when memory for a new entry ran out (the range is then not recorded).
    pub fn add(&mut self, first: u64, end: u64) -> bool {
        if first >= end {
            return true;
        }
        // The first range that may touch the new one (its end plus the gap reaches `first`).
        let lo = self.v.partition_point(|&(_, b)| b.saturating_add(GAP) < first);
        let mut hi = lo;
        let (mut a, mut b) = (first, end);
        while hi < self.v.len() && self.v[hi].0 <= b.saturating_add(GAP) {
            a = a.min(self.v[hi].0);
            b = b.max(self.v[hi].1);
            hi += 1;
        }
        if hi > lo {
            self.v[lo] = (a, b);
            self.v.drain(lo + 1..hi);
            return true;
        }
        if no_memory() || self.v.try_reserve(1).is_err() {
            return false;
        }
        self.v.insert(lo, (a, b));
        true
    }

    /// Adds every range of `o`; false when memory ran out part way.
    pub fn add_all(&mut self, o: &Ranges) -> bool {
        o.v.iter().all(|&(a, b)| self.add(a, b))
    }

    /// Keeps the first `pages` pages (lowest offsets first); returns how many pages were dropped.
    pub fn cut_to(&mut self, pages: u64) -> u64 {
        let mut left = pages;
        let mut keep = 0;
        let mut dropped = 0;
        for r in self.v.iter_mut() {
            let n = r.1 - r.0;
            if left == 0 {
                dropped += n;
                continue;
            }
            if n > left {
                dropped += n - left;
                r.1 = r.0 + left;
            }
            left -= (r.1 - r.0).min(left);
            keep += 1;
        }
        self.v.truncate(keep);
        dropped
    }
}

/// Host tests: the next `n` new entries of this thread find no memory (`Ranges::add` returns false).
#[cfg(test)]
pub(crate) fn fail_new_entries(n: u32) {
    FAIL.with(|f| f.set(n));
}

#[cfg(test)]
std::thread_local! {
    static FAIL: core::cell::Cell<u32> = const { core::cell::Cell::new(0) };
}

#[cfg(test)]
fn no_memory() -> bool {
    FAIL.with(|f| {
        let n = f.get();
        f.set(n.saturating_sub(1));
        n > 0
    })
}

#[cfg(not(test))]
const fn no_memory() -> bool {
    false
}
