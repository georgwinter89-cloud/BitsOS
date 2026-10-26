//! Host tests of the timers in the signal state (`state/timers.rs`): ITIMER_REAL's fire-once-then-re-arm on
//! dequeue, the CPU itimers' tick rules, POSIX timers' overrun accounting, settime invalidating a queued
//! signal's re-arm, ignored periodic timers, SIGEV_NONE reads and id allocation. Times are plain numbers.

use super::*;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicU64};
use sched::cputime::CpuTimes;
use uapi::ab::siginfo::*;

const MS: u64 = 1_000_000;
const SIGRT: u32 = 40;

fn group() -> Signals {
    let mut s = Signals::default();
    s.add_thread(1, 1, 0, Arc::new(AtomicBool::new(false)), Arc::new(AtomicU64::new(1001)));
    s
}

fn none() -> Samples {
    Samples::default()
}

fn posix(s: &mut Signals, notify: Notify, signo: u32) -> i32 {
    let id = s.timers.alloc_id().unwrap();
    s.timers.posix.insert(id, PosixTimer::new(TClock::Mono, notify, signo, 0x77));
    id
}

#[test]
fn forward_is_hrtimer_forward() {
    assert_eq!(forward(100, 50, 10), (100, 0), "not expired");
    assert_eq!(forward(100, 100, 10), (110, 1), "at the expiry: one period");
    assert_eq!(forward(100, 105, 10), (110, 1));
    assert_eq!(forward(100, 130, 10), (140, 4), "130 itself is not after 130");
    assert_eq!(forward(100, 200, 0), (100, 0), "no interval");
}

#[test]
fn itimer_real_fires_once_and_rearms_on_dequeue() {
    let mut s = group();
    assert_eq!(s.timers.real_set(1000, 50 * MS, 10 * MS), (0, 0));
    assert_eq!(s.timers.real_get(1000 + 20 * MS), (30 * MS, 10 * MS));
    let (out, wall, _) = s.fire_timers(1000 + 20 * MS, &none(), u64::MAX);
    assert!(out.is_empty() && wall == Some(1000 + 50 * MS), "not due yet");
    let (out, _, _) = s.fire_timers(1000 + 50 * MS, &none(), u64::MAX);
    assert_eq!(out.len(), 1, "SIGALRM sent");
    assert!(s.shared.has(SIGALRM));
    assert_eq!(s.timers.real_get(1000 + 60 * MS), (0, 10 * MS), "not re-armed while SIGALRM is pending");
    let (info, _) = s.dequeue_at(1, !0, 1000 + 75 * MS).unwrap();
    assert_eq!((info.signo(), info.code()), (SIGALRM, SI_KERNEL));
    assert_eq!(s.timers.real_get(1000 + 75 * MS), (5 * MS, 10 * MS), "re-armed from the old expiry, forwarded past now");
    // A one-shot alarm: an expired armed timer reads 1 µs; disarming returns the old value.
    let mut s = group();
    s.timers.real_set(0, 5 * MS, 0);
    assert_eq!(s.timers.real_get(6 * MS), (1_000, 0), "expired, still armed: 1 µs");
    assert_eq!(s.timers.real_set(6 * MS, 0, 0), (1_000, 0));
    assert!(!s.timers.configured());
}

#[test]
fn cpu_itimers_add_a_tick_and_step_by_incr() {
    let mut s = group();
    // CPU time 100 ms used; set VIRTUAL to 10 ms every 5 ms.
    assert_eq!(s.timers.cpu_set(CPU_VIRT, 100 * MS, 10 * MS, 5 * MS), (0, 0));
    assert_eq!(s.timers.virt.expires, 111 * MS, "value + one tick + now");
    assert_eq!(s.timers.cpu_get(CPU_VIRT, 105 * MS), (6 * MS, 5 * MS));
    assert_eq!(s.timers.cpu_get(CPU_VIRT, 120 * MS), (TICK_NSEC, 5 * MS), "about to fire");
    let smp = Samples { own: Some(CpuTimes { user_ns: 112 * MS, system_ns: 50 * MS, nvcsw: 0, nivcsw: 0 }), ..Samples::default() };
    let (out, _, cpu) = s.fire_timers(0, &smp, u64::MAX);
    assert_eq!(out.len(), 1);
    assert!(s.shared.has(SIGVTALRM));
    assert_eq!(s.timers.virt.expires, 116 * MS, "expires += incr");
    assert_eq!(cpu, Some(4 * MS), "the CPU time left to the next expiry");
    // The old setting comes back relative.
    assert_eq!(s.timers.cpu_set(CPU_VIRT, 114 * MS, 0, 0), (2 * MS, 5 * MS));
    assert_eq!(s.timers.virt.expires, 0);
}

#[test]
fn posix_timer_overrun_counts_missed_periods() {
    let mut s = group();
    s.sigprocmask(SIG_BLOCK, Some(bit(SIGRT))).unwrap();
    let id = posix(&mut s, Notify::Group, SIGRT);
    assert_eq!(id, 0, "the first id is 0");
    s.timers.posix_set(id, 0, 10 * MS, 10 * MS).unwrap();
    let (out, _, _) = s.fire_timers(10 * MS, &none(), u64::MAX);
    assert_eq!(out.len(), 1);
    // Not re-armed while queued: nothing more fires.
    let (out, wall, _) = s.fire_timers(30 * MS, &none(), u64::MAX);
    assert!(out.is_empty() && wall.is_none());
    // gettime forwards a periodic timer whose signal is pending.
    assert_eq!(s.timers.posix_get(id, 35 * MS), Some((5 * MS, 10 * MS)));
    // Taken at 55 ms: expiries at 20..50 were missed (4).
    let (info, _) = s.dequeue_at(1, bit(SIGRT), 55 * MS).unwrap();
    assert_eq!((info.signo(), info.code(), info.get32(16)), (SIGRT, SI_TIMER, 0));
    assert_eq!(info.get32(20) as i32, 4, "si_overrun");
    assert_eq!(info.get64(24), 0x77, "si_value");
    assert_eq!(info.get32(32), 0, "si_sys_private never reaches a program");
    assert_eq!(s.timers.posix_overrun(id), Some(4));
    let t = s.timers.posix[&id];
    assert!(t.active && t.expires == 60 * MS, "re-armed at the next period");
}

#[test]
fn settime_invalidates_a_queued_rearm_and_bumps_overrun() {
    let mut s = group();
    s.sigprocmask(SIG_BLOCK, Some(bit(SIGRT))).unwrap();
    let id = posix(&mut s, Notify::Group, SIGRT);
    s.timers.posix_set(id, 0, 10 * MS, 10 * MS).unwrap();
    s.fire_timers(10 * MS, &none(), u64::MAX);
    // Set again (one-shot) while its signal is queued; it fires again: the queued siginfo counts the overrun.
    s.timers.posix_set(id, 12 * MS, 20 * MS, 0).unwrap();
    let (out, _, _) = s.fire_timers(20 * MS, &none(), u64::MAX);
    assert!(out.is_empty(), "no second entry");
    let (info, _) = s.dequeue_at(1, bit(SIGRT), 25 * MS).unwrap();
    assert_eq!(info.get32(20), 1, "si_overrun of the queued entry");
    assert!(s.dequeue_at(1, bit(SIGRT), 25 * MS).is_none(), "one entry only");
    assert!(!s.timers.posix[&id].active, "the old generation does not re-arm it");
    assert_eq!(s.timers.posix_overrun(id), Some(0));
}

#[test]
fn ignored_periodic_timer_keeps_running_and_none_counts() {
    let mut s = group();
    s.sigaction(SIGRT, Some(SigAction { handler: SIG_IGN, ..Default::default() })).unwrap();
    let id = posix(&mut s, Notify::Group, SIGRT);
    s.timers.posix_set(id, 0, 10 * MS, 10 * MS).unwrap();
    let (out, wall, _) = s.fire_timers(10 * MS, &none(), u64::MAX);
    assert!(out.is_empty() && !s.shared.has(SIGRT), "ignored: nothing queued");
    assert_eq!(wall, Some(20 * MS), "re-armed at once");
    // SIGEV_NONE: never fires, gettime counts down and forwards.
    let n = posix(&mut s, Notify::None, 0);
    s.timers.posix_set(n, 0, 10 * MS, 0).unwrap();
    assert!(!s.timers.posix[&n].active);
    assert_eq!(s.timers.posix_get(n, 4 * MS), Some((6 * MS, 0)));
    assert_eq!(s.timers.posix_get(n, 11 * MS), Some((0, 0)), "an expired one-shot SIGEV_NONE reads 0");
    let p = posix(&mut s, Notify::None, 0);
    s.timers.posix_set(p, 0, 10 * MS, 10 * MS).unwrap();
    assert_eq!(s.timers.posix_get(p, 35 * MS), Some((5 * MS, 10 * MS)));
}

#[test]
fn ids_and_exec() {
    let mut s = group();
    let a = posix(&mut s, Notify::Group, SIGALRM);
    let b = posix(&mut s, Notify::Group, SIGALRM);
    assert_eq!((a, b), (0, 1));
    s.timers.posix.remove(&a);
    assert_eq!(s.timers.alloc_id(), Some(2), "ids go on, a freed one is not taken again at once");
    s.timers.next_id = i32::MAX;
    assert_eq!(s.timers.alloc_id(), Some(i32::MAX));
    assert_eq!(s.timers.alloc_id(), Some(0), "wraps within the positive ints");
    s.timers.real_set(0, 10 * MS, 0);
    s.drop_posix_timers();
    assert!(s.timers.posix.is_empty() && s.timers.real.active, "exec keeps the itimers");
    assert!(s.for_child(false).timers.posix.is_empty() && !s.for_child(false).timers.real.active, "fork starts with none");
}

#[test]
fn thread_target_gone_stops_the_timer() {
    let mut s = group();
    let id = posix(&mut s, Notify::Thread(9), SIGRT);
    s.timers.posix_set(id, 0, 10 * MS, 10 * MS).unwrap();
    let (out, wall, _) = s.fire_timers(10 * MS, &none(), u64::MAX);
    assert!(out.is_empty() && wall.is_none() && !s.timers.posix[&id].active);
}
