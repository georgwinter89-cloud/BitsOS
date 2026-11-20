//! `perf_event_open(2)` (linux-7.3.1/kernel/events/core.c:13879), its checks in Linux's order.

use super::event::{Event, Kind, Target, file_for, same_clock};
use super::{EVENTS, allow_cpu, allow_kernel, perfmon_capable};
use crate::SysCtx;
use alloc::sync::Arc;
use core::sync::atomic::Ordering;
use perf_abi::attr::{self, AttrError, flag};
use perf_abi::*;
use uapi::errno::*;

const O_RDWR: u64 = 2;
const CLOCK_REALTIME: i32 = 0;
const CLOCK_MONOTONIC: i32 = 1;
const CLOCK_MONOTONIC_RAW: i32 = 4;
const CLOCK_BOOTTIME: i32 = 7;
const CLOCK_TAI: i32 = 11;

/// `perf_copy_attr`: the size word, the bytes it names, Linux's E2BIG with the kernel's size written back.
fn copy_attr(c: &SysCtx<'_>, uattr: u64) -> Result<attr::Attr, i64> {
    let mm = &c.proc.mm;
    copy_attr_with(|at, b| mm.copy_from_user(b, at).is_ok(), |at, b| mm.copy_to_user(at, b).is_ok(), uattr)
}

/// `perf_copy_attr` through any view of the caller's memory (open, and the MODIFY_ATTRIBUTES ioctl).
pub(crate) fn copy_attr_with(read: impl Fn(u64, &mut [u8]) -> bool, write: impl Fn(u64, &[u8]) -> bool, uattr: u64) -> Result<attr::Attr, i64> {
    let mut sz = [0u8; 4];
    if !read(uattr.wrapping_add(4), &mut sz) {
        return Err(-EFAULT);
    }
    let too_big = || {
        let _ = write(uattr.wrapping_add(4), &(attr::SIZE as u32).to_le_bytes());
        -E2BIG
    };
    let size = attr::Attr::effective_size(u32::from_le_bytes(sz), 4096).map_err(|_| too_big())? as usize;
    let mut b = alloc::vec![0u8; size];
    if !read(uattr, &mut b) {
        return Err(-EFAULT);
    }
    match attr::Attr::parse(&b) {
        Ok(a) => Ok(a),
        Err(AttrError::TooBig) => Err(too_big()),
        Err(AttrError::Invalid) => Err(-EINVAL),
    }
}

fn sys_perf_event_open(c: &mut SysCtx<'_>) -> i64 {
    match open(c) {
        Ok(fd) => fd,
        Err(e) => e,
    }
}
crate::syscall_nr!(298, "perf_event_open", sys_perf_event_open);

fn open(c: &mut SysCtx<'_>) -> Result<i64, i64> {
    let (uattr, pid, cpu, group_fd, flags) = (c.args[0], c.args[1] as i32, c.args[2] as i32, c.args[3] as i32, c.args[4]);
    if flags & !FLAG_ALL != 0 {
        return Err(-EINVAL);
    }
    let a = copy_attr(c, uattr)?;
    let p = c.proc.clone();
    let kernel = !a.has(flag::EXCLUDE_KERNEL) || a.has(flag::TEXT_POKE) || (a.sample_type & SAMPLE_CALLCHAIN != 0 && !a.has(flag::EXCLUDE_CALLCHAIN_KERNEL));
    if kernel && !allow_kernel(&p) {
        return Err(-EACCES);
    }
    if a.has(flag::NAMESPACES) && !perfmon_capable(&p) {
        return Err(-EACCES);
    }
    if a.has(flag::FREQ) {
        if a.sample > super::MAX_SAMPLE_RATE.load(Ordering::Relaxed) {
            return Err(-EINVAL);
        }
    } else if a.sample & (1 << 63) != 0 {
        return Err(-EINVAL);
    }
    if a.sample_type & SAMPLE_PHYS_ADDR != 0 && !allow_kernel(&p) {
        return Err(-EACCES);
    }
    if flags & FLAG_PID_CGROUP != 0 && (pid == -1 || cpu == -1) {
        return Err(-EINVAL);
    }
    let (mut output, mut leader) = (None, None);
    if group_fd != -1 {
        let f = p.fds.get(group_fd as u32 as usize).map_err(|_| -EBADF)?;
        let l = super::event::of(&f).ok_or(-EBADF)?;
        if l.hup.load(Ordering::Acquire) {
            return Err(-ENODEV);
        }
        if flags & FLAG_FD_OUTPUT != 0 {
            output = Some(l.clone());
        }
        if flags & FLAG_FD_NO_GROUP == 0 {
            leader = Some(l);
        }
    }
    let task = if pid != -1 && flags & FLAG_PID_CGROUP == 0 { Some(find_task(c, pid)?) } else { None };
    // perf_event_alloc.
    let ncpu = hal::percpu::cpu_count();
    if cpu < -1 || cpu >= ncpu as i32 || (cpu == -1 && task.is_none()) {
        return Err(-EINVAL);
    }
    if a.has(flag::SIGTRAP) && task.is_none() {
        return Err(-EINVAL);
    }
    if a.has(flag::INHERIT) && a.sample_type & SAMPLE_READ != 0 && a.sample_type & SAMPLE_TID == 0 {
        return Err(-EINVAL);
    }
    let kind = init_event(&a)?;
    if a.has(flag::AUX_OUTPUT) {
        return Err(-EOPNOTSUPP);
    }
    let max_stack = if a.sample_max_stack == 0 { super::MAX_STACK.load(Ordering::Relaxed) } else { a.sample_max_stack as u64 };
    if a.sample_type & SAMPLE_CALLCHAIN != 0 && max_stack > super::MAX_STACK.load(Ordering::Relaxed) {
        return Err(-EOVERFLOW);
    }
    if flags & FLAG_PID_CGROUP != 0 {
        // No cgroup events (a Linux without CONFIG_CGROUP_PERF answers the same).
        return Err(-EINVAL);
    }
    let clock_off = clock_offset(&a)?;
    // `perf_check_permission` for a task, `find_get_context`'s `perf_allow_cpu` and the online check for a CPU:
    // before the group's rules, as Linux.
    if let Some((th, _)) = &task {
        if !perfmon_capable(&p) {
            let target = th.group().image().ok_or(-ESRCH)?;
            if !p.may_ptrace(&target, false) {
                return Err(-EACCES);
            }
        }
    }
    if task.is_none() {
        if !allow_cpu(&p) {
            return Err(-EACCES);
        }
        if !hal::smp::is_online(cpu as usize) && cpu as usize != hal::percpu::this_cpu() {
            return Err(-ENODEV);
        }
    }
    // The group's rules (`perf_event_open` after the context): the leader leads its own group, the same clock,
    // the same CPU, the same task; only a leader may be pinned or exclusive.
    if let Some(l) = &leader {
        let same_target = match (&l.target, &task) {
            (Target::Cpu(_), None) => true,
            (Target::Thread { tid, .. }, Some((_, t))) => tid == t,
            _ => false,
        };
        if l.leader.is_some() || !same_clock(&l.attr, &a) || l.cpu() != (cpu >= 0).then_some(cpu as usize) || !same_target || a.has(flag::PINNED) || a.has(flag::EXCLUSIVE) {
            return Err(-EINVAL);
        }
    }
    // The sample fields and features V2 does not fill: refused rather than answered with zeros.
    if a.sample_type & !SAMPLE_SUPPORTED != 0 || a.has(flag::BUILD_ID) || a.has(flag::INHERIT) || a.has(flag::SIGTRAP) || a.has(flag::CONTEXT_SWITCH) || a.has(flag::NAMESPACES) || a.has(flag::WRITE_BACKWARD) {
        return Err(-EOPNOTSUPP);
    }
    let period = sample_period_ns(&a);
    let target = match task {
        Some((thread, tid)) => Target::Thread { thread, tid, cpu: (cpu >= 0).then_some(cpu as usize) },
        None => Target::Cpu(cpu as usize),
    };
    let spread = matches!(target, Target::Thread { cpu: None, .. }) && !perfmon_capable(&p);
    let ev = Event::new(a, kind, target, period, max_stack, clock_off, leader);
    ev.spread.store(spread, Ordering::Relaxed);
    if let Some(o) = &output
        && let Err(e) = ev.set_output(Some(o))
    {
        ev.close();
        return Err(-e);
    }
    let file = match file_for(ev.clone()) {
        Ok(f) => f,
        Err(e) => {
            ev.close();
            return Err(-e);
        }
    };
    if let Err(e) = ev.publish() {
        ev.close();
        return Err(-e);
    }
    EVENTS.inc();
    let of = vfs::OpenFile::anon(file, O_RDWR);
    p.fds.install_flags(of, 0, flags & FLAG_FD_CLOEXEC != 0).map(|fd| fd as i64).map_err(|e| -e)
}

/// `find_lively_task_by_vpid`: 0 is the caller; else the thread with that id in the caller's pid namespace.
fn find_task(c: &SysCtx<'_>, pid: i32) -> Result<(Arc<proc::Thread>, u32), i64> {
    if pid == 0 {
        return Ok((c.thread.clone(), c.thread.tid()));
    }
    if pid < 0 {
        return Err(-ESRCH);
    }
    let global = c.proc.pid_ns.id(pid as u32).ok_or(-ESRCH)?;
    let t = proc::find_thread(global).ok_or(-ESRCH)?;
    Ok((t, global))
}

/// `perf_init_event` for the events V2 has: the two clocks and DUMMY of PERF_TYPE_SOFTWARE; ENOENT for every
/// other type or config (no PMU claims it), EOPNOTSUPP for branch sampling on a software event.
fn init_event(a: &attr::Attr) -> Result<Kind, i64> {
    if a.kind != PERF_TYPE_SOFTWARE {
        return Err(-ENOENT);
    }
    let k = match a.config {
        SW_CPU_CLOCK => Kind::CpuClock,
        SW_TASK_CLOCK => Kind::TaskClock,
        SW_DUMMY => Kind::Dummy,
        _ => return Err(-ENOENT),
    };
    if a.sample_type & SAMPLE_BRANCH_STACK != 0 {
        return Err(-EOPNOTSUPP);
    }
    Ok(k)
}

/// `perf_event_set_clock`: the clocks Linux takes, as an offset from V2's monotonic clock (REALTIME and TAI: the
/// wall clock's offset now; V2 does not suspend, so BOOTTIME is MONOTONIC).
fn clock_offset(a: &attr::Attr) -> Result<u64, i64> {
    if !a.has(flag::USE_CLOCKID) {
        return Ok(0);
    }
    match a.clockid {
        CLOCK_MONOTONIC | CLOCK_MONOTONIC_RAW | CLOCK_BOOTTIME => Ok(0),
        CLOCK_REALTIME | CLOCK_TAI => Ok(crate::clock::realtime_offset_ns() as u64),
        _ => Err(-EINVAL),
    }
}

/// `perf_swevent_init_hrtimer`: a frequency becomes the period NSEC_PER_SEC / freq; 0 = a counting event.
fn sample_period_ns(a: &attr::Attr) -> u64 {
    if a.has(flag::FREQ) {
        if a.sample == 0 { 0 } else { 1_000_000_000 / a.sample }
    } else {
        a.sample
    }
}
