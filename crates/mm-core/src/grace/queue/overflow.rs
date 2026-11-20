//! MEMCRASH checker 13.11.2026: a reclaim item that found every stash full or held went to the queue and waited
//! for `mm.grace` when another CPU held it (B4: reclaim never waits for a lock). A fixed pool of slots, each
//! taken with one compare-and-swap, keeps such items until the next drain hands them over; only a full pool
//! leaves the item to the old road (counted, `mm.grace.reclaim_waits`).

use super::Deferred;
use core::cell::UnsafeCell;
use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicU64, AtomicU8, Ordering::*};

const FREE: u8 = 0;
/// Being filled or emptied by one CPU (an interrupt on top of it passes over the slot).
const BUSY: u8 = 1;
const FULL: u8 = 2;

pub(super) struct Pool<const N: usize> {
    state: [AtomicU8; N],
    items: [UnsafeCell<MaybeUninit<Deferred>>; N],
    full: AtomicU64,
}

// SAFETY: a slot's item is written only by the CPU that moved it FREE -> BUSY and read only by the one that
// moved it FULL -> BUSY (Acquire/Release on `state`), so no two CPUs touch one item at once; `Deferred` is Send.
unsafe impl<const N: usize> Sync for Pool<N> {}

impl<const N: usize> Pool<N> {
    pub(super) const fn new() -> Pool<N> {
        Pool { state: [const { AtomicU8::new(FREE) }; N], items: [const { UnsafeCell::new(MaybeUninit::uninit()) }; N], full: AtomicU64::new(0) }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.full.load(Relaxed) == 0
    }

    pub(super) const fn len_max(&self) -> usize {
        N
    }

    /// `d` into a free slot, looking from `start` on. Err: every slot is taken.
    pub(super) fn put(&self, d: Deferred, start: usize) -> Result<(), Deferred> {
        for k in 0..N {
            let i = (start + k) % N;
            if self.state[i].compare_exchange(FREE, BUSY, Acquire, Relaxed).is_ok() {
                // SAFETY: this CPU moved the slot to BUSY: nobody else reads or writes its item until FULL.
                unsafe { (*self.items[i].get()).write(d) };
                self.full.fetch_add(1, Relaxed);
                self.state[i].store(FULL, Release);
                return Ok(());
            }
        }
        Err(d)
    }

    pub(super) fn is_full(&self, i: usize) -> bool {
        self.state[i].load(Relaxed) == FULL
    }

    /// The item of slot `i`, if it holds one.
    pub(super) fn take(&self, i: usize) -> Option<Deferred> {
        self.state[i].compare_exchange(FULL, BUSY, Acquire, Relaxed).ok()?;
        // SAFETY: FULL means `put` wrote the item and released it; this CPU now owns it until FREE.
        let d = unsafe { (*self.items[i].get()).assume_init_read() };
        self.full.fetch_sub(1, Relaxed);
        self.state[i].store(FREE, Release);
        Some(d)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;
    use std::sync::Arc;
    use std::vec::Vec;

    unsafe fn nothing(_: *mut ()) {}

    fn item(cookie: u64) -> Deferred {
        Deferred { cookie, ptr: core::ptr::null_mut(), drop: nothing, bytes: 0, held: 0 }
    }

    #[test]
    fn a_full_pool_gives_the_item_back_and_every_item_comes_out_once() {
        let p: Pool<8> = Pool::new();
        for c in 0..8 {
            assert!(p.put(item(c), c as usize * 3).is_ok());
        }
        assert_eq!(p.put(item(99), 0).err().map(|d| d.cookie), Some(99));
        let mut seen: Vec<u64> = (0..p.len_max()).filter_map(|i| p.take(i)).map(|d| d.cookie).collect();
        seen.sort_unstable();
        assert_eq!(seen, (0..8).collect::<Vec<_>>());
        assert!(p.is_empty() && p.take(0).is_none());
    }

    /// Eight threads put while one takes: nothing is lost or seen twice, and nobody waits for a lock.
    #[test]
    fn puts_and_takes_from_many_threads_lose_nothing() {
        let p: Arc<Pool<64>> = Arc::new(Pool::new());
        let putters: Vec<_> = (0..8u64)
            .map(|t| {
                let p = p.clone();
                std::thread::spawn(move || {
                    for k in 0..2000u64 {
                        let mut d = item(t * 10_000 + k);
                        while let Err(back) = p.put(d, (t * 8) as usize) {
                            d = back;
                            std::thread::yield_now();
                        }
                    }
                })
            })
            .collect();
        let mut got = Vec::new();
        while got.len() < 16_000 {
            for i in 0..64 {
                if p.is_full(i) && let Some(d) = p.take(i) {
                    got.push(d.cookie);
                }
            }
        }
        putters.into_iter().for_each(|h| h.join().unwrap());
        got.sort_unstable();
        got.dedup();
        assert_eq!(got.len(), 16_000);
        assert!(p.is_empty());
    }
}
