//! An indexed binary min-heap (pure; host-tested): each element knows its own position, so a timer can be
//! removed or moved in O(log n) when it is cancelled or re-armed (Linux uses an rbtree, `timerqueue`). Pushing
//! never allocates unless the caller asked for room: hard-IRQ code arms timers, and the heap there must not grow
//! (`push_within`); thread code makes room first (`reserve`).

use alloc::vec::Vec;

/// What the heap needs from an element: its order key (smaller first) and a slot for its position.
pub trait Keyed {
    fn key(&self) -> (u64, u64);
    fn pos(&self) -> usize;
    fn set_pos(&self, p: usize);
}

pub struct Heap<T: Keyed> {
    v: Vec<T>,
}

impl<T: Keyed> Default for Heap<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[allow(dead_code)] // `is_empty`, `iter`, `reserve_total`: the tests and later callers
impl<T: Keyed> Heap<T> {
    pub const fn new() -> Self {
        Heap { v: Vec::new() }
    }
    pub fn len(&self) -> usize {
        self.v.len()
    }
    pub fn is_empty(&self) -> bool {
        self.v.is_empty()
    }
    pub fn capacity(&self) -> usize {
        self.v.capacity()
    }
    /// Makes room for `n` elements in total (allocates; thread context).
    pub fn reserve_total(&mut self, n: usize) -> bool {
        n <= self.v.capacity() || self.v.try_reserve(n - self.v.len()).is_ok()
    }
    /// Moves the elements into `v` (empty, with room for them; positions stay) and returns the old storage, now
    /// empty, for the caller to free outside its lock. A `v` that is too small or not empty comes back unused.
    pub fn replace_storage(&mut self, mut v: Vec<T>) -> Vec<T> {
        if !v.is_empty() || v.capacity() < self.v.len() {
            return v;
        }
        v.extend(self.v.drain(..));
        core::mem::replace(&mut self.v, v)
    }
    /// Inserts `t` if there is room without allocating; gives it back otherwise.
    pub fn push_within(&mut self, t: T) -> Result<(), T> {
        if self.v.len() == self.v.capacity() {
            return Err(t);
        }
        let i = self.v.len();
        t.set_pos(i);
        self.v.push(t);
        self.up(i);
        Ok(())
    }
    pub fn peek(&self) -> Option<&T> {
        self.v.first()
    }
    pub fn pop(&mut self) -> Option<T> {
        if self.v.is_empty() { None } else { Some(self.remove(0)) }
    }
    /// Removes the element at `pos` (its own `pos()`); panics on a position outside the heap (an invariant the
    /// owner keeps under its lock, never input).
    pub fn remove(&mut self, pos: usize) -> T {
        let last = self.v.len() - 1;
        self.v.swap(pos, last);
        let t = self.v.pop().expect("heap not empty");
        if pos < self.v.len() {
            self.v[pos].set_pos(pos);
            self.fix(pos);
        }
        t
    }
    /// Restores the order after the key of the element at `pos` changed.
    pub fn fix(&mut self, pos: usize) {
        let p = self.up(pos);
        self.down(p);
    }
    pub fn iter(&self) -> impl Iterator<Item = &T> {
        self.v.iter()
    }

    fn swap(&mut self, a: usize, b: usize) {
        self.v.swap(a, b);
        self.v[a].set_pos(a);
        self.v[b].set_pos(b);
    }
    fn up(&mut self, mut i: usize) -> usize {
        while i > 0 {
            let p = (i - 1) / 2;
            if self.v[i].key() >= self.v[p].key() {
                break;
            }
            self.swap(i, p);
            i = p;
        }
        i
    }
    fn down(&mut self, mut i: usize) {
        loop {
            let (l, r) = (2 * i + 1, 2 * i + 2);
            let mut m = i;
            if l < self.v.len() && self.v[l].key() < self.v[m].key() {
                m = l;
            }
            if r < self.v.len() && self.v[r].key() < self.v[m].key() {
                m = r;
            }
            if m == i {
                return;
            }
            self.swap(i, m);
            i = m;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::rc::Rc;
    use core::cell::Cell;

    struct E {
        k: Cell<(u64, u64)>,
        p: Cell<usize>,
    }
    impl Keyed for Rc<E> {
        fn key(&self) -> (u64, u64) {
            self.k.get()
        }
        fn pos(&self) -> usize {
            self.p.get()
        }
        fn set_pos(&self, p: usize) {
            self.p.set(p)
        }
    }
    fn e(d: u64, s: u64) -> Rc<E> {
        Rc::new(E { k: Cell::new((d, s)), p: Cell::new(usize::MAX) })
    }

    fn check(h: &Heap<Rc<E>>) {
        for (i, x) in h.iter().enumerate() {
            assert_eq!(x.pos(), i, "position kept");
            if i > 0 {
                assert!(h.v[(i - 1) / 2].key() <= x.key(), "heap order");
            }
        }
    }

    #[test]
    fn pops_in_order_and_ties_by_sequence() {
        let mut h = Heap::new();
        assert!(h.reserve_total(64));
        let mut seq = 0;
        let mut keys: Vec<u64> = (0..50).map(|i| (i * 7919) % 31).collect();
        for &d in &keys {
            seq += 1;
            h.push_within(e(d, seq)).ok().unwrap();
            check(&h);
        }
        keys.sort();
        let mut last = (0, 0);
        for &d in &keys {
            let x = h.pop().unwrap();
            assert_eq!(x.key().0, d);
            assert!(x.key() > last, "equal deadlines come out in arm order");
            last = x.key();
            check(&h);
        }
        assert!(h.pop().is_none());
    }

    #[test]
    fn remove_and_fix_anywhere() {
        let mut h = Heap::new();
        assert!(h.reserve_total(16));
        let xs: Vec<_> = (0..16).map(|i| e(100 - i, i)).collect();
        for x in &xs {
            h.push_within(x.clone()).ok().unwrap();
        }
        let r = h.remove(xs[5].pos());
        assert_eq!(r.key(), (95, 5));
        check(&h);
        xs[10].k.set((1, 99));
        h.fix(xs[10].pos());
        check(&h);
        assert_eq!(h.peek().unwrap().key(), (1, 99));
        xs[10].k.set((1000, 99));
        h.fix(xs[10].pos());
        check(&h);
        assert_eq!(h.len(), 15);
    }

    #[test]
    fn full_heap_refuses_without_allocating() {
        let mut h: Heap<Rc<E>> = Heap::new();
        assert!(h.push_within(e(1, 1)).is_err(), "no room reserved");
        assert!(h.reserve_total(1));
        let cap = h.capacity();
        for i in 0..cap {
            assert!(h.push_within(e(i as u64, 0)).is_ok());
        }
        assert!(h.push_within(e(9, 9)).is_err());
        assert_eq!(h.capacity(), cap, "never grew by itself");
        let small = h.replace_storage(Vec::with_capacity(1));
        assert_eq!(small.capacity(), 1, "too small: given back");
        let old = h.replace_storage(Vec::with_capacity(cap * 2));
        assert!(old.is_empty() && old.capacity() == cap);
        check(&h);
        assert!(h.push_within(e(9, 9)).is_ok(), "room now");
        check(&h);
    }
}
