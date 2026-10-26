//! SC-6 ktests (kernel threads; the syscalls and real programs are tested in `syscall::sched_ktests`):
//! affinity moves the caller, a running and a sleeping thread and holds them there; nice and SCHED_IDLE change
//! the CPU share on one CPU in weight order; CPU time splits at the user-mode marks, is read with the running
//! slice and never goes back; switch counts; `copy_attrs` with reset-on-fork. Timing is never judged under
//! QEMU: the share checks are orderings with generous margins, the numbers go to `sched ktest.*` events.

use crate::attr::{self, AttrError, Policy};
use crate::{Class, JoinHandle, SpawnOpts, cond_resched, current_cpu, current_id, spawn_with};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use ktest::{kassert, ktest};

type R = ktest::TestResult;

ktest!("sc.attr.affinity_self", Safe, affinity_self);
ktest!("sc.attr.affinity_other", Safe, affinity_other);
ktest!("sc.attr.nice_share", Safe, nice_share);
ktest!("sc.attr.copy_attrs", Safe, copy_attrs);
ktest!("sc.cputime.split", Safe, split);
ktest!("sc.cputime.monotonic_read", Safe, monotonic_read);
ktest!("sc.cputime.switch_counts", Safe, switch_counts);

const MS: u64 = 1_000_000;
const SEC: u64 = 1_000_000_000;

fn online(c: usize) -> bool {
    crate::cpus::of(c).is_some_and(|s| s.online.load(Ordering::Acquire))
}

/// Two different online CPUs (the last and the first other one) with the caller pinned to the second, so it
/// is never stuck behind the test's threads. None with one CPU.
fn two_cpus() -> Option<(usize, usize, crate::PinGuard)> {
    if crate::wait_cpus_online(5 * SEC) < 2 {
        return None;
    }
    let t = (0..crate::cpus::count()).rev().find(|&c| online(c))?;
    let home = (0..crate::cpus::count()).find(|&c| c != t && online(c))?;
    crate::pin_for_now(home).map(|g| (t, home, g))
}

fn one(cpu: usize) -> Vec<u64> {
    let mut m = vec![0u64; attr::mask_words(hal::percpu::possible_cpus())];
    m[cpu / 64] |= 1 << (cpu % 64);
    m
}

fn all() -> Vec<u64> {
    vec![u64::MAX; attr::mask_words(hal::percpu::possible_cpus())]
}

fn spawn(name: &str, f: impl FnOnce() + Send + 'static) -> Result<JoinHandle, String> {
    spawn_with(name, Class::Normal, SpawnOpts::default(), f).map_err(|e| format!("spawn {name}: {e:?}"))
}

fn join(h: &JoinHandle, what: &str) -> R {
    kassert!(h.join_timeout(30 * SEC), "{what}: thread {} did not finish within 30 s", h.id());
    Ok(())
}

/// Busy for `ns` of wall time, with preemption points.
fn busy_for(ns: u64) {
    let end = time::now_ns() + ns;
    let mut x = 0u64;
    while time::now_ns() < end {
        for i in 0..1000u64 {
            x = core::hint::black_box(x.wrapping_mul(31).wrapping_add(i));
        }
        cond_resched();
    }
    core::hint::black_box(x);
}

fn busy_until(stop: &AtomicBool) {
    while !stop.load(Ordering::Acquire) {
        busy_for(MS);
    }
}

/// A thread narrows its own mask: it moves at once and stays; a mask without an online CPU and an unknown
/// thread are refused; the read-back is the mask limited to the online CPUs.
fn affinity_self() -> R {
    let Some((t, home, _pin)) = two_cpus() else { return ktest::skip("needs two CPUs") };
    let errs = Arc::new(ksync::SpinLock::new(Vec::<String>::new()));
    let e = errs.clone();
    let h = spawn("sc-aff-self", move || {
        let me = current_id();
        let fail = |s: String| e.lock().push(s);
        if attr::set_affinity(me, &one(t)).is_err() {
            fail(String::from("set to one CPU refused"));
        }
        if current_cpu() != t {
            fail(format!("still on CPU {} after narrowing to {t}", current_cpu()));
        }
        for _ in 0..20 {
            busy_for(MS);
            crate::sleep_ms(1);
            if current_cpu() != t {
                fail(format!("ran on CPU {} with the mask {{{t}}}", current_cpu()));
                break;
            }
        }
        if attr::set_affinity(me, &one(home)).is_err() || current_cpu() != home {
            fail(format!("did not move to CPU {home} (on {})", current_cpu()));
        }
        let _ = attr::set_affinity(me, &all());
    })?;
    join(&h, "affinity_self")?;
    let errs: Vec<String> = core::mem::take(&mut *errs.lock());
    kassert!(errs.is_empty(), "{}", errs.join("; "));
    // Refusals and the read-back on a live thread.
    let stop = Arc::new(AtomicBool::new(false));
    let s = stop.clone();
    let w = spawn("sc-aff-idle", move || {
        while !s.load(Ordering::Acquire) {
            crate::sleep_ms(2);
        }
    })?;
    let possible = hal::percpu::possible_cpus();
    let r = (|| -> R {
        kassert!(attr::set_affinity(w.id(), &[0]) == Err(AttrError::Invalid), "an empty mask was taken");
        kassert!(attr::set_affinity(w.id(), &[]) == Err(AttrError::Invalid), "no words were taken");
        if possible < 64 {
            kassert!(attr::set_affinity(w.id(), &[1 << 63]) == Err(AttrError::Invalid), "a mask of only a CPU beyond the possible ones was taken");
        }
        kassert!(attr::set_affinity(u64::MAX - 1, &one(t)) == Err(AttrError::NoThread), "an unknown thread was found");
        let mut m = one(t);
        m[0] |= 1 << home;
        // Bits beyond the CPUs ride along and are dropped.
        let mut wide = m.clone();
        wide.push(u64::MAX);
        kassert!(attr::set_affinity(w.id(), &wide).is_ok(), "two CPUs refused");
        kassert!(attr::affinity(w.id()) == Some(m.clone()), "read back {:?}, want {m:?}", attr::affinity(w.id()));
        kassert!(attr::set_affinity(w.id(), &all()).is_ok() && attr::affinity(w.id()).is_some_and(|a| (0..possible).all(|c| attr::mask_has(&a, c) == online(c))), "all CPUs did not read back as the online ones");
        Ok(())
    })();
    stop.store(true, Ordering::Release);
    join(&w, "sleeper")?;
    r
}

/// Another thread's mask: a running one leaves its CPU at once and stays on the allowed one; a sleeping one
/// wakes on the allowed one; widening lets it go again.
fn affinity_other() -> R {
    let Some((t, home, _pin)) = two_cpus() else { return ktest::skip("needs two CPUs") };
    let stop = Arc::new(AtomicBool::new(false));
    let s = stop.clone();
    let busy = spawn("sc-aff-busy", move || busy_until(&s))?;
    let s = stop.clone();
    let woke_on = Arc::new(AtomicUsize::new(usize::MAX));
    let w2 = woke_on.clone();
    let sleeper = spawn("sc-aff-sleep", move || {
        crate::sleep_ms(200);
        w2.store(current_cpu(), Ordering::Release);
        while !s.load(Ordering::Acquire) {
            crate::sleep_ms(2);
        }
    })?;
    let r = (|| -> R {
        crate::sleep_ms(20);
        // The busy thread to CPU t, the sleeper to CPU home (where the test thread is pinned, so it shares).
        attr::set_affinity(busy.id(), &one(t)).map_err(|e| format!("set busy: {e:?}"))?;
        attr::set_affinity(sleeper.id(), &one(home)).map_err(|e| format!("set sleeper: {e:?}"))?;
        let end = time::now_ns() + 2 * SEC;
        while busy.thread().last_cpu() != t {
            kassert!(time::now_ns() < end, "the busy thread is still on CPU {} 2 s after its mask became {{{t}}}", busy.thread().last_cpu());
            crate::sleep_ms(1);
        }
        for _ in 0..100 {
            let c = busy.thread().last_cpu();
            kassert!(c == t, "the busy thread ran on CPU {c} with the mask {{{t}}}");
            crate::sleep_ms(2);
        }
        let end = time::now_ns() + 5 * SEC;
        while woke_on.load(Ordering::Acquire) == usize::MAX {
            kassert!(time::now_ns() < end, "the sleeper did not wake within 5 s");
            crate::sleep_ms(5);
        }
        let w = woke_on.load(Ordering::Acquire);
        kassert!(w == home, "the sleeper woke on CPU {w}, its mask is {{{home}}}");
        attr::set_affinity(busy.id(), &all()).map_err(|e| format!("widen: {e:?}"))?;
        kassert!(busy.thread().affinity_mask().iter().any(|&w| w != 0) && busy.thread().may_run_on(home), "widened mask not in effect");
        Ok(())
    })();
    stop.store(true, Ordering::Release);
    join(&busy, "busy")?;
    join(&sleeper, "sleeper")?;
    r
}

/// Two busy threads share one CPU: nice 0 gets clearly more than nice 5 (weights 1024 : 335); a SCHED_IDLE
/// thread gets far less than a SCHED_OTHER one (3 : 1024).
fn nice_share() -> R {
    let Some((t, _home, _pin)) = two_cpus() else { return ktest::skip("needs two CPUs") };
    let share = |a_set: &dyn Fn(u64) -> Result<(), AttrError>, b_set: &dyn Fn(u64) -> Result<(), AttrError>| -> Result<(u64, u64), String> {
        let stop = Arc::new(AtomicBool::new(false));
        let mut hs = Vec::new();
        for name in ["sc-share-a", "sc-share-b"] {
            let s = stop.clone();
            let h = spawn(name, move || busy_until(&s))?;
            attr::set_affinity(h.id(), &one(t)).map_err(|e| format!("affinity: {e:?}"))?;
            hs.push(h);
        }
        let r = (|| {
            a_set(hs[0].id()).map_err(|e| format!("set a: {e:?}"))?;
            b_set(hs[1].id()).map_err(|e| format!("set b: {e:?}"))?;
            crate::sleep_ms(100);
            let r0: Vec<u64> = hs.iter().map(|h| h.thread().runtime_ns()).collect();
            crate::sleep_ms(1_000);
            let r1: Vec<u64> = hs.iter().map(|h| h.thread().runtime_ns()).collect();
            Ok::<(u64, u64), String>((r1[0] - r0[0], r1[1] - r0[1]))
        })();
        stop.store(true, Ordering::Release);
        for h in &hs {
            join(h, "share")?;
        }
        r
    };
    let (n0, n5) = share(&|id| attr::set_nice(id, 0), &|id| attr::set_nice(id, 5))?;
    let (other, idle) = share(&|id| attr::set_policy(id, Policy::Other, false), &|id| attr::set_policy(id, Policy::Idle, false))?;
    klog::event!(Info, "sched", "ktest.nice_share", nice0_ms = n0 / MS, nice5_ms = n5 / MS, other_ms = other / MS, idle_ms = idle / MS;
        "Linux weights: nice 0 : nice 5 = 3.06, SCHED_OTHER : SCHED_IDLE = 341");
    kassert!(n5 > 0 && n0 > n5 + n5 / 2, "nice 0 got {} ms, nice 5 {} ms: not in weight order (want > 1.5 x)", n0 / MS, n5 / MS);
    kassert!(other > 4 * idle, "SCHED_OTHER got {} ms, SCHED_IDLE {} ms (want > 4 x)", other / MS, idle / MS);
    // On hardware the shares follow the weight table (a wrong table or nice-to-weight mapping fails here):
    // 1024 : 335 = 3.06 within 2.4..3.9; 1024 : 3 leaves SCHED_IDLE under 3 % of SCHED_OTHER. Under QEMU vCPU
    // stalls make shares unreliable: the orderings above only.
    if !hal::cpu::live().has("hypervisor") {
        let ratio_x100 = n0 * 100 / n5.max(1);
        kassert!((240..=390).contains(&ratio_x100), "nice 0 : nice 5 = {}.{:02} (Linux weights 3.06; want 2.40..3.90)", ratio_x100 / 100, ratio_x100 % 100);
        kassert!(idle * 33 < other, "SCHED_IDLE got {} ms beside SCHED_OTHER's {} ms (weights 3 : 1024; want < 3 %)", idle / MS, other / MS);
    }
    Ok(())
}

/// fork/clone's copy: nice, policy and mask come along; reset-on-fork turns a negative nice into 0 and is not
/// inherited.
fn copy_attrs() -> R {
    let Some((t, _home, _pin)) = two_cpus() else { return ktest::skip("needs two CPUs") };
    let stop = Arc::new(AtomicBool::new(false));
    let mut hs = Vec::new();
    for name in ["sc-copy-a", "sc-copy-b", "sc-copy-c"] {
        let s = stop.clone();
        hs.push(spawn(name, move || {
            while !s.load(Ordering::Acquire) {
                crate::sleep_ms(2);
            }
        })?);
    }
    let r = (|| -> R {
        let (a, b, c) = (hs[0].id(), hs[1].id(), hs[2].id());
        attr::set_nice(a, -3).map_err(|e| format!("{e:?}"))?;
        attr::set_policy(a, Policy::Batch, false).map_err(|e| format!("{e:?}"))?;
        attr::set_affinity(a, &one(t)).map_err(|e| format!("{e:?}"))?;
        attr::copy_attrs(a, b).map_err(|e| format!("{e:?}"))?;
        kassert!(attr::nice(b) == Some(-3) && attr::policy(b) == Some((Policy::Batch, false)), "copy: nice {:?} policy {:?}", attr::nice(b), attr::policy(b));
        kassert!(attr::affinity(b) == Some(one(t)), "copy: mask {:?}", attr::affinity(b));
        kassert!(hs[1].thread().weight() == attr::nice_weight(-3), "copy: weight {}", hs[1].thread().weight());
        attr::set_policy(a, Policy::Batch, true).map_err(|e| format!("{e:?}"))?;
        attr::copy_attrs(a, c).map_err(|e| format!("{e:?}"))?;
        kassert!(attr::nice(c) == Some(0) && attr::policy(c) == Some((Policy::Batch, false)), "reset-on-fork copy: nice {:?} policy {:?}", attr::nice(c), attr::policy(c));
        kassert!(attr::copy_attrs(u64::MAX - 1, c) == Err(AttrError::NoThread), "copy from nothing");
        Ok(())
    })();
    stop.store(true, Ordering::Release);
    for h in &hs {
        join(h, "copy")?;
    }
    r
}

/// The user-mode marks split CPU time: ~40 ms between enter and exit is user time, ~40 ms after is system time;
/// user + system is all the thread ran; the CPU's user time grows with it.
fn split() -> R {
    let Some((t, _home, _pin)) = two_cpus() else { return ktest::skip("needs two CPUs") };
    let out = Arc::new(ksync::SpinLock::new(None));
    let o = out.clone();
    let cpu_user0 = crate::cpu_stats().get(t).map_or(0, |c| c.user_ns);
    let h = spawn("sc-split", move || {
        let me = current_id();
        let _ = attr::set_affinity(me, &one(t));
        let a = crate::cputime::current();
        crate::cputime::mark(true);
        let inside = crate::cputime::in_user();
        busy_for(40 * MS);
        let mid = crate::cputime::current();
        crate::cputime::mark(false);
        busy_for(40 * MS);
        let b = crate::cputime::current();
        *o.lock() = Some((a, mid, b, inside, crate::cputime::in_user()));
    })?;
    join(&h, "split")?;
    let (a, mid, b, inside, after) = out.lock().take().ok_or("no result")?;
    let user = b.user_ns - a.user_ns;
    let sys = b.system_ns - a.system_ns;
    klog::event!(Info, "sched", "ktest.cputime_split", user_ms = user / MS, system_ms = sys / MS);
    kassert!(inside && !after, "in_user: {inside} inside, {after} after");
    kassert!(mid.user_ns > a.user_ns, "the running user slice was not visible ({} -> {})", a.user_ns, mid.user_ns);
    kassert!(user >= 30 * MS && user < 2 * SEC, "user time {} ms for 40 ms marked as user", user / MS);
    kassert!(sys >= 30 * MS, "system time {} ms for 40 ms after the mark", sys / MS);
    let total = crate::cputime::of_thread(h.id()).map(|x| x.total_ns());
    if let Some(total) = total {
        kassert!(total >= b.total_ns(), "total went back");
    }
    let cpu_user1 = crate::cpu_stats().get(t).map_or(0, |c| c.user_ns);
    kassert!(cpu_user1 >= cpu_user0 + 30 * MS, "CPU {t}'s user time grew by {} ms only", (cpu_user1 - cpu_user0) / MS);
    Ok(())
}

/// Read from another CPU while the thread runs: CPU time never goes back and includes the running slice; it
/// does not grow while the thread sleeps.
fn monotonic_read() -> R {
    let Some((t, _home, _pin)) = two_cpus() else { return ktest::skip("needs two CPUs") };
    let stop = Arc::new(AtomicBool::new(false));
    let sleep_now = Arc::new(AtomicBool::new(false));
    let (s, sl) = (stop.clone(), sleep_now.clone());
    let h = spawn("sc-mono", move || {
        let _ = attr::set_affinity(current_id(), &one(t));
        while !s.load(Ordering::Acquire) {
            if sl.load(Ordering::Acquire) {
                crate::sleep_ms(2);
            } else {
                busy_for(MS);
            }
        }
    })?;
    let r = (|| -> R {
        crate::sleep_ms(20);
        let th = h.thread().clone();
        let mut last = th.cpu_times();
        let mut grew = 0u32;
        for i in 0..20_000u32 {
            let now = th.cpu_times();
            kassert!(now.total_ns() >= last.total_ns() && now.user_ns >= last.user_ns && now.system_ns >= last.system_ns,
                "read {i}: went back from {last:?} to {now:?}");
            grew += u32::from(now.total_ns() > last.total_ns());
            last = now;
        }
        kassert!(grew > 100, "the running slice was not visible ({grew} of 20000 reads grew)");
        sleep_now.store(true, Ordering::Release);
        crate::sleep_ms(50);
        let a = th.cpu_times().total_ns();
        crate::sleep_ms(200);
        let b = th.cpu_times().total_ns();
        klog::event!(Info, "sched", "ktest.cputime_sleeping", ran_ms_in_200 = (b - a) / MS);
        kassert!(b - a < 60 * MS, "a sleeping thread gained {} ms of CPU time in 200 ms", (b - a) / MS);
        Ok(())
    })();
    stop.store(true, Ordering::Release);
    join(&h, "mono")?;
    r
}

/// Blocking counts voluntary switches; yielding to another runnable thread counts involuntary ones.
fn switch_counts() -> R {
    let Some((t, _home, _pin)) = two_cpus() else { return ktest::skip("needs two CPUs") };
    let h = spawn("sc-nvcsw", || {
        for _ in 0..20 {
            crate::sleep_ms(1);
        }
    })?;
    join(&h, "sleeper")?;
    let c = h.thread().cpu_times();
    kassert!(c.nvcsw >= 20, "20 sleeps gave {} voluntary switches", c.nvcsw);
    let go = Arc::new(AtomicU64::new(0));
    let mut hs = Vec::new();
    for name in ["sc-yield-a", "sc-yield-b"] {
        let g = go.clone();
        let h = spawn(name, move || {
            let _ = attr::set_affinity(current_id(), &one(t));
            g.fetch_add(1, Ordering::AcqRel);
            let end = time::now_ns() + 5 * SEC;
            while g.load(Ordering::Acquire) < 2 && time::now_ns() < end {
                crate::sleep_ms(1);
            }
            for _ in 0..50 {
                busy_for(MS / 10);
                crate::yield_now();
            }
        })?;
        hs.push(h);
    }
    for h in &hs {
        join(h, "yielders")?;
    }
    let iv: u64 = hs.iter().map(|h| h.thread().cpu_times().nivcsw).sum();
    kassert!(iv >= 10, "100 yields between two threads on one CPU gave {iv} involuntary switches");
    Ok(())
}
