//! Growing a `Vec` or `VecDeque` that lives behind a spin lock without allocating under the lock.
//!
//! Under a spin lock the heap can neither reclaim nor fall back to single frames, so growing a queue there
//! needs a free contiguous run: an infallible push panics when there is none (Mini-PC, 11.11.2026), a
//! `try_reserve` fails although memory is there. A `Spare` allocates the larger queue with the lock dropped
//! and swaps it in under the lock, where only plain moves are left:
//!
//! ```ignore
//! let mut spare = Spare::new();   // before the guard: the emptied old queue is freed after the unlock
//! loop {
//!     let mut g = lock.lock();
//!     // ... every check the push depends on, again in each round ...
//!     if !spare.fits(&mut g.q, 1) {
//!         drop(g);
//!         spare.grow()?;
//!         continue;
//!     }
//!     g.q.push_back(x);
//!     break;
//! }
//! ```

use alloc::collections::VecDeque;
use alloc::vec::Vec;

/// What `Spare` needs of a queue.
pub trait Growable: Sized {
    fn empty() -> Self;
    fn used(&self) -> usize;
    fn room(&self) -> usize;
    fn try_room(&mut self, n: usize) -> bool;
    fn must_room(&mut self, n: usize);
    /// Moves everything of `old` behind `self`; `self` has the room.
    fn take_all(&mut self, old: &mut Self);
}

impl<T> Growable for Vec<T> {
    fn empty() -> Self {
        Vec::new()
    }
    fn used(&self) -> usize {
        self.len()
    }
    fn room(&self) -> usize {
        self.capacity() - self.len()
    }
    fn try_room(&mut self, n: usize) -> bool {
        self.try_reserve_exact(n).is_ok()
    }
    fn must_room(&mut self, n: usize) {
        self.reserve_exact(n);
    }
    fn take_all(&mut self, old: &mut Self) {
        self.extend(old.drain(..));
    }
}

impl<T> Growable for VecDeque<T> {
    fn empty() -> Self {
        VecDeque::new()
    }
    fn used(&self) -> usize {
        self.len()
    }
    fn room(&self) -> usize {
        self.capacity() - self.len()
    }
    fn try_room(&mut self, n: usize) -> bool {
        self.try_reserve_exact(n).is_ok()
    }
    fn must_room(&mut self, n: usize) {
        self.reserve_exact(n);
    }
    fn take_all(&mut self, old: &mut Self) {
        self.extend(old.drain(..));
    }
}

/// The larger queue in the making; see the module doc.
pub struct Spare<Q> {
    fresh: Option<Q>,
    want: usize,
}

impl<Q: Growable> Spare<Q> {
    pub const fn new() -> Self {
        Spare { fresh: None, want: 0 }
    }

    /// Under the lock: whether `q` now has room for `n` more elements (it is moved into the grown queue when
    /// that is large enough). False: drop the lock, `grow`, take the lock and look again.
    pub fn fits(&mut self, q: &mut Q, n: usize) -> bool {
        if q.room() >= n {
            return true;
        }
        let need = q.used() + n;
        if let Some(f) = self.fresh.as_mut().filter(|f| f.room() >= need) {
            f.take_all(q);
            core::mem::swap(q, f);
            return true;
        }
        // Doubling keeps a queue that fills one by one from coming back here every time.
        self.want = need.max(q.used() * 2);
        false
    }

    /// With no spin lock held: allocates what the last `fits` missed. `Err`: no memory.
    pub fn grow(&mut self) -> Result<(), ()> {
        // The stale one goes first, so both are never held at once.
        self.fresh = None;
        let mut f = Q::empty();
        if !f.try_room(self.want) {
            return Err(());
        }
        self.fresh = Some(f);
        Ok(())
    }

    /// `grow` where the push cannot fail (as `Vec::reserve`: no memory ends the kernel, but here the heap may
    /// wait for some first).
    pub fn must_grow(&mut self) {
        self.fresh = None;
        let mut f = Q::empty();
        f.must_room(self.want);
        self.fresh = Some(f);
    }
}

impl<Q: Growable> Default for Spare<Q> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn room_at_hand_needs_no_spare() {
        let mut q: VecDeque<u32> = VecDeque::with_capacity(4);
        let mut s = Spare::new();
        assert!(s.fits(&mut q, 4));
        assert!(s.fresh.is_none());
    }

    #[test]
    fn grows_outside_and_keeps_the_order() {
        let mut q: Vec<u32> = Vec::new();
        for i in 0..100 {
            let mut s = Spare::new();
            let cap = q.capacity();
            while !s.fits(&mut q, 1) {
                s.grow().unwrap();
            }
            q.push(i);
            assert!(q.capacity() == cap || s.fresh.as_ref().is_some_and(|old| old.is_empty()), "the old queue is left empty for the caller to free");
        }
        assert!(q.iter().copied().eq(0..100));
    }

    /// The queue filled up further between the unlock and the relock: the spare made for the old size is not
    /// used, a larger one is asked for.
    #[test]
    fn queue_that_grew_meanwhile_asks_again() {
        let mut q: VecDeque<u32> = VecDeque::new();
        q.extend(0..8);
        q.shrink_to_fit();
        let mut s = Spare::new();
        assert!(!s.fits(&mut q, 1));
        s.grow().unwrap();
        let made = s.fresh.as_ref().unwrap().capacity();
        // Another thread's pushes while the lock was dropped.
        q.extend(8..8 + made as u32);
        q.shrink_to_fit();
        assert!(!s.fits(&mut q, 1), "the spare is too small now");
        s.grow().unwrap();
        assert!(s.fits(&mut q, 1));
        q.push_back(u32::MAX);
        assert!(q.iter().copied().take(8 + made).eq(0..8 + made as u32));
        assert_eq!(q.back(), Some(&u32::MAX));
    }
}
