//! PF-5: grace periods (a section holds back what was deferred while it was open, nothing else), the lockless
//! index (a model run, readers that never see a freed value while a writer replaces and removes), and the
//! allocator waiting for frames that only wait for a grace period instead of answering out-of-memory.
use super::*;
use crate::grace;
use crate::rcu_index::RcuIndex;
use crate::Buddy;
use alloc::sync::Arc;
use std::boxed::Box;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Mutex};
use std::time::Duration;

/// Counts its drop.
struct Probe(Arc<AtomicU64>);
impl Drop for Probe {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn an_open_section_holds_back_what_was_deferred_meanwhile() {
    let dropped = Arc::new(AtomicU64::new(0));
    let (enter_tx, enter_rx) = mpsc::channel();
    let (leave_tx, leave_rx) = mpsc::channel::<()>();
    let reader = std::thread::spawn(move || {
        let s = grace::enter().expect("a slot");
        enter_tx.send(()).unwrap();
        leave_rx.recv().unwrap();
        drop(s);
    });
    enter_rx.recv().unwrap();
    grace::defer_arc(Arc::new(Probe(dropped.clone())), 0);
    for _ in 0..50 {
        grace::drain();
    }
    assert_eq!(dropped.load(Ordering::SeqCst), 0, "a section that was open at the defer keeps it");
    assert!(grace::open_sections() >= 1);
    leave_tx.send(()).unwrap();
    reader.join().unwrap();
    assert!(grace::synchronize(), "no section left: the grace period ends");
    // `drain` gives up when another test holds the queue lock for a push; `expedite` waits for it.
    grace::expedite();
    // Other tests may defer meanwhile; this probe is past its grace period now.
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
}

#[test]
fn nested_sections_take_their_own_slots() {
    let a = grace::enter().unwrap();
    let b = grace::enter().unwrap();
    assert!(grace::open_sections() >= 2);
    drop(b);
    drop(a);
}

#[test]
fn pending_bytes_follow_the_queue() {
    let before = grace::stats();
    grace::defer_arc(Arc::new(7u64), 3 * PAGE);
    assert!(grace::stats().pending_bytes >= 3 * PAGE || grace::stats().drained_bytes >= before.drained_bytes + 3 * PAGE);
    // INT-W28: a parallel test may hold a section open longer than one expedite's bound; retry (bounded).
    let mut after = grace::stats();
    for _ in 0..500 {
        grace::expedite();
        after = grace::stats();
        if after.drained_bytes >= before.drained_bytes + 3 * PAGE {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    assert!(after.drained_bytes >= before.drained_bytes + 3 * PAGE, "{before:?} -> {after:?}");
}

#[test]
fn rcu_index_against_a_model() {
    let t: RcuIndex<u64> = RcuIndex::named("test.rcu");
    let mut m: BTreeMap<u64, u64> = BTreeMap::new();
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    let mut rnd = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    for step in 0..20_000u64 {
        let r = rnd();
        let i = match r % 7 {
            0 => rnd(),
            1 => rnd() >> 20,
            _ => rnd() % 5000,
        };
        let mut w = t.write();
        match (r >> 8) % 4 {
            0 | 1 => {
                let old = w.insert(i, Arc::new(step)).unwrap();
                assert_eq!(old.as_deref().copied(), m.insert(i, step));
            }
            2 => assert_eq!(w.remove(i).as_deref().copied(), m.remove(&i)),
            _ => {
                let want = m.range(i..).next().map(|(&k, &v)| (k, v));
                assert_eq!(w.next_from(i).map(|(k, v)| (k, *v)), want);
            }
        }
        assert_eq!(w.len(), m.len());
        assert_eq!(w.get(i).map(|v| *v), m.get(&i).copied());
        drop(w);
        let s = grace::enter().unwrap();
        assert_eq!(t.get(&s, i).copied(), m.get(&i).copied(), "lockless look-up");
    }
    let cut = 2500;
    let mut gone = Vec::new();
    t.write().drain_from(cut, &mut |k, _| gone.push(k));
    let want: Vec<u64> = m.range(cut..).map(|(&k, _)| k).collect();
    assert_eq!(gone, want);
    m.retain(|&k, _| k < cut);
    assert_eq!(t.len(), m.len());
    t.write().drain_from(0, &mut |_, _| {});
    assert!(t.is_empty());
    grace::expedite();
}

/// A value whose drop poisons it: a reader that sees the poison read freed memory's last state.
struct Canary {
    magic: AtomicU64,
    n: u64,
}
const ALIVE: u64 = 0xA11C_E5A1_7E5A_11CE;
impl Drop for Canary {
    fn drop(&mut self) {
        self.magic.store(0xDEAD, Ordering::SeqCst);
    }
}

#[test]
fn lockless_readers_never_see_a_value_that_was_freed() {
    let t: Arc<RcuIndex<Canary>> = Arc::new(RcuIndex::named("test.rcu.race"));
    const N: u64 = 300;
    {
        let mut w = t.write();
        for i in 0..N {
            w.insert(i, Arc::new(Canary { magic: AtomicU64::new(ALIVE), n: i })).unwrap();
        }
    }
    let stop = Arc::new(AtomicBool::new(false));
    let readers: Vec<_> = (0..6u64)
        .map(|r| {
            let (t, stop) = (t.clone(), stop.clone());
            std::thread::spawn(move || {
                let mut k = r + 1;
                let mut seen = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    let s = grace::enter().unwrap();
                    for _ in 0..16 {
                        k = k.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                        let i = (k >> 33) % N;
                        if let Some(c) = t.get(&s, i) {
                            assert_eq!(c.n % N, i, "the value of another index at {i}");
                            // Keep it a while (a copy): it must stay alive for the whole section.
                            for _ in 0..200 {
                                core::hint::spin_loop();
                            }
                            assert_eq!(c.magic.load(Ordering::SeqCst), ALIVE, "a freed value at {i}");
                            seen += 1;
                        }
                    }
                }
                seen
            })
        })
        .collect();
    let mut k = 7u64;
    let t0 = std::time::Instant::now();
    let mut round = 0u64;
    while t0.elapsed() < Duration::from_millis(300) {
        round += 1;
        k = k.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let i = (k >> 33) % N;
        let mut w = t.write();
        if round % 3 == 0 {
            drop(w.remove(i));
        } else {
            drop(w.insert(i, Arc::new(Canary { magic: AtomicU64::new(ALIVE), n: i + N * round })).unwrap());
        }
        drop(w);
        grace::drain();
    }
    stop.store(true, Ordering::Relaxed);
    let seen: u64 = readers.into_iter().map(|r| r.join().unwrap()).sum();
    assert!(seen > 0);
    grace::expedite();
}

/// The first insert into an empty index far out (index 1 << 20 needs a root of height 4) and the removal of the
/// last entry, again and again, while readers look up small indices (which walk a low root if one is ever
/// published) and a driver advances the epoch: a node is never freed while a reader can still reach it (nodes
/// poison their slots when dropped; the reader asserts on the poison). Regression test of the prepare order: an
/// empty root was deferred before it was unpublished.
#[test]
fn readers_never_walk_a_node_that_a_growing_root_freed() {
    let t: Arc<RcuIndex<Canary>> = Arc::new(RcuIndex::named("test.rcu.grow"));
    let stop = Arc::new(AtomicBool::new(false));
    let mut hs: Vec<_> = (0..4u64)
        .map(|r| {
            let (t, stop) = (t.clone(), stop.clone());
            std::thread::spawn(move || {
                let mut looks = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    let s = grace::enter().unwrap();
                    for i in [r, 5, 63, 1 << 20, (1 << 20) + 1] {
                        if let Some(c) = t.get(&s, i) {
                            assert_eq!(c.magic.load(Ordering::SeqCst), ALIVE, "a freed value at {i}");
                        }
                        looks += 1;
                    }
                }
                looks
            })
        })
        .collect();
    let driver = {
        let stop = stop.clone();
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                grace::try_advance();
                grace::drain();
            }
            0
        })
    };
    let t0 = std::time::Instant::now();
    let mut rounds = 0u64;
    while t0.elapsed() < Duration::from_millis(300) {
        rounds += 1;
        let mut w = t.write();
        let at = if rounds % 2 == 0 { 1 << 20 } else { 64 + rounds % 64 };
        assert!(w.insert(at, Arc::new(Canary { magic: AtomicU64::new(ALIVE), n: at })).unwrap().is_none());
        drop(w.remove(at));
        assert!(w.is_empty());
        drop(w);
    }
    stop.store(true, Ordering::Relaxed);
    hs.push(driver);
    let looks: u64 = hs.into_iter().map(|h| h.join().unwrap()).sum();
    assert!(looks > 0 && rounds > 100, "looks {looks}, rounds {rounds}");
    grace::expedite();
}

/// Frees its frames into `b` when dropped (a page-cache page whose frame waits for its grace period).
struct Frames {
    b: &'static Buddy,
    phys: Vec<u64>,
}
impl Drop for Frames {
    fn drop(&mut self) {
        for &p in &self.phys {
            self.b.free(p, 0, Kind::PageCache);
        }
    }
}

static ALLOC_TEST: Mutex<()> = Mutex::new(());

#[test]
fn an_allocation_waits_for_frames_that_only_wait_for_a_grace_period() {
    let _one = ALLOC_TEST.lock().unwrap();
    let f: &'static BuddyFrames = Box::leak(Box::new(staged(&[free(MIB, 3 * MIB)])));
    let b: &'static Buddy = f;
    let mut all = Vec::new();
    while let Some(p) = b.alloc(0, Zone::Dma32, Kind::PageCache) {
        all.push(p);
    }
    assert!(all.len() > 64);
    assert!(b.alloc(0, Zone::Dma32, Kind::User).is_none(), "full");
    // A reader is in a section while 32 frames are retired: they wait.
    let (in_tx, in_rx) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        let s = grace::enter().unwrap();
        in_tx.send(()).unwrap();
        std::thread::sleep(Duration::from_millis(3));
        drop(s);
    });
    in_rx.recv().unwrap();
    let back: Vec<u64> = all.drain(..32).collect();
    grace::defer_box(Box::new(Frames { b, phys: back }), 32 * PAGE);
    assert!(b.stats().pending_bytes >= 32 * PAGE, "pending shows in the allocator's numbers");
    // No reclaimer and nothing free: only the grace period gives frames back; the allocation waits for it.
    let got = b.alloc(0, Zone::Dma32, Kind::User);
    assert!(got.is_some(), "frames that only wait for a grace period are no out-of-memory");
    reader.join().unwrap();
    // A contiguous allocation below a limit waits for them too.
    let blocks: Vec<u64> = all.drain(..16).collect();
    grace::defer_box(Box::new(Frames { b, phys: blocks }), 16 * PAGE);
    assert!(b.alloc_below(2, 3 * MIB, Kind::Dma).is_some() || b.alloc_below(0, 3 * MIB, Kind::Dma).is_some());
    // The heap never waits (its lock may be held by the caller).
    grace::defer_box(Box::new(Frames { b, phys: all.drain(..8).collect() }), 8 * PAGE);
    let _heap = b.alloc(0, Zone::Dma32, Kind::Heap);
    grace::expedite();
}
