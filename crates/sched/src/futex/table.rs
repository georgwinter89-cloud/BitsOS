//! The queue logic of one hash bucket, without locks or threads (host-tested): Linux's `futex_wake`,
//! `futex_requeue` (non-PI part) and the wake halves of `futex_wake_op` on a list of waiters in arrival order
//! (linux-6.6/kernel/futex/waitwake.c:143, requeue.c:364). The kernel side (`super`) holds the bucket locks and
//! wakes the threads; here only who is taken off, moved or left.
//!
//! The order is FIFO: Linux queues by priority (`plist`) and every normal-priority waiter has the same one, so
//! among normal threads its order is arrival order too. SC-7b: PI waiters (FUTEX_LOCK_PI) sit in the same
//! buckets with an empty bitset (`PI_BITSET`, which no plain wait can have); who gets a PI lock next is decided
//! by `sched::pi` (class first, then arrival). A plain wake, requeue or wake_op that meets a PI waiter of its key
//! answers EINVAL (Linux: "this->pi_state || this->rt_waiter"), after the waiters before it were handled.

use alloc::vec::Vec;
use uapi::errno::EINVAL;

/// The bitset of a PI waiter's entry.
pub const PI_BITSET: u32 = 0;

/// What a futex is identified by (Linux `union futex_key`): a private futex by (address space, address), a
/// shared one on anonymous memory by the same pair but a different kind (Linux sets FUT_OFF_MMSHARED in the
/// offset, so a private and a shared operation on the same anonymous word never meet), a shared one on a file
/// or shmem page by (inode, page index in the file, offset).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Key {
    pub kind: KeyKind,
    /// The address space's identity (its root) or the inode's (its address while a reference is held).
    pub object: u64,
    /// The page's virtual address (private, mm-shared) or its index in the file (inode).
    pub page: u64,
    /// The word's offset in the page.
    pub offset: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum KeyKind {
    Private,
    MmShared,
    Inode,
}

impl Key {
    /// A hash for the bucket index (any good mix of all fields; the murmur3 finaliser).
    pub fn hash(&self) -> u64 {
        let mut h = self.object ^ self.page.rotate_left(17) ^ ((self.offset as u64) << 40) ^ ((self.kind as u64) << 61);
        h ^= h >> 33;
        h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
        h ^= h >> 33;
        h = h.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
        h ^ (h >> 33)
    }
}

/// One waiter: its key, its bitset (FUTEX_WAIT_BITSET), the slot (its index in a futex_waitv list, 0 for a
/// plain wait) and who it is.
#[derive(Debug)]
pub struct Entry<W> {
    pub key: Key,
    pub bitset: u32,
    pub slot: u32,
    pub who: W,
}

impl<W> Entry<W> {
    /// A FUTEX_LOCK_PI waiter.
    pub fn is_pi(&self) -> bool {
        self.bitset == PI_BITSET
    }
}

/// `futex_wake` on one bucket: takes matching entries (key equal, bitsets overlapping) off in order and hands
/// each to `woke`, until `nr_wake` are taken. Linux checks `++ret >= nr_wake` after each one, so a count of 0
/// or a negative one still wakes one waiter. Returns how many were taken. One pass over the bucket whatever
/// the count (the bucket lock is held: B4), as Linux's O(1) plist unlink per waiter. EINVAL when a PI waiter of
/// `key` comes before the count is reached (the ones before it are woken, as Linux's loop does).
pub fn wake<W>(q: &mut Vec<Entry<W>>, key: &Key, nr_wake: i32, bitset: u32, woke: &mut dyn FnMut(Entry<W>)) -> Result<i64, i64> {
    let limit = nr_wake.max(1) as usize;
    let (mut taken, mut pi) = (0usize, false);
    let take = |e: &mut Entry<W>| {
        if pi || taken >= limit || e.key != *key {
            return false;
        }
        if e.is_pi() {
            pi = true;
            return false;
        }
        if e.bitset & bitset == 0 {
            return false;
        }
        taken += 1;
        true
    };
    for e in q.extract_if(.., take) {
        woke(e);
    }
    if pi { Err(EINVAL) } else { Ok(taken as i64) }
}

/// How many entries of `q` wait on `key`, counting at most `cap` (to size the target bucket of a requeue).
pub fn count(q: &[Entry<impl Sized>], key: &Key, cap: usize) -> usize {
    q.iter().filter(|e| e.key == *key).take(cap).count()
}

/// The non-PI loop of `futex_requeue`: the first `nr_wake` waiters on `k1` are taken off and handed to `woke`,
/// the next `nr_requeue` get key `k2` and move to the tail of `q2` (`moved` sees each one in its new place; with
/// `q2` None both keys hash to the same bucket and the entry only changes its key, as Linux's `requeue_futex`).
/// Returns woken + requeued (Linux's `task_count`), or EINVAL when a PI waiter of `k1` is met before the counts
/// are used up (the ones before it stay woken or moved, as Linux). The caller has made room in `q2`; entries past
/// that room stay where they are (never happens with the room made). One pass over `q1` (bucket locks held: B4).
pub fn requeue<W>(
    q1: &mut Vec<Entry<W>>,
    q2: Option<&mut Vec<Entry<W>>>,
    k1: &Key,
    k2: &Key,
    nr_wake: i32,
    nr_requeue: i32,
    woke: &mut dyn FnMut(Entry<W>),
    moved: &mut dyn FnMut(&Entry<W>),
) -> Result<i64, i64> {
    let (nr_wake, nr_requeue) = (nr_wake as i64, nr_requeue as i64);
    let mut task_count: i64 = 0;
    let mut pi = false;
    match q2 {
        None => {
            let take = |e: &mut Entry<W>| {
                if pi || e.key != *k1 || task_count - nr_wake >= nr_requeue {
                    return false;
                }
                if e.is_pi() {
                    pi = true;
                    return false;
                }
                task_count += 1;
                if task_count <= nr_wake {
                    return true;
                }
                e.key = *k2;
                moved(e);
                false
            };
            for e in q1.extract_if(.., take) {
                woke(e);
            }
        }
        Some(q2) => {
            let room = (q2.capacity() - q2.len()) as i64;
            let take = |e: &mut Entry<W>| {
                if pi || e.key != *k1 || task_count - nr_wake >= nr_requeue || task_count - nr_wake >= room {
                    return false;
                }
                if e.is_pi() {
                    pi = true;
                    return false;
                }
                task_count += 1;
                true
            };
            let mut seen: i64 = 0;
            for mut e in q1.extract_if(.., take) {
                seen += 1;
                if seen <= nr_wake {
                    woke(e);
                } else {
                    e.key = *k2;
                    q2.push(e);
                    if let Some(last) = q2.last() {
                        moved(last);
                    }
                }
            }
        }
    }
    if pi { Err(EINVAL) } else { Ok(task_count) }
}

/// Takes the entry `slot` of waiter `who` off the queue; whether it was there.
pub fn remove<W>(q: &mut Vec<Entry<W>>, slot: u32, who: &dyn Fn(&W) -> bool) -> Option<Entry<W>> {
    let i = q.iter().position(|e| e.slot == slot && who(&e.who))?;
    Some(q.remove(i))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn k(page: u64) -> Key {
        Key { kind: KeyKind::Private, object: 1, page, offset: 0 }
    }
    fn e(key: Key, bitset: u32, who: u32) -> Entry<u32> {
        Entry { key, bitset, slot: 0, who }
    }
    fn ids(q: &[Entry<u32>]) -> Vec<u32> {
        q.iter().map(|e| e.who).collect()
    }

    #[test]
    fn pi_waiters_make_plain_operations_einval_after_the_ones_before() {
        let mut q = vec![e(k(1), !0, 1), e(k(1), PI_BITSET, 2), e(k(1), !0, 3), e(k(2), PI_BITSET, 4)];
        let mut got = Vec::new();
        assert_eq!(wake(&mut q, &k(1), 5, !0, &mut |x| got.push(x.who)), Err(EINVAL));
        assert_eq!((got, ids(&q)), (vec![1], vec![2, 3, 4]));
        // The count used up before the PI waiter: no error (Linux breaks out of its loop first).
        let mut q = vec![e(k(1), !0, 1), e(k(1), PI_BITSET, 2)];
        assert_eq!(wake(&mut q, &k(1), 1, !0, &mut |_| {}), Ok(1));
        // Other keys' PI waiters do not matter.
        assert_eq!(wake(&mut q, &k(5), 1, !0, &mut |_| {}), Ok(0));
        let mut q1 = vec![e(k(1), !0, 1), e(k(1), !0, 2), e(k(1), PI_BITSET, 3)];
        let mut moved = 0;
        assert_eq!(requeue(&mut q1, None, &k(1), &k(2), 1, 5, &mut |_| {}, &mut |_| moved += 1), Err(EINVAL));
        assert_eq!((ids(&q1), moved), (vec![2, 3], 1));
        let mut q1 = vec![e(k(1), PI_BITSET, 1)];
        let mut q2 = Vec::with_capacity(4);
        assert_eq!(requeue(&mut q1, Some(&mut q2), &k(1), &k(2), 0, 1, &mut |_| {}, &mut |_| {}), Err(EINVAL));
        assert!(q2.is_empty());
    }

    #[test]
    fn wake_takes_matching_in_order_and_counts() {
        let mut q = vec![e(k(1), !0, 1), e(k(2), !0, 2), e(k(1), !0, 3), e(k(1), !0, 4)];
        let mut got = Vec::new();
        assert_eq!(wake(&mut q, &k(1), 2, !0, &mut |x| got.push(x.who)), Ok(2));
        assert_eq!(got, [1, 3]);
        assert_eq!(ids(&q), [2, 4]);
        // INT_MAX: all.
        assert_eq!(wake(&mut q, &k(1), i32::MAX, !0, &mut |_| {}), Ok(1));
        assert_eq!(ids(&q), [2]);
        assert_eq!(wake(&mut q, &k(9), 5, !0, &mut |_| {}), Ok(0));
    }

    #[test]
    fn wake_zero_or_negative_still_wakes_one() {
        // Linux: `if (++ret >= nr_wake) break;` after the first waiter.
        for n in [0, -1, i32::MIN] {
            let mut q = vec![e(k(1), !0, 1), e(k(1), !0, 2)];
            assert_eq!(wake(&mut q, &k(1), n, !0, &mut |_| {}), Ok(1), "nr_wake {n}");
            assert_eq!(ids(&q), [2]);
        }
    }

    #[test]
    fn wake_bitsets_must_overlap() {
        let mut q = vec![e(k(1), 0b01, 1), e(k(1), 0b10, 2), e(k(1), 0b11, 3)];
        let mut got = Vec::new();
        assert_eq!(wake(&mut q, &k(1), 10, 0b10, &mut |x| got.push(x.who)), Ok(2));
        assert_eq!(got, [2, 3]);
        assert_eq!(ids(&q), [1]);
    }

    #[test]
    fn keys_of_different_kinds_never_match() {
        let a = Key { kind: KeyKind::Private, object: 7, page: 0x1000, offset: 4 };
        let b = Key { kind: KeyKind::MmShared, ..a };
        assert_ne!(a, b);
        let mut q = vec![e(a, !0, 1)];
        assert_eq!(wake(&mut q, &b, 1, !0, &mut |_| {}), Ok(0));
        assert_ne!(a.hash(), b.hash());
    }

    #[test]
    fn requeue_wakes_then_moves() {
        let mut q1 = vec![e(k(1), !0, 1), e(k(1), !0, 2), e(k(3), !0, 9), e(k(1), !0, 3), e(k(1), !0, 4)];
        let mut q2 = vec![e(k(2), !0, 7)];
        q2.reserve(8);
        let (mut woken, mut moved) = (Vec::new(), Vec::new());
        let n = requeue(&mut q1, Some(&mut q2), &k(1), &k(2), 1, 2, &mut |x| woken.push(x.who), &mut |x| moved.push(x.who)).unwrap();
        assert_eq!(n, 3);
        assert_eq!(woken, [1]);
        assert_eq!(moved, [2, 3]);
        assert_eq!(ids(&q1), [9, 4]);
        assert_eq!(ids(&q2), [7, 2, 3]);
        assert!(q2.iter().all(|x| x.key == k(2)));
    }

    #[test]
    fn requeue_zero_zero_does_nothing() {
        // Unlike FUTEX_WAKE, nr_wake 0 wakes nobody here: the loop stops before the first waiter.
        let mut q1 = vec![e(k(1), !0, 1)];
        assert_eq!(requeue(&mut q1, None, &k(1), &k(2), 0, 0, &mut |_| {}, &mut |_| {}), Ok(0));
        assert_eq!(q1[0].key, k(1));
    }

    #[test]
    fn requeue_in_the_same_bucket_changes_keys_in_place() {
        let mut q1 = vec![e(k(1), !0, 1), e(k(1), !0, 2), e(k(1), !0, 3)];
        let n = requeue(&mut q1, None, &k(1), &k(2), 0, i32::MAX, &mut |_| {}, &mut |_| {}).unwrap();
        assert_eq!(n, 3);
        assert_eq!(ids(&q1), [1, 2, 3]);
        assert!(q1.iter().all(|x| x.key == k(2)));
    }

    #[test]
    fn requeue_counts_with_large_numbers() {
        let mut q1: Vec<Entry<u32>> = (0..5).map(|i| e(k(1), !0, i)).collect();
        let mut q2 = Vec::with_capacity(8);
        let mut woken = 0;
        let n = requeue(&mut q1, Some(&mut q2), &k(1), &k(2), i32::MAX, i32::MAX, &mut |_| woken += 1, &mut |_| {}).unwrap();
        assert_eq!((n, woken, q2.len()), (5, 5, 0));
    }

    #[test]
    fn requeue_stops_at_the_room_made() {
        let mut q1: Vec<Entry<u32>> = (0..6).map(|i| e(k(1), !0, i)).collect();
        let mut q2: Vec<Entry<u32>> = Vec::with_capacity(2);
        let n = requeue(&mut q1, Some(&mut q2), &k(1), &k(2), 1, i32::MAX, &mut |_| {}, &mut |_| {}).unwrap();
        assert_eq!(n, 1 + q2.capacity() as i64);
        assert_eq!(ids(&q2)[..2], [1, 2]);
        assert_eq!(q1.len() + q2.len() + 1, 6);
        assert_eq!(ids(&q1)[0], 3);
    }

    #[test]
    fn wake_of_many_keeps_the_others_in_order() {
        // 20000 waiters on two keys, interleaved: one pass takes every second one.
        let mut q: Vec<Entry<u32>> = (0..20000).map(|i| e(k(i as u64 % 2), !0, i)).collect();
        let mut got = 0u32;
        assert_eq!(wake(&mut q, &k(0), i32::MAX, !0, &mut |x| { assert_eq!(x.who, got * 2); got += 1 }), Ok(10000));
        assert_eq!(q.len(), 10000);
        assert!(q.iter().enumerate().all(|(i, x)| x.who == i as u32 * 2 + 1));
    }

    #[test]
    fn count_and_remove() {
        let mut q = vec![e(k(1), !0, 1), e(k(1), !0, 2), e(k(2), !0, 1)];
        assert_eq!(count(&q, &k(1), 10), 2);
        assert_eq!(count(&q, &k(1), 1), 1);
        assert!(remove(&mut q, 0, &|w| *w == 2).is_some());
        assert!(remove(&mut q, 0, &|w| *w == 2).is_none());
        assert_eq!(ids(&q), [1, 1]);
        assert!(remove(&mut q, 1, &|w| *w == 1).is_none());
    }
}
