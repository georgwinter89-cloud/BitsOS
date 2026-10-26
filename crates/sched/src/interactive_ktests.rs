//! SC-7a ktests: the grant/focus API turns a group's threads Interactive and back, SCHED_FIFO/RR only inside a
//! grant (lowering without one as Linux), SCHED_IDLE runs as Background, a fork's child does not inherit the
//! class, a group's grants end with its last thread; and under 64 busy Normal threads (plus SCHED_IDLE ones and
//! a busy Interactive one) an Interactive thread meets a generous wake-up deadline every time while nothing
//! starves; the last two threads of a granted group exiting at once take the grants with them; focus flips while
//! the groups' threads wake and exit leave consistent classes; a thread in front that runs and sleeps briefly in
//! a loop leaves a Normal thread on its CPU its share. Rerunnable: every run uses fresh group numbers and puts the focus back. Timing is judged on
//! hardware only (20 ms); under a hypervisor the limit is functional (500 ms).

use crate::attr::{self, Policy, SchedError};
use crate::interactive::{self, Grant, GrantError};
use crate::{Class, JoinHandle, cond_resched};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use ktest::{kassert, ktest};

type R = ktest::TestResult;

ktest!("sched.interactive.grants", Safe, grants);
ktest!("sched.interactive.deadline64", Safe, deadline64);
ktest!("sched.interactive.exit_race", Safe, exit_race);
ktest!("sched.interactive.focus_flips", Safe, focus_flips);
ktest!("sched.interactive.front_shares", Safe, front_shares);

const MS: u64 = 1_000_000;
const SEC: u64 = 1_000_000_000;

/// Test groups far above any pid (pids are 32-bit), new for every use.
static NEXT_GROUP: AtomicU64 = AtomicU64::new(1 << 48);

fn new_group() -> u64 {
    NEXT_GROUP.fetch_add(1, Ordering::Relaxed)
}

fn spawn_in(name: &str, group: u64, f: impl FnOnce() + Send + 'static) -> Result<JoinHandle, String> {
    crate::spawn_in_group(name, Class::Normal, group, f).map_err(|e| format!("spawn {name}: {e:?}"))
}

/// A thread of `group` that sleeps until `stop`.
fn sleeper(group: u64, stop: &Arc<AtomicBool>) -> Result<JoinHandle, String> {
    let s = stop.clone();
    spawn_in("sc7-sleeper", group, move || {
        while !s.load(Ordering::Acquire) {
            crate::sleep_ms(1);
        }
    })
}

fn class(h: &JoinHandle) -> Class {
    h.thread().class()
}

fn join_all(hs: &[JoinHandle], what: &str) -> R {
    for h in hs {
        kassert!(h.join_timeout(30 * SEC), "{what}: thread {} did not finish within 30 s", h.id());
    }
    Ok(())
}

/// Puts the focus back to `prev` (or none when its group is gone) whatever the test did.
struct FocusBack(Option<u64>);

impl Drop for FocusBack {
    fn drop(&mut self) {
        if interactive::set_focus(self.0.unwrap_or(0)).is_err() {
            let _ = interactive::set_focus(0);
        }
    }
}

fn grants() -> R {
    let _back = FocusBack(interactive::focus());
    let stop = Arc::new(AtomicBool::new(false));
    let r = grants_body(&stop);
    // Every sleeper ends with `stop` (also those an early failure dropped the handle of).
    stop.store(true, Ordering::Release);
    r
}

fn grants_body(stop: &Arc<AtomicBool>) -> R {
    let (g, g2, g3) = (new_group(), new_group(), new_group());
    let a = sleeper(g, stop)?;
    let b = sleeper(g, stop)?;
    let idle = sleeper(g, stop)?;
    let other = sleeper(g2, stop)?;
    let (ta, tb, ti) = (a.id(), b.id(), idle.id());
    kassert!(class(&a) == Class::Normal && class(&other) == Class::Normal, "user threads start Normal");
    kassert!(attr::set_policy(ti, Policy::Idle, false).is_ok());
    kassert!(class(&idle) == Class::Background, "SCHED_IDLE runs as Background, not {:?}", class(&idle));
    kassert!(idle.thread().weight() == attr::WEIGHT_IDLEPRIO);

    // FIFO/RR without a grant: refused, nothing changes.
    let refused = attr::RT_REFUSED.get();
    kassert!(attr::set_scheduler(ta, Policy::Fifo, 10, false) == Err(SchedError::NotGranted));
    kassert!(attr::set_scheduler(ta, Policy::Rr, 1, false) == Err(SchedError::NotGranted));
    kassert!(attr::RT_REFUSED.get() >= refused + 2, "refusals counted");
    kassert!(attr::policy(ta) == Some((Policy::Other, false)) && attr::rt_priority(ta) == Some(0));
    kassert!(attr::set_scheduler(ta, Policy::Fifo, 0, false) == Err(SchedError::Invalid), "FIFO needs a priority");
    kassert!(attr::set_scheduler(ta, Policy::Other, 3, false) == Err(SchedError::Invalid), "a fair policy has none");
    kassert!(attr::set_scheduler(ta, Policy::Fifo, 100, false) == Err(SchedError::Invalid));
    kassert!(attr::set_policy(ta, Policy::Rr, false) == Err(attr::AttrError::Invalid), "RT only through set_scheduler");
    kassert!(!attr::rt_granted(ta));

    // Focus: the group runs Interactive (SCHED_IDLE stays Background), FIFO is accepted.
    kassert!(interactive::set_focus(g).is_ok());
    kassert!(interactive::focus() == Some(g) && interactive::grants_of(g) == Grant::Focus as u8);
    kassert!(class(&a) == Class::Interactive && class(&b) == Class::Interactive, "focus did not raise: {:?} {:?}", class(&a), class(&b));
    kassert!(a.thread().weight() == 3121, "Interactive weight {}", a.thread().weight());
    kassert!(class(&idle) == Class::Background, "SCHED_IDLE in front stays Background");
    kassert!(class(&other) == Class::Normal, "another group is not raised");
    kassert!(attr::rt_granted(ta));
    kassert!(attr::set_scheduler(ta, Policy::Fifo, 10, false) == Ok(()));
    kassert!(attr::policy(ta) == Some((Policy::Fifo, false)) && attr::rt_priority(ta) == Some(10));
    kassert!(class(&a) == Class::Interactive && a.thread().weight() == 3121);
    kassert!(
        crate::thread_stats().iter().any(|s| s.id == ta && s.rt_prio == 10 && s.policy == Policy::Fifo && s.class == Class::Interactive && s.group == g),
        "thread_stats"
    );
    // A new thread of the group in front is Interactive from its spawn.
    let c = sleeper(g, stop)?;
    kassert!(class(&c) == Class::Interactive, "a new thread of the focused group is {:?}", class(&c));

    // A compositor grant outlives a focus change; the focus moves on to g2.
    kassert!(interactive::set_grant(g, Grant::Compositor, true) == Ok(4));
    kassert!(interactive::set_focus(g2) == Ok(g));
    kassert!(class(&a) == Class::Interactive && class(&other) == Class::Interactive);
    kassert!(interactive::grants_of(g) == Grant::Compositor as u8 && interactive::grants_of(g2) == Grant::Focus as u8);
    let list = interactive::grants();
    kassert!(list.contains(&(g, 1)) && list.contains(&(g2, 4)), "{list:?}");

    // Revoked: back to Normal; the FIFO policy stays reported, may be lowered, not raised or changed.
    kassert!(interactive::set_grant(g, Grant::Compositor, false) == Ok(4));
    kassert!(class(&a) == Class::Normal && class(&b) == Class::Normal && class(&c) == Class::Normal, "revoke did not lower");
    kassert!(a.thread().weight() == 1024);
    kassert!(attr::policy(ta) == Some((Policy::Fifo, false)));
    kassert!(attr::set_scheduler(ta, Policy::Fifo, 5, false) == Ok(()), "lowering needs no grant (Linux)");
    kassert!(attr::set_scheduler(ta, Policy::Fifo, 6, false) == Err(SchedError::NotGranted), "raising does");
    kassert!(attr::set_scheduler(ta, Policy::Rr, 5, false) == Err(SchedError::NotGranted), "so does changing it");
    kassert!(attr::set_scheduler(ta, Policy::Other, 0, false) == Ok(()) && attr::rt_priority(ta) == Some(0));

    // Fork: the child (another group) gets the policy, not the class; reset-on-fork drops FIFO/RR.
    kassert!(interactive::set_grant(g, Grant::Input, true).is_ok());
    kassert!(attr::set_scheduler(tb, Policy::Rr, 7, false) == Ok(()));
    let kid = sleeper(g3, stop)?;
    kassert!(attr::copy_attrs(tb, kid.id()).is_ok());
    kassert!(attr::policy(kid.id()) == Some((Policy::Rr, false)) && attr::rt_priority(kid.id()) == Some(7));
    kassert!(class(&kid) == Class::Normal, "a fork's child inherited the class: {:?}", class(&kid));
    kassert!(attr::set_scheduler(tb, Policy::Rr, 7, true) == Ok(()));
    kassert!(attr::copy_attrs(tb, kid.id()).is_ok());
    kassert!(attr::policy(kid.id()) == Some((Policy::Other, false)) && attr::rt_priority(kid.id()) == Some(0), "reset-on-fork");
    kassert!(interactive::set_grant(g, Grant::Input, false).is_ok());
    kassert!(class(&b) == Class::Normal);

    // Refusals: kernel threads and groups without threads.
    kassert!(interactive::set_focus(new_group()) == Err(GrantError::NoGroup));
    kassert!(interactive::set_grant(new_group(), Grant::Compositor, true) == Err(GrantError::NoGroup));
    kassert!(interactive::set_grant(0, Grant::Input, true) == Err(GrantError::NoGroup));

    // g2's last thread ends: its grants end with it.
    let s2 = Arc::new(AtomicBool::new(false));
    let extra = sleeper(g2, &s2)?;
    kassert!(class(&extra) == Class::Interactive);
    s2.store(true, Ordering::Release);
    join_all(core::slice::from_ref(&extra), "g2 extra")?;
    kassert!(interactive::grants_of(g2) != 0, "the grant ended with a thread that was not the last");
    stop.store(true, Ordering::Release);
    join_all(&[a, b, idle, other, c, kid], "sleepers")?;
    kassert!(interactive::grants_of(g2) == 0 && interactive::focus() != Some(g2), "the grant outlived its group");
    kassert!(interactive::grants_of(g) == 0);
    Ok(())
}

/// Busy work that cannot be optimised away.
fn spin(n: u32) {
    let mut x = 0u64;
    for i in 0..n {
        x = core::hint::black_box(x.wrapping_mul(31).wrapping_add(i as u64));
    }
    core::hint::black_box(x);
}

/// 64 busy Normal threads (4 per CPU on 16), 2 SCHED_IDLE ones and a busy Interactive one; an Interactive
/// thread sleeps 2 ms and wakes 150 times. Every wake-up runs within the deadline, every Normal thread advances
/// in the three queue rounds after the probe, the SCHED_IDLE ones advance at all (weight 3: Linux's ~0.1 %),
/// the metric sees the waits.
fn deadline64() -> R {
    let n = crate::wait_cpus_online(5 * SEC);
    if n < 2 {
        return ktest::skip("needs two CPUs");
    }
    let _back = FocusBack(interactive::focus());
    let _pin = crate::pin_for_now(crate::current_cpu()).ok_or("could not pin the driver")?;
    let stop = Arc::new(AtomicBool::new(false));
    let go = Arc::new(AtomicBool::new(false));
    let r = deadline_body(n, &stop, &go);
    stop.store(true, Ordering::Release);
    go.store(true, Ordering::Release);
    let (j, hs) = r;
    let joined = join_all(&hs, "hogs");
    j?;
    joined
}

fn deadline_body(n: usize, stop: &Arc<AtomicBool>, go: &Arc<AtomicBool>) -> (R, Vec<JoinHandle>) {
    let mut hs = Vec::new();
    let r = deadline_run(n, stop, go, &mut hs);
    (r, hs)
}

fn deadline_run(n: usize, stop: &Arc<AtomicBool>, go: &Arc<AtomicBool>, hs: &mut Vec<JoinHandle>) -> R {
    const BUSY: usize = 64;
    const IDLE_HOGS: usize = 2;
    const ROUNDS: usize = 150;
    let limit = if hal::cpu::live().has("hypervisor") { 500 * MS } else { 20 * MS };
    let g = new_group();
    let total = BUSY + IDLE_HOGS;
    let progress: Arc<Vec<AtomicU64>> = Arc::new((0..total).map(|_| AtomicU64::new(0)).collect());
    let lat = Arc::new(ksync::SpinLock::new(Vec::new()));
    let waits0 = interactive::WAITS.get();
    for i in 0..total {
        let (s, p) = (stop.clone(), progress.clone());
        let h = spawn_in("sc7-hog", 0, move || {
            while !s.load(Ordering::Acquire) {
                spin(2_000);
                p[i].fetch_add(1, Ordering::Relaxed);
                cond_resched();
            }
        })?;
        if i >= BUSY {
            kassert!(attr::set_policy(h.id(), Policy::Idle, false).is_ok());
        }
        hs.push(h);
    }
    // The busy Interactive one: runs 5 ms, sleeps 1 ms (the program in front rendering).
    let s = stop.clone();
    hs.push(spawn_in("sc7-front-busy", g, move || {
        while !s.load(Ordering::Acquire) {
            let t0 = time::now_ns();
            while time::now_ns() - t0 < 5 * MS {
                spin(1_000);
                cond_resched();
            }
            crate::sleep_ms(1);
        }
    })?);
    let (gg, l) = (go.clone(), lat.clone());
    let probe = spawn_in("sc7-front", g, move || {
        while !gg.load(Ordering::Acquire) {
            crate::sleep_ms(1);
        }
        for _ in 0..ROUNDS {
            let want = time::now_ns() + 2 * MS;
            crate::sleep_ns(2 * MS);
            l.lock().push(time::now_ns().saturating_sub(want));
        }
    })?;
    let probe_t = probe.thread().clone();
    hs.push(probe);
    kassert!(interactive::set_focus(g).is_ok());
    kassert!(probe_t.class() == Class::Interactive, "the probe is {:?}", probe_t.class());
    crate::sleep_ms(20);
    let before: Vec<u64> = progress.iter().map(|p| p.load(Ordering::Relaxed)).collect();
    go.store(true, Ordering::Release);
    let t0 = time::now_ns();
    while lat.lock().len() < ROUNDS && time::now_ns() - t0 < 60 * SEC {
        crate::sleep_ms(5);
    }
    let mid: Vec<u64> = progress.iter().map(|p| p.load(Ordering::Relaxed)).collect();
    // Three rounds of every CPU's queue (all threads one slice each), at least 100 ms: on 2 CPUs ~1 s.
    let round_ms = (total as u64 / n as u64 + 2) * (crate::SLICE_NS / MS);
    crate::sleep_ms((3 * round_ms).max(100));
    let end: Vec<u64> = progress.iter().map(|p| p.load(Ordering::Relaxed)).collect();
    let v = lat.lock().clone();
    kassert!(v.len() == ROUNDS, "{} of {ROUNDS} wake-ups within 60 s", v.len());
    let max = v.iter().copied().max().unwrap_or(0);
    let avg = v.iter().sum::<u64>() / v.len().max(1) as u64;
    let own = probe_t.interactive_wait_max();
    let st = interactive::wait_stats(time::now_ns());
    klog::event!(Info, "sched", "ktest.interactive_deadline", busy = BUSY, idle_hogs = IDLE_HOGS, cpus = n, late_max_us = max / 1000, late_avg_us = avg / 1000,
        wake_to_run_max_us = own / 1000, metric_max_us = st.max_ns / 1000, limit_ms = limit / MS);
    kassert!(max < limit, "an Interactive wake-up was {} ms late under {BUSY} busy threads (limit {} ms)", max / MS, limit / MS);
    kassert!(own < limit, "wake-to-run {} ms (limit {} ms)", own / MS, limit / MS);
    let seen = interactive::WAITS.get() - waits0;
    kassert!(seen >= ROUNDS as u64 / 2, "the metric saw {seen} waits");
    kassert!(st.max_ns >= own && interactive::WAIT_MAX_NS.get() == st.max_ns, "metric max {} < own {own} or counter {}", st.max_ns, interactive::WAIT_MAX_NS.get());
    for i in 0..BUSY {
        kassert!(end[i] > mid[i] && mid[i] >= before[i], "busy thread {i} starved: {} -> {} -> {}", before[i], mid[i], end[i]);
    }
    for (i, e) in end.iter().enumerate().skip(BUSY) {
        kassert!(*e > 0, "SCHED_IDLE thread {i} never ran");
    }
    Ok(())
}

/// The last two threads of a granted group exit at the same moment (on two CPUs): the grants end every time
/// (the race the fixer closed: each saw the other still alive), and the group cannot be granted afterwards.
fn exit_race() -> R {
    if crate::wait_cpus_online(5 * SEC) < 2 {
        return ktest::skip("needs two CPUs");
    }
    let _back = FocusBack(interactive::focus());
    const ROUNDS: usize = 32;
    for round in 0..ROUNDS {
        let g = new_group();
        let go = Arc::new(AtomicBool::new(false));
        let mut hs = Vec::new();
        for _ in 0..2 {
            let go = go.clone();
            hs.push(spawn_in("sc7-exiter", g, move || {
                while !go.load(Ordering::Acquire) {
                    core::hint::spin_loop();
                    cond_resched();
                }
            })?);
        }
        let granted = if round % 2 == 0 { interactive::set_focus(g).map(|_| ()) } else { interactive::set_grant(g, Grant::Input, true).map(|_| ()) };
        go.store(true, Ordering::Release);
        kassert!(granted.is_ok(), "round {round}: grant refused: {granted:?}");
        join_all(&hs, "exiters")?;
        kassert!(interactive::grants_of(g) == 0, "round {round}: the grants {} outlived the group", interactive::bits_names(interactive::grants_of(g)));
        kassert!(interactive::focus() != Some(g), "round {round}: the focus outlived the group");
        kassert!(interactive::set_grant(g, Grant::Compositor, true) == Err(GrantError::NoGroup), "round {round}: a group without threads was granted");
        kassert!(interactive::set_focus(g) == Err(GrantError::NoGroup), "round {round}: a group without threads got the focus");
    }
    Ok(())
}

/// Two groups of 4 threads each wake every millisecond and end after a while; the driver flips the focus
/// between them meanwhile. After every flip each live thread of the group in front is Interactive and each
/// of the other Normal; no grant survives; every flip is counted.
fn focus_flips() -> R {
    let _back = FocusBack(interactive::focus());
    let (g1, g2) = (new_group(), new_group());
    let stop = Arc::new(AtomicBool::new(false));
    let mut hs: Vec<(u64, JoinHandle)> = Vec::new();
    for (i, g) in [g1, g2, g1, g2, g1, g2, g1, g2].into_iter().enumerate() {
        let s = stop.clone();
        // Different lifetimes, so threads leave while the focus moves.
        let n = 40 + 20 * i as u32;
        hs.push((g, spawn_in("sc7-flip", g, move || {
            for _ in 0..n {
                if s.load(Ordering::Acquire) {
                    break;
                }
                crate::sleep_ms(1);
            }
        })?));
    }
    let changes0 = interactive::GRANT_CHANGES.get();
    let mut flips = 0u64;
    let mut r = Ok(());
    for i in 0..200 {
        let g = if i % 2 == 0 { g1 } else { g2 };
        match interactive::set_focus(g) {
            // Ok(g): it was in front already (the flip before found the other group gone), no change counted.
            Ok(old) if old != g => flips += 1,
            Ok(_) => {}
            // Every thread of g has left: fine, the other group may still be there.
            Err(GrantError::NoGroup) => {}
            Err(e) => {
                r = Err(format!("set_focus: {e:?}"));
                break;
            }
        }
        let front = interactive::focus();
        for (tg, h) in &hs {
            let t = h.thread();
            let want = if Some(*tg) == front { Class::Interactive } else { Class::Normal };
            // `left` only goes one way: a thread still in its group now was in it when the focus was set.
            if t.in_group() && t.class() != want {
                r = Err(format!("flip {i}: thread {} of group {tg} is {:?}, want {want:?} (front {front:?})", t.id, t.class()));
                break;
            }
        }
        if r.is_err() || hs.iter().all(|(_, h)| !h.thread().in_group()) {
            break;
        }
        crate::sleep_ns(300_000);
    }
    let _ = interactive::set_focus(0);
    let raised = hs.iter().filter(|(_, h)| h.thread().in_group() && h.thread().class() != Class::Normal).count();
    stop.store(true, Ordering::Release);
    let handles: Vec<JoinHandle> = hs.into_iter().map(|(_, h)| h).collect();
    let joined = join_all(&handles, "flippers");
    r?;
    joined?;
    kassert!(raised == 0, "{raised} threads stayed Interactive after the focus was removed");
    kassert!(flips >= 2, "only {flips} focus changes happened");
    kassert!(interactive::GRANT_CHANGES.get() >= changes0 + flips, "grant_changes {} -> {} for {flips} flips", changes0, interactive::GRANT_CHANGES.get());
    kassert!(interactive::grants_of(g1) == 0 && interactive::grants_of(g2) == 0, "a grant outlived its group");
    Ok(())
}

/// The fixer's starvation case: a thread in front runs 200 us and sleeps 10 us in a loop, next to a Normal
/// hog pinned to the same CPU. Forgiving the front thread's debt at every wake-up gave the hog only the gaps
/// (~5 %); with the bounded bonus the weights decide (3121 : 1024, ~25 %). Asserted: at least 12 %.
fn front_shares() -> R {
    let n = crate::wait_cpus_online(5 * SEC);
    if n < 2 {
        return ktest::skip("needs two CPUs");
    }
    let _back = FocusBack(interactive::focus());
    let me = crate::current_cpu();
    let _pin = crate::pin_for_now(me).ok_or("could not pin the driver")?;
    let cpu = (me + 1) % n;
    let g = new_group();
    let stop = Arc::new(AtomicBool::new(false));
    let ready = Arc::new(AtomicU64::new(0));
    let (s, rd) = (stop.clone(), ready.clone());
    let front = spawn_in("sc7-front-loop", g, move || {
        let Some(_p) = crate::pin_for_now(cpu) else { return };
        rd.fetch_add(1, Ordering::Release);
        while !s.load(Ordering::Acquire) {
            let t0 = time::now_ns();
            while time::now_ns() - t0 < 200_000 {
                spin(200);
                cond_resched();
            }
            crate::sleep_ns(10_000);
        }
    });
    let (s, rd) = (stop.clone(), ready.clone());
    let hog = spawn_in("sc7-hog", 0, move || {
        let Some(_p) = crate::pin_for_now(cpu) else { return };
        rd.fetch_add(1, Ordering::Release);
        while !s.load(Ordering::Acquire) {
            spin(2_000);
            cond_resched();
        }
    });
    let r = front_shares_measure(g, cpu, &front, &hog, &ready);
    stop.store(true, Ordering::Release);
    let hs: Vec<JoinHandle> = [front, hog].into_iter().filter_map(|h| h.ok()).collect();
    let joined = join_all(&hs, "front/hog");
    r?;
    joined
}

fn front_shares_measure(g: u64, cpu: usize, front: &Result<JoinHandle, String>, hog: &Result<JoinHandle, String>, ready: &AtomicU64) -> R {
    let (front, hog) = (front.as_ref()?, hog.as_ref()?);
    kassert!(interactive::set_focus(g).is_ok());
    let t0 = time::now_ns();
    while ready.load(Ordering::Acquire) < 2 && time::now_ns() - t0 < 5 * SEC {
        crate::sleep_ms(1);
    }
    kassert!(ready.load(Ordering::Acquire) == 2, "the two threads did not pin themselves to CPU {cpu}");
    kassert!(front.thread().class() == Class::Interactive, "the front thread is {:?}", front.thread().class());
    crate::sleep_ms(100);
    let (f0, h0, w0) = (front.thread().runtime_ns(), hog.thread().runtime_ns(), time::now_ns());
    crate::sleep_ms(1_000);
    let (f1, h1, w1) = (front.thread().runtime_ns(), hog.thread().runtime_ns(), time::now_ns());
    let (fd, hd) = (f1 - f0, h1 - h0);
    let share = hd * 1000 / (fd + hd).max(1);
    klog::event!(Info, "sched", "ktest.front_shares", cpu = cpu, front_ms = fd / MS, hog_ms = hd / MS, wall_ms = (w1 - w0) / MS, hog_permille = share);
    kassert!(share >= 120, "the Normal hog got {}.{} % of CPU {cpu} next to the thread in front ({} ms : {} ms)", share / 10, share % 10, hd / MS, fd / MS);
    Ok(())
}
