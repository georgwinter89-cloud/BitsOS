//! Process syscalls: exit (one thread) and exit_group (AB-2, AB-4), wait4 and waitid (linux-6.6/kernel/exit.c
//! `kernel_wait4` :1745, `kernel_waitid` :1646, `SYSCALL_DEFINE5(waitid)` :1712), getppid, and the process
//! group and session calls (kernel/sys.c; the rules are in `proc::job`).

use crate::SysCtx;
use alloc::sync::Arc;
use proc::ExitStatus;
use proc::wait::Select;
use uapi::ab::wait::*;
use uapi::errno::*;

/// `exit(status)`: ends the calling thread; the last thread's end ends the process (its status is the
/// leader's exit code then, Linux `wait_task_zombie`).
fn sys_exit(c: &mut SysCtx<'_>) -> i64 {
    c.thread.exit_thread((c.args[0] & 0xFF) as u32);
    0
}
crate::syscall!(EXIT, sys_exit);

/// `exit_group(status)`: ends every thread of the process.
fn sys_exit_group(c: &mut SysCtx<'_>) -> i64 {
    c.proc.exit((c.args[0] & 0xFF) as i32);
    0
}
crate::syscall!(EXIT_GROUP, sys_exit_group);

fn opts_from(options: u64, sel: Select, exited: bool) -> proc::WaitOpts {
    proc::WaitOpts {
        sel,
        nohang: options & WNOHANG != 0,
        exited,
        // WUNTRACED (wait4) is WSTOPPED (waitid): the same bit.
        stopped: options & WUNTRACED != 0,
        continued: options & WCONTINUED != 0,
        nowait: options & WNOWAIT != 0,
        all: options & WALL != 0,
        clone: options & WCLONE != 0,
        nothread: options & WNOTHREAD != 0,
    }
}

/// `wait4(pid, wstatus, options, rusage)`. The child is collected before the status is written, so a bad
/// `wstatus` gives EFAULT with the child gone, as on Linux (`kernel_wait4`). rusage: the child's CPU time and
/// its reaped children's (SC-6, `cputime_sys::reaped_times`).
/// AB-6: WUNTRACED reports a stopped child once (`(sig << 8) | 0x7f`), WCONTINUED a continued one (0xffff).
fn sys_wait4(c: &mut SysCtx<'_>) -> i64 {
    let [pid, wstatus, options, rusage, ..] = c.args;
    let options = options as u32 as u64;
    if let Err(e) = proc::wait::check_options(options) {
        return -e;
    }
    let sel = match Select::from_linux(pid as i32) {
        Ok(s) => s,
        Err(e) => return -e,
    };
    let r = match proc::wait_for(c.proc, c.thread.tid(), &opts_from(options, sel, true)) {
        Ok(None) => return 0,
        Ok(Some(r)) => r,
        Err(e) => return -e,
    };
    if wstatus != 0 && c.proc.mm.copy_to_user(wstatus, &r.wait_status().to_le_bytes()).is_err() {
        return -EFAULT;
    }
    if rusage != 0 && c.proc.mm.copy_to_user(rusage, &crate::cputime_sys::rusage_bytes(crate::cputime_sys::reaped_times(&r), r.maxrss_pages * 4)).is_err() {
        return -EFAULT;
    }
    r.pid as i64
}
crate::syscall!(WAIT4, sys_wait4);

/// waitid's id and type: `Select`, whether the pidfd it came through is O_NONBLOCK and the pidfd's process
/// (held for the wait, so the address its selection names stays its own), or the errno
/// `kernel_waitid` gives first.
pub(crate) fn waitid_select(c: &SysCtx<'_>, which: u32, id: i32) -> Result<(Select, bool, Option<Arc<proc::Group>>), i64> {
    if which == P_PIDFD {
        // AB-12 (`pidfd_get_pid`): EBADF for a closed fd and for any file that is not a pidfd; the selection
        // is the process itself, so a reused pid never matches. A non-child is ECHILD, as on Linux.
        if id < 0 {
            return Err(-EINVAL);
        }
        let (g, status) = crate::pidfd::get(c, id)?;
        let sel = Select::Exact { pid: g.tgid, group: Arc::as_ptr(&g) as usize };
        return Ok((sel, status & uapi::ab::pidfd::PIDFD_NONBLOCK != 0, Some(g)));
    }
    waitid_plain(c, which, id).map(|s| (s, false, None))
}

fn waitid_plain(c: &SysCtx<'_>, which: u32, id: i32) -> Result<Select, i64> {
    match which {
        P_ALL => Ok(Select::Any),
        P_PID if id <= 0 => Err(-EINVAL),
        P_PID => Ok(Select::Pid(id as u32)),
        P_PGID if id < 0 => Err(-EINVAL),
        P_PGID if id == 0 => Ok(Select::Group(c.proc.pgid())),
        P_PGID => Ok(Select::Group(id as u32)),
        _ => Err(-EINVAL),
    }
}

/// The six `siginfo_t` fields waitid writes (si_signo, si_errno, si_code, si_pid, si_uid, si_status) for what
/// it found (`None`: WNOHANG and nothing ended: zeros).
pub(crate) fn waitid_info(r: Option<&proc::Reaped>) -> [(u64, u32); 6] {
    let (signo, code, pid, uid, status) = match r {
        None => (0, 0, 0, 0, 0),
        Some(r) => match (r.event, r.status) {
            (proc::WaitEvent::Stopped(s), _) => (uapi::ab::signal::SIGCHLD, CLD_STOPPED as u32, r.pid, r.uid, s),
            (proc::WaitEvent::Continued, _) => (uapi::ab::signal::SIGCHLD, CLD_CONTINUED as u32, r.pid, r.uid, uapi::ab::signal::SIGCONT),
            (proc::WaitEvent::Ended, ExitStatus::Exited(s)) => (uapi::ab::signal::SIGCHLD, CLD_EXITED as u32, r.pid, r.uid, s as u32),
            (proc::WaitEvent::Ended, ExitStatus::Signaled(s)) => (uapi::ab::signal::SIGCHLD, CLD_KILLED as u32, r.pid, r.uid, s),
        },
    };
    [(0, signo), (4, 0), (8, code), (16, pid), (20, uid), (24, status)]
}

/// `sizeof(siginfo_t)`.
const SIGINFO_SIZE: u64 = 128;

/// Linux `access_ok(infop, sizeof(siginfo_t))`: the range ends at or below the top of user space.
pub(crate) fn waitid_infop_ok(infop: u64) -> bool {
    infop.checked_add(SIGINFO_SIZE).is_some_and(|end| end <= hal::USER_TOP)
}

/// `waitid(which, id, infop, options, rusage)`.
fn sys_waitid(c: &mut SysCtx<'_>) -> i64 {
    let [which, id, infop, options, rusage, ..] = c.args;
    let options = options as u32 as u64;
    if options & !WAITID_VALID != 0 || options & (WEXITED | WSTOPPED | WCONTINUED) == 0 {
        return -EINVAL;
    }
    let (sel, nonblock, _held) = match waitid_select(c, which as u32, id as i32) {
        Ok(s) => s,
        Err(e) => return e,
    };
    // An O_NONBLOCK pidfd waits as WNOHANG; nothing to report then is EAGAIN unless the caller asked for
    // WNOHANG itself (linux-6.6/kernel/exit.c:1700-1705).
    let mut wopts = opts_from(options, sel, options & WEXITED != 0);
    wopts.nohang |= nonblock;
    let r = match proc::wait_for(c.proc, c.thread.tid(), &wopts) {
        Ok(None) if nonblock && options & WNOHANG == 0 => return -EAGAIN,
        Ok(r) => r,
        Err(e) => return -e,
    };
    // SC-6: the collected child's times, as wait4's (`wait_task_zombie` fills `ru` for both).
    if let Some(found) = &r
        && rusage != 0
        && c.proc.mm.copy_to_user(rusage, &crate::cputime_sys::rusage_bytes(crate::cputime_sys::reaped_times(found), found.maxrss_pages * 4)).is_err()
    {
        return -EFAULT;
    }
    if infop != 0 {
        // `user_write_access_begin(infop, sizeof(siginfo_t))`: the whole siginfo must lie in user space (a
        // hostile pointer near the top cannot wrap); then only the six fields are written, as Linux.
        if !waitid_infop_ok(infop) {
            return -EFAULT;
        }
        for (off, v) in waitid_info(r.as_ref()) {
            if c.proc.mm.copy_to_user(infop + off, &v.to_le_bytes()).is_err() {
                return -EFAULT;
            }
        }
    }
    0
}
crate::syscall_nr!(uapi::ab::nr_more::WAITID, "waitid", sys_waitid);

fn sys_getppid(c: &mut SysCtx<'_>) -> i64 {
    c.proc.ppid() as i64
}
crate::syscall!(GETPPID, sys_getppid);

fn sys_getpgrp(c: &mut SysCtx<'_>) -> i64 {
    c.proc.pgid() as i64
}
crate::syscall!(GETPGRP, sys_getpgrp);

/// `getpgid(pid)`: 0 = the caller; another pid (or a thread id) must exist (ESRCH otherwise).
fn sys_getpgid(c: &mut SysCtx<'_>) -> i64 {
    proc::job::getpgid(c.proc, c.args[0] as i32).map(|g| g as i64).unwrap_or_else(|e| -e)
}
crate::syscall!(GETPGID, sys_getpgid);

fn sys_setpgid(c: &mut SysCtx<'_>) -> i64 {
    proc::job::setpgid(c.proc, c.args[0] as i32, c.args[1] as i32).map(|_| 0).unwrap_or_else(|e| -e)
}
crate::syscall!(SETPGID, sys_setpgid);

fn sys_setsid(c: &mut SysCtx<'_>) -> i64 {
    proc::job::setsid(c.proc).map(|s| s as i64).unwrap_or_else(|e| -e)
}
crate::syscall!(SETSID, sys_setsid);

fn sys_getsid(c: &mut SysCtx<'_>) -> i64 {
    proc::job::getsid(c.proc, c.args[0] as i32).map(|s| s as i64).unwrap_or_else(|e| -e)
}
crate::syscall_nr!(uapi::ab::nr_more::GETSID, "getsid", sys_getsid);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waitid_writes_linux_siginfo_fields() {
        let r = proc::Reaped { pid: 42, status: ExitStatus::Exited(3), uid: 1000, times: Default::default(), maxrss_pages: 0, event: proc::WaitEvent::Ended };
        assert_eq!(waitid_info(Some(&r)), [(0, 17), (4, 0), (8, 1), (16, 42), (20, 1000), (24, 3)]);
        let k = proc::Reaped { pid: 7, status: ExitStatus::Signaled(9), uid: 0, times: Default::default(), maxrss_pages: 0, event: proc::WaitEvent::Ended };
        let st = proc::Reaped { pid: 7, status: ExitStatus::Exited(0), uid: 0, times: Default::default(), maxrss_pages: 0, event: proc::WaitEvent::Stopped(19) };
        assert_eq!(waitid_info(Some(&st))[2], (8, 5), "CLD_STOPPED");
        assert_eq!(waitid_info(Some(&st))[5], (24, 19));
        assert_eq!(st.wait_status(), 0x137f, "WIFSTOPPED, WSTOPSIG 19");
        let co = proc::Reaped { event: proc::WaitEvent::Continued, ..st };
        assert_eq!((waitid_info(Some(&co))[2], co.wait_status()), ((8, 6), 0xffff), "CLD_CONTINUED, WIFCONTINUED");
        assert_eq!(waitid_info(Some(&k))[2], (8, 2), "CLD_KILLED");
        assert_eq!(waitid_info(Some(&k))[5], (24, 9));
        assert!(waitid_info(None).iter().all(|&(_, v)| v == 0), "WNOHANG with nothing: zeros");
        assert!(!waitid_infop_ok(0xFFFF_FFFF_FFFF_FFFC), "a pointer that wraps is EFAULT, not a panic");
        assert!(!waitid_infop_ok(hal::USER_TOP - 64), "a siginfo crossing the top of user space");
        assert!(waitid_infop_ok(hal::USER_TOP - 128));
    }
}
