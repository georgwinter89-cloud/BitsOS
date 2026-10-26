//! Virtual address ranges of the vmap area (pure, host-tested). Free space is a map of disjoint ranges;
//! allocations come top-down (so the G0 MMIO bump, which grows up from the bottom of vmap, meets them only
//! after the whole area is used) and each one reserves `guard` unmapped bytes below its body, so two bodies
//! never touch: a run past either end of a mapping hits a guard page or free space and faults.

use alloc::vec::Vec;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VaSpace {
    lo: u64,
    hi: u64,
    /// (start, end) of each free range, sorted; disjoint, never adjacent (merged). A sorted Vec rather than a
    /// BTreeMap: every change that adds a range reserves its room first (`try_reserve`), so an empty kernel
    /// heap makes the call fail instead of aborting the kernel (R2). Reservations may be made under spin locks.
    free: Vec<(u64, u64)>,
}

/// One reservation: [start, body) is the guard, [body, end) the part that is mapped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reservation {
    pub start: u64,
    pub body: u64,
    pub end: u64,
}

impl VaSpace {
    /// Everything in [lo, hi) free (page-aligned bounds).
    pub fn new(lo: u64, hi: u64) -> VaSpace {
        let mut free = Vec::new();
        if hi > lo {
            free.push((lo, hi));
        }
        VaSpace { lo, hi, free }
    }

    /// Index of the free range starting at or below `a`.
    fn at_or_below(&self, a: u64) -> Option<usize> {
        self.free.partition_point(|r| r.0 <= a).checked_sub(1)
    }

    /// Replaces free range `i` by what is left of it around [start, end) (`room` reserved by the caller).
    fn carve(&mut self, i: usize, start: u64, end: u64) {
        let (s, e) = self.free[i];
        match (start > s, e > end) {
            (true, true) => {
                self.free[i] = (s, start);
                self.free.insert(i + 1, (end, e));
            }
            (true, false) => self.free[i] = (s, start),
            (false, true) => self.free[i] = (end, e),
            (false, false) => {
                self.free.remove(i);
            }
        }
    }

    /// Takes exactly [start, end) out of the free space (mappings that exist already). False if any part
    /// of it is not free, or there is no heap for the bookkeeping.
    pub fn reserve(&mut self, start: u64, end: u64) -> bool {
        let Some(i) = self.at_or_below(start) else { return false };
        let (_, e) = self.free[i];
        if end <= start || end > e || self.free.try_reserve(1).is_err() {
            return false;
        }
        self.carve(i, start, end);
        true
    }

    /// `len` bytes (page multiple) with the body aligned to `align` (a power of two, at least 4 KiB) and
    /// `guard` bytes below it, from the highest free range that fits. None also when there is no heap for
    /// the bookkeeping.
    pub fn alloc(&mut self, len: u64, align: u64, guard: u64) -> Option<Reservation> {
        if len == 0 || !align.is_power_of_two() {
            return None;
        }
        let need = len.checked_add(guard)?;
        let i = self.free.iter().rposition(|&(s, e)| {
            e.checked_sub(len).map(|b| b & !(align - 1)).is_some_and(|body| body >= s && body - s >= guard && e - s >= need)
        })?;
        self.free.try_reserve(1).ok()?;
        let (_, e) = self.free[i];
        let body = (e - len) & !(align - 1);
        let r = Reservation { start: body - guard, body, end: body + len };
        self.carve(i, r.start, r.end);
        Some(r)
    }

    /// Gives [start, end) back (a whole reservation, guard included); merges with free neighbours.
    /// False (nothing changed) if the range is outside the space, overlaps free space, or needs a new free
    /// range while the kernel heap has no room for it (the range then stays out of use).
    pub fn release(&mut self, start: u64, end: u64) -> bool {
        if end <= start || start < self.lo || end > self.hi {
            return false;
        }
        let i = self.free.partition_point(|r| r.0 < end);
        if i > 0 && self.free[i - 1].1 > start {
            return false;
        }
        let merge_before = i > 0 && self.free[i - 1].1 == start;
        let merge_after = i < self.free.len() && self.free[i].0 == end;
        match (merge_before, merge_after) {
            (true, true) => {
                self.free[i - 1].1 = self.free[i].1;
                self.free.remove(i);
            }
            (true, false) => self.free[i - 1].1 = end,
            (false, true) => self.free[i].0 = start,
            (false, false) => {
                if self.free.try_reserve(1).is_err() {
                    return false;
                }
                self.free.insert(i, (start, end));
            }
        }
        true
    }

    pub fn free_bytes(&self) -> u64 {
        self.free.iter().map(|(s, e)| e - s).sum()
    }
    pub fn free_ranges(&self) -> usize {
        self.free.len()
    }
    pub fn is_free(&self, a: u64) -> bool {
        self.at_or_below(a).is_some_and(|i| a < self.free[i].1)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec::Vec;

    const P: u64 = 4096;
    const LO: u64 = 0xFFFF_C900_0000_0000;
    const HI: u64 = LO + (1 << 30);

    #[test]
    fn top_down_with_guards() {
        let mut v = VaSpace::new(LO, HI);
        let a = v.alloc(3 * P, P, P).unwrap();
        assert_eq!(a, Reservation { start: HI - 4 * P, body: HI - 3 * P, end: HI });
        let b = v.alloc(P, P, P).unwrap();
        assert_eq!(b.end, a.start, "b sits right below a's guard");
        assert!(!v.is_free(a.body - 1) && !v.is_free(b.body - 1));
        let c = v.alloc(2 << 20, 2 << 20, P).unwrap();
        assert_eq!(c.body % (2 << 20), 0);
        assert!(c.end <= b.start);
        assert_eq!(v.free_bytes(), (1 << 30) - (a.end - a.start) - (b.end - b.start) - (c.end - c.start));
    }

    #[test]
    fn release_merges_back_to_one_range() {
        let mut v = VaSpace::new(LO, HI);
        let r: Vec<_> = (1..20).map(|i| v.alloc(i * P, P, P).unwrap()).collect();
        for x in r.iter().step_by(2).chain(r.iter().skip(1).step_by(2)) {
            assert!(v.release(x.start, x.end));
        }
        assert_eq!(v, VaSpace::new(LO, HI));
        assert!(!v.release(LO, LO + P), "already free");
        assert!(!v.release(HI, HI + P), "outside");
    }

    #[test]
    fn holes_are_reused_and_bodies_never_touch() {
        let mut v = VaSpace::new(LO, LO + 64 * P);
        let a = v.alloc(4 * P, P, P).unwrap();
        let b = v.alloc(4 * P, P, P).unwrap();
        let c = v.alloc(4 * P, P, P).unwrap();
        assert!(v.release(b.start, b.end));
        let d = v.alloc(2 * P, P, P).unwrap();
        assert!(d.start >= c.end && d.end <= a.start, "the hole between a and c is reused");
        assert!(d.end == a.start, "top of the hole");
        let mut bodies = [(a.body, a.end), (c.body, c.end), (d.body, d.end)];
        bodies.sort();
        for w in bodies.windows(2) {
            assert!(w[1].0 > w[0].1, "a guard page between {w:?}");
        }
    }

    #[test]
    fn exhaustion_and_bad_requests() {
        let mut v = VaSpace::new(LO, LO + 8 * P);
        assert_eq!(v.alloc(8 * P, P, P), None, "no room for the guard");
        assert!(v.alloc(7 * P, P, P).is_some());
        assert_eq!(v.alloc(P, P, P), None);
        assert_eq!(v.alloc(0, P, P), None);
        assert_eq!(v.alloc(P, 3 * P, P), None);
        assert_eq!(v.alloc(u64::MAX - 10, P, P), None);
    }

    #[test]
    fn reserve_carves_exact_ranges() {
        let mut v = VaSpace::new(LO, HI);
        assert!(v.reserve(LO, LO + 0x100_0000), "the framebuffer at the bottom");
        assert!(!v.reserve(LO + 0x80_0000, LO + 0x90_0000), "not free any more");
        assert!(v.reserve(LO + 0x200_0000, LO + 0x200_1000));
        assert_eq!(v.free_ranges(), 2);
        assert!(!v.is_free(LO) && v.is_free(LO + 0x100_0000));
    }

    #[test]
    fn random_cycles_keep_the_invariants() {
        let mut v = VaSpace::new(LO, LO + 4096 * P);
        let mut live: Vec<Reservation> = Vec::new();
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        for _ in 0..20_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            if live.is_empty() || !x.is_multiple_of(3) {
                let len = (x >> 8) % 32 * P + P;
                let align = if x & 0x10 != 0 { 16 * P } else { P };
                if let Some(r) = v.alloc(len, align, P) {
                    live.push(r);
                }
            } else {
                let r = live.swap_remove((x as usize >> 4) % live.len());
                assert!(v.release(r.start, r.end));
            }
        }
        live.sort_by_key(|r| r.start);
        for w in live.windows(2) {
            assert!(w[0].end <= w[1].start, "reservations overlap");
            assert!(w[1].body > w[0].end, "bodies touch");
        }
        let used: u64 = live.iter().map(|r| r.end - r.start).sum();
        assert_eq!(v.free_bytes() + used, 4096 * P);
        for r in live {
            assert!(v.release(r.start, r.end));
        }
        assert_eq!(v, VaSpace::new(LO, LO + 4096 * P));
    }
}
