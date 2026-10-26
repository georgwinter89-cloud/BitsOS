//! The book's rules on the host: lending, chains, giving up, hand-over, order of waiters, deadlock detection.

use super::*;
use alloc::collections::BTreeMap;
extern crate std;

#[derive(Default)]
struct Map(BTreeMap<u64, u8>);

impl Boosts for Map {
    fn get(&self, id: u64) -> u8 {
        *self.0.get(&id).unwrap_or(&NO_BOOST)
    }
    fn set(&mut self, id: u64, b: u8) {
        self.0.insert(id, b);
    }
}

const RT: u8 = 0;
const INTER: u8 = 1;
const NORMAL: u8 = 2;
const BG: u8 = 3;

fn o(id: u64) -> Owner {
    Owner { id, tid: id as u32 }
}
fn m(a: usize) -> LockId {
    LockId::Mutex(a)
}

#[test]
fn a_waiter_lends_its_class_while_it_waits() {
    let (mut b, mut x) = (Book::new(), Map::default());
    b.wait(&mut x, m(1), o(2), INTER, Some(o(1)), false).unwrap();
    assert_eq!(x.get(1), INTER);
    assert_eq!(b.leave(&mut x, 2), Some(m(1)));
    assert_eq!(x.get(1), NO_BOOST);
    assert!(b.is_empty() && b.owner(m(1)).is_none());
}

#[test]
fn kernel_rt_lends_interactive_and_the_most_urgent_wins() {
    let (mut b, mut x) = (Book::new(), Map::default());
    b.wait(&mut x, m(1), o(2), BG, Some(o(1)), false).unwrap();
    assert_eq!(x.get(1), BG);
    b.wait(&mut x, m(1), o(3), RT, None, false).unwrap();
    assert_eq!(x.get(1), INTER);
    b.leave(&mut x, 3);
    assert_eq!(x.get(1), BG);
}

#[test]
fn boosts_cover_every_lock_held() {
    let (mut b, mut x) = (Book::new(), Map::default());
    b.wait(&mut x, m(1), o(2), NORMAL, Some(o(1)), false).unwrap();
    b.wait(&mut x, m(5), o(3), INTER, Some(o(1)), false).unwrap();
    assert_eq!(x.get(1), INTER);
    b.leave(&mut x, 3);
    assert_eq!(x.get(1), NORMAL);
}

#[test]
fn chains_pass_on_and_shrink_back() {
    // 1 holds A; 2 holds B and waits for A; 3 holds C and waits for B; 4 (Interactive) waits for C.
    let (mut b, mut x) = (Book::new(), Map::default());
    b.wait(&mut x, m(10), o(2), BG, Some(o(1)), false).unwrap();
    b.wait(&mut x, m(20), o(3), BG, Some(o(2)), false).unwrap();
    assert_eq!((x.get(1), x.get(2)), (BG, BG));
    b.wait(&mut x, m(30), o(4), INTER, Some(o(3)), false).unwrap();
    assert_eq!((x.get(1), x.get(2), x.get(3)), (INTER, INTER, INTER));
    b.leave(&mut x, 4);
    assert_eq!((x.get(1), x.get(2), x.get(3)), (BG, BG, NO_BOOST));
}

#[test]
fn hand_over_moves_the_remaining_donations_to_the_new_owner() {
    let (mut b, mut x) = (Book::new(), Map::default());
    b.wait(&mut x, m(1), o(2), NORMAL, Some(o(1)), false).unwrap();
    b.wait(&mut x, m(1), o(3), INTER, None, false).unwrap();
    assert_eq!(x.get(1), INTER);
    // 1 releases, 3 (top waiter) takes it: 2 now lends to 3.
    assert_eq!(b.top(m(1)), Some(o(3)));
    b.released(&mut x, m(1), 1);
    assert_eq!(x.get(1), NO_BOOST);
    b.leave(&mut x, 3);
    b.set_owner(&mut x, m(1), Some(o(3)));
    assert_eq!((x.get(3), x.get(1)), (NORMAL, NO_BOOST));
    b.leave(&mut x, 2);
    assert_eq!(x.get(3), NO_BOOST);
    assert!(b.owner(m(1)).is_none());
}

#[test]
fn a_stale_release_does_not_clear_a_new_owner() {
    let (mut b, mut x) = (Book::new(), Map::default());
    b.wait(&mut x, m(1), o(2), INTER, Some(o(1)), false).unwrap();
    b.set_owner(&mut x, m(1), Some(o(5)));
    b.released(&mut x, m(1), 1);
    assert_eq!(b.owner(m(1)), Some(Some(o(5))));
    assert_eq!((x.get(5), x.get(1)), (INTER, NO_BOOST));
}

#[test]
fn top_waiter_is_most_urgent_then_earliest() {
    let (mut b, mut x) = (Book::new(), Map::default());
    for (id, c) in [(2, NORMAL), (3, BG), (4, INTER), (5, INTER), (6, RT)] {
        b.wait(&mut x, m(1), o(id), c, Some(o(1)), false).unwrap();
    }
    assert_eq!(b.top(m(1)), Some(o(4)), "KernelRt lends Interactive, so the earlier Interactive one is first");
    b.leave(&mut x, 4);
    assert_eq!(b.top(m(1)), Some(o(5)));
    b.leave(&mut x, 5);
    b.leave(&mut x, 6);
    assert_eq!(b.top(m(1)), Some(o(2)));
    assert_eq!(b.waiter_count(m(1)), 2);
}

#[test]
fn a_boosted_waiter_ranks_by_its_boost() {
    // 2 (Background) holds B, waited for by 3 (Interactive); 2 then waits for A along with 4 (Normal).
    let (mut b, mut x) = (Book::new(), Map::default());
    b.wait(&mut x, m(20), o(3), INTER, Some(o(2)), false).unwrap();
    b.wait(&mut x, m(10), o(4), NORMAL, Some(o(1)), false).unwrap();
    b.wait(&mut x, m(10), o(2), BG, Some(o(1)), false).unwrap();
    assert_eq!(b.top(m(10)), Some(o(2)));
    assert_eq!(x.get(1), INTER);
}

#[test]
fn deadlocks_are_seen() {
    let (mut b, mut x) = (Book::new(), Map::default());
    b.wait(&mut x, m(10), o(2), NORMAL, Some(o(1)), true).unwrap();
    // 1 waiting for a lock held by 2 closes the cycle.
    assert_eq!(b.wait(&mut x, m(20), o(1), NORMAL, Some(o(2)), true), Err(WaitError::Deadlock));
    assert!(b.would_deadlock(7, Some(o(7))));
    assert_eq!(b.waiting(1), None);
    assert_eq!(b.len(), 1);
}

#[test]
fn cycles_without_detection_end_at_the_depth_bound() {
    let (mut b, mut x) = (Book::new(), Map::default());
    b.wait(&mut x, m(10), o(2), BG, Some(o(1)), false).unwrap();
    b.wait(&mut x, m(20), o(1), BG, Some(o(2)), false).unwrap();
    b.wait(&mut x, m(10), o(3), INTER, None, false).unwrap();
    assert_eq!((x.get(1), x.get(2)), (INTER, INTER));
    b.leave(&mut x, 3);
    // Both keep lending each other Interactive (a real deadlock); the walk ended without looping forever.
    assert!(b.len() == 2);
}

#[test]
fn own_class_changes_follow_the_chain() {
    let (mut b, mut x) = (Book::new(), Map::default());
    b.wait(&mut x, m(10), o(2), BG, Some(o(1)), false).unwrap();
    b.adjust(&mut x, 2, INTER);
    assert_eq!(x.get(1), INTER);
    b.adjust(&mut x, 2, NORMAL);
    assert_eq!(x.get(1), NORMAL);
}

#[test]
fn forget_ends_boosts_and_ownership() {
    let (mut b, mut x) = (Book::new(), Map::default());
    b.wait(&mut x, m(10), o(2), INTER, Some(o(1)), false).unwrap();
    b.forget(&mut x, 1);
    assert_eq!(x.get(1), NO_BOOST);
    assert_eq!(b.owner(m(10)), Some(None));
    assert_eq!(b.owned_by(1).count(), 0);
}

impl Book {
    /// Every derived value equals its definition (boosts, donations, queue order, `tops`).
    fn check(&self, x: &Map) {
        for w in &self.waiters {
            assert_eq!(w.don, w.own.min(x.get(w.who.id)).max(TOP), "donation of {}", w.who.id);
            let r = self.rec(w.lock).expect("record of a waited lock");
            assert!(r.q.contains(&(w.don, w.seq, w.who.id)));
        }
        let mut tops = Vec::new();
        for r in &self.recs {
            assert!(!r.q.is_empty() && r.q.windows(2).all(|p| p[0] < p[1]));
            tops.extend(Book::top_entry(r));
        }
        tops.sort();
        assert_eq!(tops, self.tops);
        for (&id, &b) in &x.0 {
            let want = self.waiters.iter().filter(|w| self.owner_id(w.lock) == Some(id)).map(|w| w.don).min().unwrap_or(NO_BOOST);
            assert_eq!(b, want, "boost of {id}");
        }
        for e in &self.tops {
            assert_eq!(x.get(e.0), self.boost_of(e.0), "boost of owner {}", e.0);
        }
    }
}

#[test]
fn random_operations_keep_every_derived_value_right() {
    let mut s = 0x9e37_79b9_7f4a_7c15u64;
    let mut rnd = |n: u64| {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s % n
    };
    let (mut b, mut x) = (Book::new(), Map::default());
    for _ in 0..20_000 {
        let id = 1 + rnd(24);
        match rnd(6) {
            0 | 1 => {
                let owner = 1 + rnd(24);
                let _ = b.wait(&mut x, m(1 + rnd(8) as usize), o(id), rnd(4) as u8, Some(o(owner)), true);
            }
            2 => {
                b.leave(&mut x, id);
            }
            3 => {
                let l = m(1 + rnd(8) as usize);
                if let Some(n) = b.top(l) {
                    b.leave(&mut x, n.id);
                    b.set_owner(&mut x, l, Some(n));
                }
            }
            4 => b.adjust(&mut x, id, rnd(4) as u8),
            _ => b.forget(&mut x, id),
        }
        b.check(&x);
    }
}

/// Counts the boost reads: the work of one operation under the book's lock.
#[derive(Default)]
struct Counting(Map, core::cell::Cell<u64>);

impl Boosts for Counting {
    fn get(&self, id: u64) -> u8 {
        self.1.set(self.1.get() + 1);
        self.0.get(id)
    }
    fn set(&mut self, id: u64, b: u8) {
        self.0.set(id, b);
    }
}

#[test]
fn work_per_operation_does_not_grow_with_the_waiters() {
    // 4000 threads each own a lock with a waiter, and one owner holds 4000 more locks with a waiter each.
    let (mut b, mut x) = (Book::new(), Counting::default());
    const N: u64 = 4000;
    let t0 = std::time::Instant::now();
    for i in 0..N {
        b.wait(&mut x, m(i as usize), o(100_000 + i), NORMAL, Some(o(1 + i)), true).unwrap();
        b.wait(&mut x, m((N + i) as usize), o(200_000 + i), BG, Some(o(50_000)), true).unwrap();
    }
    let before = x.1.get();
    b.wait(&mut x, m(N as usize * 3), o(300_000), INTER, Some(o(50_000)), true).unwrap();
    b.leave(&mut x, 300_000);
    b.adjust(&mut x, 100_007, INTER);
    b.set_owner(&mut x, m(5), Some(o(9_999_999)));
    let per = x.1.get() - before;
    assert!(per <= 16, "{per} boost reads for four operations among {} waiters", b.len());
    assert_eq!(x.0.get(50_000), BG);
    std::eprintln!("book: {} waiters set up in {:?}", b.len(), t0.elapsed());
}
