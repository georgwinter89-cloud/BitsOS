//! AB-6 fixer: interval timers and POSIX timers, the senders of SIGALRM, SIGVTALRM, SIGPROF and SI_TIMER
//! signals (linux-6.6/kernel/time/itimer.c: getitimer :112, setitimer :336, alarm :312; kernel/time/
//! posix-timers.c: timer_create :551, timer_gettime :648, timer_getoverrun :714, timer_settime :911,
//! timer_delete :996). This file checks and copies; the timers live in the group's signal state and fire from
//! `proc::itimer`'s service.
//!
//! Clocks of timer_create: REALTIME and TAI (absolute times are wall times; V2's wall clock never jumps, so they
//! count on the monotonic clock), MONOTONIC, BOOTTIME (= MONOTONIC: V2 never suspends), the CPU clocks
//! (process/thread, also by encoded pid), and the alarm clocks as timerfd takes them (CAP_WAKE_ALARM = root,
//! then their base clock). MONOTONIC_RAW and the COARSE clocks have no timers on Linux (EOPNOTSUPP).

use crate::SysCtx;
use crate::clock::{Clock, decode, realtime_ns, valid_ts};
use proc::itimer::{ClockSpec, SigEvent};
use proc::signal::PosixTimer;
use uapi::Timespec;
use uapi::ab::clock::*;
use uapi::ab::timers::*;
use uapi::errno::*;

const NSEC: u64 = 1_000_000_000;
/// `KTIME_MAX`: `timespec64_to_ktime` saturates there.
const KTIME_MAX: u64 = i64::MAX as u64;

/// A `struct __kernel_old_itimerval` (it_interval, it_value as timevals) as (interval, value) in ns, checked
/// as `timeval_valid` (EINVAL). Pure.
pub(crate) fn itimerval_from(b: &[u8; ITIMERVAL_SIZE]) -> Result<(u64, u64), i64> {
    let w = |i: usize| i64::from_le_bytes(b[i * 8..i * 8 + 8].try_into().unwrap_or([0; 8]));
    let tv = |sec: i64, usec: i64| -> Result<u64, i64> {
        if sec < 0 || !(0..1_000_000).contains(&usec) {
            return Err(-EINVAL);
        }
        Ok((sec as u64).saturating_mul(NSEC).saturating_add(usec as u64 * 1000).min(KTIME_MAX))
    };
    Ok((tv(w(0), w(1))?, tv(w(2), w(3))?))
}

/// (interval, value) in ns as a `struct __kernel_old_itimerval` (µs truncated, `put_itimerval`). Pure.
pub(crate) fn itimerval_bytes(interval: u64, value: u64) -> [u8; ITIMERVAL_SIZE] {
    let mut b = [0u8; ITIMERVAL_SIZE];
    let ns = [interval, value];
    for (i, n) in ns.iter().enumerate() {
        b[i * 16..i * 16 + 8].copy_from_slice(&((n / NSEC) as i64).to_le_bytes());
        b[i * 16 + 8..i * 16 + 16].copy_from_slice(&(((n % NSEC) / 1000) as i64).to_le_bytes());
    }
    b
}

/// (interval, value) in ns as a `struct __kernel_itimerspec`. Pure.
pub(crate) fn itimerspec_bytes(interval: u64, value: u64) -> [u8; 32] {
    let mut b = [0u8; 32];
    b[..16].copy_from_slice(&Timespec::from_ns(interval).to_bytes());
    b[16..].copy_from_slice(&Timespec::from_ns(value).to_bytes());
    b
}

fn sys_getitimer(c: &mut SysCtx<'_>) -> i64 {
    let [which, value, ..] = c.args;
    let (v, iv) = match c.proc.getitimer(which as i32) {
        Ok(r) => r,
        Err(e) => return -e,
    };
    if c.proc.mm.copy_to_user(value, &itimerval_bytes(iv, v)).is_err() {
        return -EFAULT;
    }
    0
}
crate::syscall_nr!(uapi::ab::nr_more::GETITIMER, "getitimer", sys_getitimer);

fn sys_setitimer(c: &mut SysCtx<'_>) -> i64 {
    let [which, value, ovalue, ..] = c.args;
    // A NULL value disarms (Linux still accepts it, with a one-time warning).
    let (iv, v) = if value != 0 {
        let mut b = [0u8; ITIMERVAL_SIZE];
        if c.proc.mm.copy_from_user(&mut b, value).is_err() {
            return -EFAULT;
        }
        match itimerval_from(&b) {
            Ok(x) => x,
            Err(e) => return e,
        }
    } else {
        (0, 0)
    };
    let (ov, oiv) = match c.proc.group().setitimer(which as i32, v, iv) {
        Ok(r) => r,
        Err(e) => return -e,
    };
    if ovalue != 0 && c.proc.mm.copy_to_user(ovalue, &itimerval_bytes(oiv, ov)).is_err() {
        return -EFAULT;
    }
    0
}
crate::syscall_nr!(uapi::ab::nr_more::SETITIMER, "setitimer", sys_setitimer);

fn sys_alarm(c: &mut SysCtx<'_>) -> i64 {
    c.proc.group().alarm(c.args[0] as u32) as i64
}
crate::syscall_nr!(uapi::ab::nr_more::ALARM, "alarm", sys_alarm);

/// `clockid_to_kclock` + whether the clock has timers (EINVAL unknown, EOPNOTSUPP no `timer_create`), the
/// clock and whether absolute times are wall times. The alarm clocks need CAP_WAKE_ALARM (EPERM), which Linux
/// checks in `kc->timer_create` (after the id was written): returned as the third part.
fn timer_clock(id: i32) -> Result<(ClockSpec, bool, Option<i64>), i64> {
    let Some(clock) = decode(id) else { return Err(-EINVAL) };
    Ok(match clock {
        Clock::Cpu { pid, thread, which } => (ClockSpec::Cpu { pid, thread, which }, false, None),
        Clock::Alarm { real } => (ClockSpec::Mono, real, Some(-EPERM)),
        Clock::Realtime | Clock::Monotonic => match id {
            CLOCK_REALTIME | CLOCK_TAI => (ClockSpec::Mono, true, None),
            CLOCK_MONOTONIC | CLOCK_BOOTTIME => (ClockSpec::Mono, false, None),
            _ => return Err(-EOPNOTSUPP),
        },
    })
}

fn sys_timer_create(c: &mut SysCtx<'_>) -> i64 {
    let [clockid, sevp, idp, ..] = c.args;
    // The sigevent is copied before the clock is looked at (`SYSCALL_DEFINE3(timer_create)`).
    let ev = if sevp != 0 {
        let mut b = [0u8; SIGEVENT_SIZE];
        if c.proc.mm.copy_from_user(&mut b, sevp).is_err() {
            return -EFAULT;
        }
        Some(SigEvent::from_bytes(&b))
    } else {
        None
    };
    let (spec, wall, alarm) = match timer_clock(clockid as i32) {
        Ok(x) => x,
        Err(e) => return e,
    };
    let g = c.proc.group();
    let (id, notify, signo, value) = match g.timer_prepare(ev) {
        Ok(x) => x,
        Err((_, e)) => return -e,
    };
    if c.proc.mm.copy_to_user(idp, &id.to_le_bytes()).is_err() {
        return -EFAULT;
    }
    if let Some(e) = alarm
        && !c.proc.capable(proc::cred::Cap::WakeAlarm)
    {
        return e;
    }
    let clock = match g.timer_clock(spec) {
        Ok(k) => k,
        Err(e) => return -e,
    };
    let mut t = PosixTimer::new(clock, notify, signo, value);
    t.wall = wall;
    match g.timer_insert(id, t) {
        Ok(()) => 0,
        Err(e) => -e,
    }
}
crate::syscall_nr!(uapi::ab::nr_more::TIMER_CREATE, "timer_create", sys_timer_create);

fn sys_timer_settime(c: &mut SysCtx<'_>) -> i64 {
    let [id, flags, new, old, ..] = c.args;
    if new == 0 {
        return -EINVAL;
    }
    let mut b = [0u8; 32];
    if c.proc.mm.copy_from_user(&mut b, new).is_err() {
        return -EFAULT;
    }
    let ts = |o: usize| Timespec::from_bytes(b[o..o + 16].try_into().unwrap_or(&[0; 16]));
    let (Some(iv), Some(v)) = (valid_ts(&ts(0)), valid_ts(&ts(16))) else { return -EINVAL };
    let abs = flags as u32 as u64 & TIMER_ABSTIME != 0;
    let (ov, oiv) = match c.proc.group().timer_settime(id as i32, abs, v.min(KTIME_MAX), iv.min(KTIME_MAX), realtime_ns()) {
        Ok(r) => r,
        Err(e) => return -e,
    };
    if old != 0 && c.proc.mm.copy_to_user(old, &itimerspec_bytes(oiv, ov)).is_err() {
        return -EFAULT;
    }
    0
}
crate::syscall_nr!(uapi::ab::nr_more::TIMER_SETTIME, "timer_settime", sys_timer_settime);

fn sys_timer_gettime(c: &mut SysCtx<'_>) -> i64 {
    let [id, cur, ..] = c.args;
    let (v, iv) = match c.proc.timer_gettime(id as i32) {
        Ok(r) => r,
        Err(e) => return -e,
    };
    if c.proc.mm.copy_to_user(cur, &itimerspec_bytes(iv, v)).is_err() {
        return -EFAULT;
    }
    0
}
crate::syscall_nr!(uapi::ab::nr_more::TIMER_GETTIME, "timer_gettime", sys_timer_gettime);

fn sys_timer_getoverrun(c: &mut SysCtx<'_>) -> i64 {
    match c.proc.timer_getoverrun(c.args[0] as i32) {
        Ok(n) => n as i64,
        Err(e) => -e,
    }
}
crate::syscall_nr!(uapi::ab::nr_more::TIMER_GETOVERRUN, "timer_getoverrun", sys_timer_getoverrun);

fn sys_timer_delete(c: &mut SysCtx<'_>) -> i64 {
    match c.proc.timer_delete(c.args[0] as i32) {
        Ok(()) => 0,
        Err(e) => -e,
    }
}
crate::syscall_nr!(uapi::ab::nr_more::TIMER_DELETE, "timer_delete", sys_timer_delete);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn itimerval_round_trip_and_checks() {
        let mut b = [0u8; ITIMERVAL_SIZE];
        let put = |b: &mut [u8; ITIMERVAL_SIZE], i: usize, v: i64| b[i * 8..i * 8 + 8].copy_from_slice(&v.to_le_bytes());
        put(&mut b, 0, 1);
        put(&mut b, 1, 500_000);
        put(&mut b, 2, 2);
        put(&mut b, 3, 0);
        assert_eq!(itimerval_from(&b), Ok((1_500_000_000, 2_000_000_000)));
        assert_eq!(itimerval_bytes(1_500_000_999, 2_000_000_000), b, "ns below a microsecond are cut");
        put(&mut b, 1, 1_000_000);
        assert_eq!(itimerval_from(&b), Err(-EINVAL), "usec 1e6");
        put(&mut b, 1, 0);
        put(&mut b, 2, -1);
        assert_eq!(itimerval_from(&b), Err(-EINVAL), "negative seconds");
        put(&mut b, 2, i64::MAX);
        assert_eq!(itimerval_from(&b).map(|x| x.1), Ok(KTIME_MAX), "saturates at KTIME_MAX");
    }

    #[test]
    fn timer_clocks_as_linux() {
        assert_eq!(timer_clock(CLOCK_REALTIME), Ok((ClockSpec::Mono, true, None)));
        assert_eq!(timer_clock(CLOCK_BOOTTIME), Ok((ClockSpec::Mono, false, None)));
        assert_eq!(timer_clock(CLOCK_MONOTONIC_RAW), Err(-EOPNOTSUPP));
        assert_eq!(timer_clock(CLOCK_REALTIME_COARSE), Err(-EOPNOTSUPP));
        assert_eq!(timer_clock(10), Err(-EINVAL));
        assert_eq!(timer_clock(CLOCK_BOOTTIME_ALARM).map(|x| x.2), Ok(Some(-EPERM)));
        assert!(matches!(timer_clock(CLOCK_PROCESS_CPUTIME_ID), Ok((ClockSpec::Cpu { pid: 0, thread: false, .. }, false, None))));
    }
}
