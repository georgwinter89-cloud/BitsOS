//! Clocks and sleeps of wave A: clock_gettime, clock_getres, clock_nanosleep (linux-6.6/kernel/time/
//! posix-timers.c :1132, :1252, :1373, the clock table :1514), nanosleep (hrtimer.c :2121), gettimeofday and
//! time (time.c :140, :62).
//!
//! V2's clocks: MONOTONIC, MONOTONIC_RAW, MONOTONIC_COARSE and BOOTTIME are `time::now_ns()` (TSC since boot;
//! no suspend, so BOOTTIME equals MONOTONIC). REALTIME, REALTIME_COARSE and TAI are CP-5's wall clock
//! (`time::wall`, the CMOS RTC read at boot; from the Unix epoch at boot without an RTC), the same clock FS-1
//! stamps file times with (the kernel installs it with `vfs::inode::set_clock`). The CPU clocks are SC-6's
//! (`cputime_sys`: user/system split, PROF/VIRT/SCHED, the running slice included, process-clock sleeps). The
//! ALARM clocks answer as Linux with an RTC device (`alarmtimer_get_rtcdev`; CachyOS's rtc_cmos, V2's CMOS RTC):
//! REALTIME_ALARM reads REALTIME and BOOTTIME_ALARM reads BOOTTIME (`alarm_clock_get_timespec`), their sleeps
//! need CAP_WAKE_ALARM (`alarm_timer_nsleep`; V2 has no suspend to wake from, so they are plain sleeps; AB-13,
//! nplb's PosixTimeClockGettime cases). Every clock has 1 ns resolution (the coarse ones are as exact as the
//! others; `hrtimer_resolution` for the alarm clocks).

use crate::SysCtx;
use uapi::Timespec;
use uapi::ab::clock::*;
use uapi::errno::*;

const NSEC: u64 = 1_000_000_000;

/// What a clock id names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Clock {
    Monotonic,
    Realtime,
    /// CPU time of a process (`pid` 0 = the caller) or of one of its threads; `which` = CPUCLOCK_PROF (0),
    /// VIRT (1) or SCHED (2).
    Cpu { pid: u32, thread: bool, which: u8 },
    /// REALTIME_ALARM (`real`) / BOOTTIME_ALARM.
    Alarm { real: bool },
}

/// `clockid_to_kclock` and the CPU-clock encoding (`CPUCLOCK_PID` = ~(id >> 3), bit 2 per-thread, the low two
/// bits PROF/VIRT/SCHED/FD). None = EINVAL.
pub(crate) fn decode(id: i32) -> Option<Clock> {
    if id < 0 {
        if id & 7 == 3 || id & 3 == 3 {
            return None; // CLOCKFD (posix dynamic clocks) and the invalid fourth kind
        }
        let pid = !(id >> 3);
        return Some(Clock::Cpu { pid: pid as u32, thread: id & 4 != 0, which: (id & 3) as u8 });
    }
    match id {
        CLOCK_REALTIME | CLOCK_REALTIME_COARSE | CLOCK_TAI => Some(Clock::Realtime),
        CLOCK_MONOTONIC | CLOCK_MONOTONIC_RAW | CLOCK_MONOTONIC_COARSE | CLOCK_BOOTTIME => Some(Clock::Monotonic),
        CLOCK_PROCESS_CPUTIME_ID => Some(Clock::Cpu { pid: 0, thread: false, which: crate::cputime_sys::CPUCLOCK_SCHED }),
        CLOCK_THREAD_CPUTIME_ID => Some(Clock::Cpu { pid: 0, thread: true, which: crate::cputime_sys::CPUCLOCK_SCHED }),
        CLOCK_REALTIME_ALARM => Some(Clock::Alarm { real: true }),
        CLOCK_BOOTTIME_ALARM => Some(Clock::Alarm { real: false }),
        _ => None,
    }
}

/// Whether clock_nanosleep works on the clock (`kc->nsleep`): REALTIME, MONOTONIC, BOOTTIME, TAI, the alarm
/// clocks (CAP_WAKE_ALARM) and CPU clocks except CLOCK_THREAD_CPUTIME_ID (`cputime_sys`).
pub(crate) fn can_sleep(id: i32) -> bool {
    id < 0 || matches!(id, CLOCK_REALTIME | CLOCK_MONOTONIC | CLOCK_BOOTTIME | CLOCK_TAI | CLOCK_REALTIME_ALARM | CLOCK_BOOTTIME_ALARM | CLOCK_PROCESS_CPUTIME_ID)
}

/// The wall clock in ns since the Unix epoch: CP-5's (CMOS RTC at boot, Europe/Berlin rules, plus the monotonic
/// clock since; AB-3 change request 5, INT-A1). Without a readable RTC it counts from the epoch at boot.
pub(crate) fn realtime_ns() -> u64 {
    time::wall::realtime_ns().unwrap_or_else(time::now_ns)
}

/// CLOCK_REALTIME minus CLOCK_MONOTONIC from the wall clock's base alone (Linux `offs_real`): the same value
/// every time until the base changes, so `timerfd::clock_was_set` can tell a jump from no jump (two reads of
/// `realtime_ns() - now_ns()` differ by the time between them). Matches `realtime_ns` (0 without a wall time).
pub(crate) fn realtime_offset_ns() -> i64 {
    use time::wall::Base;
    let (base, tz, ..) = time::wall::state();
    let off: i128 = match base {
        Base::None => 0,
        Base::Rtc { local, mono_ns } => match u64::try_from(tz.to_utc(&local)) {
            Ok(s) => s.saturating_mul(1_000_000_000) as i128 - mono_ns as i128,
            Err(_) => 0,
        },
        Base::Set { utc_ns, mono_ns } => utc_ns as i128 - mono_ns as i128,
    };
    off.clamp(i64::MIN as i128, i64::MAX as i128) as i64
}

fn read_ns(c: &SysCtx<'_>, clock: Clock) -> Result<u64, i64> {
    match clock {
        Clock::Monotonic => Ok(time::now_ns()),
        Clock::Realtime => Ok(realtime_ns()),
        Clock::Cpu { pid, thread, which } => crate::cputime_sys::cpu_clock_ns(c, pid, thread, which),
        Clock::Alarm { real: true } => Ok(realtime_ns()),
        Clock::Alarm { real: false } => Ok(time::now_ns()),
    }
}

fn put_ts(c: &SysCtx<'_>, addr: u64, ns: u64) -> i64 {
    c.proc.mm.copy_to_user(addr, &Timespec::from_ns(ns).to_bytes()).map(|_| 0).unwrap_or(-EFAULT)
}

fn sys_clock_gettime(c: &mut SysCtx<'_>) -> i64 {
    let Some(clock) = decode(c.args[0] as i32) else { return -EINVAL };
    match read_ns(c, clock) {
        Ok(ns) => put_ts(c, c.args[1], ns),
        Err(e) => e,
    }
}
crate::syscall!(CLOCK_GETTIME, sys_clock_gettime);

fn sys_clock_getres(c: &mut SysCtx<'_>) -> i64 {
    let Some(clock) = decode(c.args[0] as i32) else { return -EINVAL };
    let res = match clock {
        Clock::Cpu { pid, thread, which } => match crate::cputime_sys::cpu_clock_res(c, pid, thread, which) {
            Ok(r) => r,
            Err(e) => return e,
        },
        _ => 1,
    };
    if c.args[1] == 0 { 0 } else { put_ts(c, c.args[1], res) }
}
crate::syscall!(CLOCK_GETRES, sys_clock_getres);

/// `timespec64_valid`: seconds >= 0, nanoseconds in 0..1e9. Returns the time in ns (saturating).
pub(crate) fn valid_ts(t: &Timespec) -> Option<u64> {
    if t.sec < 0 || !(0..NSEC as i64).contains(&t.nsec) {
        return None;
    }
    Some((t.sec as u64).saturating_mul(NSEC).saturating_add(t.nsec as u64))
}

fn get_ts(c: &SysCtx<'_>, addr: u64) -> Result<Timespec, i64> {
    let mut b = [0u8; 16];
    c.proc.mm.copy_from_user(&mut b, addr).map_err(|_| -EFAULT)?;
    Ok(Timespec::from_bytes(&b))
}

/// Sleeps until `deadline` on the monotonic clock, in slices of at most an hour (no deadline arithmetic can
/// overflow), unless the process is killed or (AB-6) the thread gets a signal: then Some(time left).
pub(crate) fn sleep_until(c: &SysCtx<'_>, deadline: u64) -> Option<u64> {
    loop {
        let now = time::now_ns();
        if now >= deadline {
            return None;
        }
        let slice = (deadline - now).min(3600 * NSEC);
        if c.proc.sleep_interruptible(slice) {
            return Some(deadline.saturating_sub(time::now_ns()));
        }
    }
}

fn sys_nanosleep(c: &mut SysCtx<'_>) -> i64 {
    let [rqtp, rmtp, ..] = c.args;
    let t = match get_ts(c, rqtp) {
        Ok(t) => t,
        Err(e) => return e,
    };
    let Some(ns) = valid_ts(&t) else { return -EINVAL };
    relative_sleep(c, time::now_ns().saturating_add(ns), rmtp)
}

/// `do_nanosleep` for a relative sleep: interrupted by a signal, the time left goes to `rmtp` (EFAULT when it
/// cannot) and the call keeps a restart block (`hrtimer_nanosleep_restart`): ERESTART_RESTARTBLOCK becomes
/// EINTR when a handler runs, else `restart_syscall` sleeps on to the same deadline.
fn relative_sleep(c: &SysCtx<'_>, deadline: u64, rmtp: u64) -> i64 {
    match sleep_until(c, deadline) {
        None => 0,
        Some(left) => {
            if rmtp != 0 && put_ts(c, rmtp, left) != 0 {
                return -EFAULT;
            }
            if c.proc.has_exited() {
                return -EINTR;
            }
            c.thread.set_sleep_restart(deadline, rmtp);
            -uapi::ab::siginfo::ERESTART_RESTARTBLOCK
        }
    }
}

/// `restart_syscall()` (219): the restart block of an interrupted sleep or timed futex wait, else EINTR
/// (`do_no_restart_syscall`).
fn sys_restart_syscall(c: &mut SysCtx<'_>) -> i64 {
    match c.thread.take_restart() {
        Some(proc::Restart::Sleep { deadline, rmtp }) => relative_sleep(c, deadline, rmtp),
        Some(proc::Restart::Futex { uaddr, shared, val, bitset, deadline, realtime }) => crate::futex::restart_wait(c.proc, uaddr, shared, val, bitset, deadline, realtime),
        None => -EINTR,
    }
}
crate::syscall_nr!(uapi::ab::nr_more::RESTART_SYSCALL, "restart_syscall", sys_restart_syscall);

crate::syscall!(NANOSLEEP, sys_nanosleep);

fn sys_clock_nanosleep(c: &mut SysCtx<'_>) -> i64 {
    let [which, flags, rqtp, rmtp, ..] = c.args;
    let id = which as i32;
    let Some(clock) = decode(id) else { return -EINVAL };
    if !can_sleep(id) {
        return -EOPNOTSUPP;
    }
    let t = match get_ts(c, rqtp) {
        Ok(t) => t,
        Err(e) => return e,
    };
    let Some(ns) = valid_ts(&t) else { return -EINVAL };
    let abs = flags as u32 as u64 & TIMER_ABSTIME != 0;
    let deadline = match clock {
        // `alarm_timer_nsleep`: only TIMER_ABSTIME in flags, then CAP_WAKE_ALARM.
        Clock::Alarm { .. } if flags as u32 as u64 & !TIMER_ABSTIME != 0 => return -EINVAL,
        Clock::Alarm { .. } if !c.proc.capable(proc::cred::Cap::WakeAlarm) => return -EPERM,
        Clock::Alarm { real: true } if abs => realtime_deadline(ns, realtime_ns(), time::now_ns()),
        Clock::Alarm { .. } if abs => ns,
        Clock::Alarm { .. } => time::now_ns().saturating_add(ns),
        // SC-6: CPU-time sleeps (posix_cpu_nsleep).
        Clock::Cpu { pid, thread, which } => return crate::cputime_sys::cpu_clock_sleep(c, pid, thread, which, flags as u32 as u64, ns, rmtp),
        Clock::Monotonic if abs => ns,
        // REALTIME is the RTC wall clock (ns since 1970, review INT-A1d user 2): an absolute REALTIME time is
        // turned into a monotonic deadline once, at the call; one already past returns at once.
        Clock::Realtime if abs => realtime_deadline(ns, realtime_ns(), time::now_ns()),
        Clock::Monotonic | Clock::Realtime => time::now_ns().saturating_add(ns),
    };
    if !abs {
        return relative_sleep(c, deadline, rmtp);
    }
    // An absolute sleep neither writes the time left nor keeps a restart block: it restarts as it was
    // (ERESTARTNOHAND, `hrtimer_nanosleep`).
    match sleep_until(c, deadline) {
        None => 0,
        Some(_) if c.proc.has_exited() => -EINTR,
        Some(_) => -uapi::ab::siginfo::ERESTARTNOHAND,
    }
}
crate::syscall!(CLOCK_NANOSLEEP, sys_clock_nanosleep);

/// The monotonic deadline of absolute wall-clock time `at` when the wall clock reads `real_now` and the
/// monotonic clock `mono_now` (pure, host-tested).
pub(crate) fn realtime_deadline(at: u64, real_now: u64, mono_now: u64) -> u64 {
    mono_now.saturating_add(at.saturating_sub(real_now))
}

fn sys_gettimeofday(c: &mut SysCtx<'_>) -> i64 {
    let [tv, tz, ..] = c.args;
    if tv != 0 {
        let ns = realtime_ns();
        let mut b = [0u8; 16];
        b[0..8].copy_from_slice(&(ns / NSEC).to_le_bytes());
        b[8..16].copy_from_slice(&((ns % NSEC) / 1000).to_le_bytes());
        if c.proc.mm.copy_to_user(tv, &b).is_err() {
            return -EFAULT;
        }
    }
    // sys_tz: minutes west 0, dst 0 (nothing sets it in V2).
    if tz != 0 && c.proc.mm.copy_to_user(tz, &[0u8; 8]).is_err() {
        return -EFAULT;
    }
    0
}
crate::syscall!(GETTIMEOFDAY, sys_gettimeofday);

fn sys_time(c: &mut SysCtx<'_>) -> i64 {
    let s = realtime_ns() / NSEC;
    if c.args[0] != 0 && c.proc.mm.write_u64(c.args[0], s).is_err() {
        return -EFAULT;
    }
    s as i64
}
crate::syscall_nr!(uapi::ab::nr_more::TIME, "time", sys_time);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_ids_as_linux() {
        assert_eq!(decode(0), Some(Clock::Realtime));
        assert_eq!(decode(1), Some(Clock::Monotonic));
        assert_eq!(decode(2), Some(Clock::Cpu { pid: 0, thread: false, which: 2 }));
        assert_eq!(decode(3), Some(Clock::Cpu { pid: 0, thread: true, which: 2 }));
        for id in [4, 6, 7] {
            assert_eq!(decode(id), Some(Clock::Monotonic), "{id}");
        }
        assert_eq!(decode(5), Some(Clock::Realtime));
        assert_eq!(decode(8), Some(Clock::Alarm { real: true }));
        assert_eq!(decode(9), Some(Clock::Alarm { real: false }));
        assert_eq!(decode(10), None, "CLOCK_SGI_CYCLE is gone");
        assert_eq!(decode(11), Some(Clock::Realtime));
        assert_eq!(decode(12), None);
        // glibc's clock_getcpuclockid(pid 0): MAKE_PROCESS_CPUCLOCK(0, CPUCLOCK_SCHED) = -6; thread clock -2.
        assert_eq!(decode(-6), Some(Clock::Cpu { pid: 0, thread: false, which: 2 }));
        assert_eq!(decode(-2), Some(Clock::Cpu { pid: 0, thread: true, which: 2 }));
        // PROF and VIRT clocks of the caller's process (SC-6).
        assert_eq!(decode(-8), Some(Clock::Cpu { pid: 0, thread: false, which: 0 }));
        assert_eq!(decode(-7), Some(Clock::Cpu { pid: 0, thread: false, which: 1 }));
        // pid 100's process clock: ~100 << 3 | 2.
        assert_eq!(decode((!100i32 << 3) | 2), Some(Clock::Cpu { pid: 100, thread: false, which: 2 }));
        assert_eq!(decode(-1), None, "CLOCKFD");
        assert_eq!(decode(-5), None, "type 3");
        assert!(can_sleep(0) && can_sleep(1) && can_sleep(7) && can_sleep(11) && can_sleep(2));
        assert!(!can_sleep(3) && !can_sleep(4) && !can_sleep(5) && !can_sleep(6));
    }

    #[test]
    fn timespec_validity() {
        let ts = |sec, nsec| Timespec { sec, nsec };
        assert_eq!(valid_ts(&ts(0, 0)), Some(0));
        assert_eq!(valid_ts(&ts(1, 999_999_999)), Some(1_999_999_999));
        assert_eq!(valid_ts(&ts(-1, 0)), None);
        assert_eq!(valid_ts(&ts(0, -1)), None);
        assert_eq!(valid_ts(&ts(0, 1_000_000_000)), None);
        assert_eq!(valid_ts(&ts(i64::MAX, 0)), Some(u64::MAX));
    }

    #[test]
    fn absolute_realtime_sleeps_end_on_the_wall_clock() {
        let real = 1_760_000_000 * 1_000_000_000u64; // October 2025 in ns since 1970
        let mono = 72 * 1_000_000_000u64; // 72 s after boot
        assert_eq!(realtime_deadline(real + 1_000_000_000, real, mono), mono + 1_000_000_000, "one second from now");
        assert_eq!(realtime_deadline(real - 5, real, mono), mono, "already past: at once");
        assert_eq!(realtime_deadline(u64::MAX, real, mono), mono + (u64::MAX - real));
    }
}
