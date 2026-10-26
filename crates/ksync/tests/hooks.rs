//! WaitQueue, Mutex and Completion with a real blocking "scheduler": std threads park on a condvar through
//! `SchedHooks` exactly as kernel threads do through sched (prepare_to_block / block / wake, spurious returns
//! allowed). Checks no lost wake-ups under stress, timeouts, wake_one not being swallowed by a waiter that
//! times out, and that waiting with a spin lock held is reported.

use ksync::{Completion, Mutex, SchedHooks, SpinLock, ThreadRef, WaitQueue, lockdep, stats};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, OnceLock};
use std::time::{Duration, Instant};

struct Slot {
    woken: std::sync::Mutex<bool>,
    cv: Condvar,
}

struct StdSched {
    start: Instant,
    slots: std::sync::Mutex<HashMap<u64, Arc<Slot>>>,
    blocks: AtomicU64,
    /// SC-7b: the scheduler's view of priority passing, as `sched::pi` keeps it: per mutex address the recorded
    /// waiters and the owner they lend to (a waiter's read of the owner word, or the acquirer's report).
    pi: std::sync::Mutex<HashMap<usize, (usize, Option<u64>)>>,
    pi_waits: AtomicU64,
    pi_ends: AtomicU64,
}

static NEXT: AtomicU64 = AtomicU64::new(1);
thread_local! {
    static ME: u64 = NEXT.fetch_add(1, Ordering::Relaxed);
}

impl StdSched {
    fn slot(&self, id: u64) -> Arc<Slot> {
        self.slots
            .lock()
            .unwrap()
            .entry(id)
            .or_insert_with(|| Arc::new(Slot { woken: std::sync::Mutex::new(false), cv: Condvar::new() }))
            .clone()
    }
}

impl SchedHooks for StdSched {
    fn current(&self) -> ThreadRef {
        ThreadRef(ME.with(|m| *m))
    }
    fn prepare_to_block(&self) {
        let s = self.slot(self.current().0);
        *s.woken.lock().unwrap() = false;
    }
    fn block(&self, deadline_ns: Option<u64>) {
        self.blocks.fetch_add(1, Ordering::Relaxed);
        let s = self.slot(self.current().0);
        let mut w = s.woken.lock().unwrap();
        while !*w {
            match deadline_ns {
                Some(d) => {
                    let now = self.now_ns();
                    if now >= d {
                        return;
                    }
                    w = s.cv.wait_timeout(w, Duration::from_nanos(d - now)).unwrap().0;
                }
                None => w = s.cv.wait(w).unwrap(),
            }
        }
    }
    fn wake(&self, t: ThreadRef) {
        let s = self.slot(t.0);
        *s.woken.lock().unwrap() = true;
        s.cv.notify_all();
    }
    fn yield_now(&self) {
        std::thread::yield_now();
    }
    fn now_ns(&self) -> u64 {
        self.start.elapsed().as_nanos() as u64
    }
    fn pi_wait(&self, lock: usize, owner: &AtomicU64) -> bool {
        let mut m = self.pi.lock().unwrap();
        let e = m.entry(lock).or_insert((0, None));
        let o = owner.load(Ordering::SeqCst);
        if e.0 == 0 || e.1.is_none() {
            e.1 = (o != u64::MAX).then_some(o);
        }
        e.0 += 1;
        self.pi_waits.fetch_add(1, Ordering::Relaxed);
        true
    }
    fn pi_acquired(&self, lock: usize, waited: bool) {
        let mut m = self.pi.lock().unwrap();
        let e = m.entry(lock).or_insert((0, None));
        if waited {
            e.0 -= 1;
            self.pi_ends.fetch_add(1, Ordering::Relaxed);
        }
        e.1 = Some(ksync::thread_id());
    }
    fn pi_gave_up(&self, lock: usize) {
        let mut m = self.pi.lock().unwrap();
        if let Some(e) = m.get_mut(&lock) {
            e.0 -= 1;
        }
        self.pi_ends.fetch_add(1, Ordering::Relaxed);
    }
    fn pi_released(&self, lock: usize) {
        let mut m = self.pi.lock().unwrap();
        if let Some(e) = m.get_mut(&lock)
            && e.1 == Some(ksync::thread_id())
        {
            e.1 = None;
        }
    }
}

impl StdSched {
    /// Whether the scheduler would lend this lock's waiters to the calling holder (or no waiter is recorded).
    fn lends_to_me(&self, lock: usize) -> bool {
        let m = self.pi.lock().unwrap();
        m.get(&lock).is_none_or(|e| e.0 == 0 || e.1 == Some(ksync::thread_id()))
    }
}

fn install() -> &'static StdSched {
    static S: OnceLock<&'static StdSched> = OnceLock::new();
    S.get_or_init(|| {
        let s: &'static StdSched =
            Box::leak(Box::new(StdSched {
                start: Instant::now(),
                slots: Default::default(),
                blocks: AtomicU64::new(0),
                pi: Default::default(),
                pi_waits: AtomicU64::new(0),
                pi_ends: AtomicU64::new(0),
            }));
        ksync::set_sched_hooks(s);
        s
    })
}

#[test]
fn ping_pong_never_loses_a_wake() {
    install();
    let wq = Arc::new(WaitQueue::new());
    let turn = Arc::new(AtomicU64::new(0));
    const ROUNDS: u64 = 20_000;
    let (wq2, t2) = (wq.clone(), turn.clone());
    let other = std::thread::spawn(move || {
        for i in 0..ROUNDS {
            assert!(wq2.wait_until_timeout(|| t2.load(Ordering::Acquire) == 2 * i + 1, 10_000_000_000), "lost wake at {i}");
            t2.store(2 * i + 2, Ordering::Release);
            wq2.wake_all();
        }
    });
    for i in 0..ROUNDS {
        turn.store(2 * i + 1, Ordering::Release);
        wq.wake_all();
        assert!(wq.wait_until_timeout(|| turn.load(Ordering::Acquire) == 2 * i + 2, 10_000_000_000), "lost wake at {i}");
    }
    other.join().unwrap();
}

/// SC-7b: whatever the interleaving, every holder that has recorded waiters is the owner they lend to (the owner
/// word and the waiter count are SeqCst on both sides), and every recorded wait ends (acquired or given up).
#[test]
fn priority_passing_always_knows_the_holder() {
    let s = install();
    let m = Arc::new(Mutex::named("t.hooks.pi", 0u64));
    let addr = &*m as *const Mutex<u64> as usize;
    let wrong = Arc::new(AtomicU64::new(0));
    let hs: Vec<_> = (0..8)
        .map(|i| {
            let (m, wrong) = (m.clone(), wrong.clone());
            std::thread::spawn(move || {
                for k in 0..2_000 {
                    let g = if (i + k) % 5 == 0 {
                        match m.lock_timeout(20_000) {
                            Some(g) => g,
                            None => continue,
                        }
                    } else {
                        m.lock()
                    };
                    for _ in 0..30 {
                        std::hint::spin_loop();
                    }
                    if !install().lends_to_me(addr) {
                        wrong.fetch_add(1, Ordering::Relaxed);
                    }
                    drop(g);
                }
            })
        })
        .collect();
    for h in hs {
        h.join().unwrap();
    }
    assert_eq!(wrong.load(Ordering::Relaxed), 0, "a holder was not the recorded owner while waiters lent to it");
    let e = *s.pi.lock().unwrap().get(&addr).unwrap_or(&(0, None));
    assert_eq!(e.0, 0, "recorded waiters left");
    assert!(s.pi_waits.load(Ordering::Relaxed) > 0, "nobody slept, the test proved nothing");
}

#[test]
fn sleeping_mutex_under_contention() {
    let s = install();
    let before = s.blocks.load(Ordering::Relaxed);
    let m = Arc::new(Mutex::named("t.hooks.mutex", 0u64));
    let hs: Vec<_> = (0..8)
        .map(|_| {
            let m = m.clone();
            std::thread::spawn(move || {
                for _ in 0..3_000 {
                    let mut g = m.lock();
                    *g += 1;
                    // Hold it a little so others really sleep.
                    for _ in 0..50 {
                        std::hint::spin_loop();
                    }
                    drop(g);
                }
            })
        })
        .collect();
    for h in hs {
        h.join().unwrap();
    }
    assert_eq!(*m.lock(), 24_000);
    assert_eq!(m.waiters(), 0);
    let st = stats::class("t.hooks.mutex").unwrap();
    assert_eq!(st.acquired, 24_001);
    assert!(st.contended > 0);
    // Some of the contention ended in a real sleep (not only in the spin phase).
    assert!(s.blocks.load(Ordering::Relaxed) > before);
}

#[test]
fn mutex_lock_timeout() {
    install();
    let m = Arc::new(Mutex::named("t.hooks.timeout", ()));
    let g = m.lock();
    let m2 = m.clone();
    let t = std::thread::spawn(move || {
        let t0 = Instant::now();
        let r = m2.lock_timeout(30_000_000).is_none();
        (r, t0.elapsed())
    });
    let (timed_out, took) = t.join().unwrap();
    assert!(timed_out);
    assert!(took >= Duration::from_millis(30), "{took:?}");
    drop(g);
    assert_eq!(stats::class("t.hooks.timeout").unwrap().timeouts, 1);
    assert!(m.lock_timeout(1_000_000_000).is_some());
}

#[test]
fn completion_wakes_all_and_times_out() {
    install();
    let c = Arc::new(Completion::new());
    assert!(!c.wait_timeout(20_000_000));
    let done = Arc::new(AtomicUsize::new(0));
    let hs: Vec<_> = (0..6)
        .map(|_| {
            let (c, d) = (c.clone(), done.clone());
            std::thread::spawn(move || {
                c.wait();
                d.fetch_add(1, Ordering::SeqCst);
            })
        })
        .collect();
    std::thread::sleep(Duration::from_millis(20));
    c.complete();
    for h in hs {
        h.join().unwrap();
    }
    assert_eq!(done.load(Ordering::SeqCst), 6);
}

/// A counting semaphore on WaitQueue + wake_one with waiters that give up after short deadlines and retry,
/// next to long-deadline waiters: every token is taken, none is stranded with sleepers around.
#[test]
fn semaphore_with_timeouts_strands_no_token() {
    install();
    let wq = Arc::new(WaitQueue::new());
    let tokens = Arc::new(AtomicU64::new(0));
    let taken = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    const N: u64 = 20_000;
    let take = |t: &AtomicU64| {
        let mut v = t.load(Ordering::Acquire);
        while v > 0 {
            match t.compare_exchange(v, v - 1, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return true,
                Err(x) => v = x,
            }
        }
        false
    };
    let hs: Vec<_> = (0..6)
        .map(|i| {
            let (wq, tokens, taken, stop) = (wq.clone(), tokens.clone(), taken.clone(), stop.clone());
            std::thread::spawn(move || {
                while !stop.load(Ordering::Acquire) {
                    // Odd threads use very short deadlines.
                    let to = if i % 2 == 1 { 50_000 } else { 5_000_000_000 };
                    if wq.wait_until_timeout(|| take(&tokens) || stop.load(Ordering::Acquire), to) && !stop.load(Ordering::Acquire) {
                        taken.fetch_add(1, Ordering::AcqRel);
                    }
                }
            })
        })
        .collect();
    let t0 = Instant::now();
    for _ in 0..N {
        tokens.fetch_add(1, Ordering::AcqRel);
        wq.wake_one();
    }
    while taken.load(Ordering::Acquire) < N {
        assert!(t0.elapsed() < Duration::from_secs(20), "tokens left: {}", tokens.load(Ordering::Acquire));
        std::thread::sleep(Duration::from_millis(1));
    }
    stop.store(true, Ordering::Release);
    wq.wake_all();
    for h in hs {
        h.join().unwrap();
    }
    assert_eq!(taken.load(Ordering::Acquire), N);
}

#[test]
fn waiting_with_a_spin_lock_held_is_reported() {
    install();
    if !lockdep::enabled() {
        return;
    }
    let l = SpinLock::named("t.hooks.spin_wait", ());
    let wq = WaitQueue::new();
    let (_, n) = lockdep::expect_violations(|| {
        let _g = l.lock();
        assert!(!wq.wait_until_timeout(|| false, 1_000_000));
    });
    assert!(n >= 1);
    assert!(lockdep::list().iter().any(|v| v.kind == ksync::ViolationKind::SleepUnderSpin && v.held == "t.hooks.spin_wait"));
}
