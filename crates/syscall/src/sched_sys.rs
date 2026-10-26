//! SC-6 syscall glue (granted by the SC-6 brief): affinity, priorities and policies, getcpu. The objects are
//! `sched::attr`'s; the rules and error order are Linux's:
//! - `sched_setaffinity`/`sched_getaffinity` (linux-6.6/kernel/sched/core.c:8448, :8500; `get_user_cpu_mask`
//!   :8436, `__set_cpus_allowed_ptr_locked` :3106): masks of `cpumask_size()` bytes = `nr_cpu_ids` (V2: the
//!   possible CPUs) rounded up to longs;
//! - `getcpu` (kernel/sys.c:2730); node 0 (one NUMA node);
//! - `setpriority`/`getpriority` (kernel/sys.c:218, :288; `set_one_prio` :195, `can_nice` core.c:7266):
//!   nice -20..19 clamped, EPERM for another owner's process, EACCES for lowering nice beyond RLIMIT_NICE;
//!   the answer of getpriority is `20 - nice` (1..40);
//! - `sched_setscheduler`/`sched_setparam`/`sched_getscheduler`/`sched_getparam`/`sched_get_priority_max|min`
//!   (core.c:8052-8170, `__sched_setscheduler` :7620, `user_check_sched_setscheduler` :7559, :9039, :9066):
//!   SCHED_OTHER, SCHED_BATCH and SCHED_IDLE are accepted (SC-7a: SCHED_IDLE runs in the Background class);
//!   SCHED_FIFO/RR pass Linux's argument checks and then run in the Interactive class when the thread's process
//!   holds an Interactive grant (`sched::interactive`: compositor, input path, the program in front; root alone
//!   is not enough, B4 "never self-raised"); without one only as Linux allows with RLIMIT_RTPRIO 0 (same RT
//!   policy, priority not raised), else EPERM; SCHED_DEADLINE fails Linux's own parameter check (EINVAL);
//!   SCHED_EXT (7, 6.12+, on CachyOS the ABI diff's oracle) is accepted and runs as SCHED_OTHER.
//!   `sched_getparam` reports the RT priority the program set.
//!
//! "pid" is Linux's task id: 0 = the calling thread, any other number a thread of any process (AB-5 on AB-4's
//! threads: `task`). Credentials are V2's constant uid (euid = uid); CAP_SYS_NICE = uid 0.
//! B3: changing another process's attributes asks `policy::check` with `Access::Signal { signal: 0 }` (the
//! kill(pid, 0) probe: "may act on that process"), until PM adds a variant of its own (change request).

use crate::SysCtx;
use alloc::sync::Arc;
use alloc::vec::Vec;
use proc::Process;
use sched::attr::{self, AttrError, MAX_NICE, MIN_NICE, Policy, SchedError};
use uapi::ab::rlimit::RLIMIT_NICE;
use uapi::errno::*;

/// Numbers `uapi::nr` does not name yet (Linux x86-64; desk change request to add them there).
pub(crate) mod nr {
    pub const GETRUSAGE: usize = 98;
    pub const TIMES: usize = 100;
    pub const SCHED_SETPARAM: usize = 142;
    pub const SCHED_GETPARAM: usize = 143;
    pub const SCHED_GETSCHEDULER: usize = 145;
    pub const SCHED_GET_PRIORITY_MAX: usize = 146;
    pub const SCHED_GET_PRIORITY_MIN: usize = 147;
    pub const IOPRIO_SET: usize = 251;
    pub const IOPRIO_GET: usize = 252;
}

pub(crate) const SCHED_NORMAL: i32 = 0;
pub(crate) const SCHED_FIFO: i32 = 1;
pub(crate) const SCHED_RR: i32 = 2;
pub(crate) const SCHED_BATCH: i32 = 3;
pub(crate) const SCHED_IDLE: i32 = 5;
pub(crate) const SCHED_DEADLINE: i32 = 6;
/// sched_ext (Linux 6.12+, CachyOS): accepted, runs as SCHED_OTHER (no BPF scheduler in V2).
pub(crate) const SCHED_EXT: i32 = 7;
pub(crate) const SCHED_RESET_ON_FORK: i32 = 0x4000_0000;
/// `sched_setparam`'s "keep the policy" (`SETPARAM_POLICY`).
const SETPARAM_POLICY: i32 = -1;
const MAX_RT_PRIO: u32 = 100;
pub(crate) const PRIO_PROCESS: i32 = 0;
pub(crate) const PRIO_PGRP: i32 = 1;
pub(crate) const PRIO_USER: i32 = 2;

/// Linux `find_process_by_pid` / `find_task_by_vpid`: 0 = the calling thread, else the live thread with that
/// id (any thread of any process, AB-4), with its process's image and its scheduler id. ESRCH for a negative
/// id, an unknown or ended thread, or one not started yet.
pub(crate) fn task(c: &SysCtx<'_>, pid: i32) -> Result<(Arc<Process>, u64), i64> {
    if pid < 0 {
        return Err(-ESRCH);
    }
    let (p, k) = if pid == 0 {
        (c.proc.clone(), c.thread.kthread())
    } else {
        let t = proc::find_thread(pid as u32).ok_or(-ESRCH)?;
        let p = t.group().image().ok_or(-ESRCH)?;
        (p, t.kthread())
    };
    if k == 0 || p.is_finished() {
        return Err(-ESRCH);
    }
    Ok((p, k))
}

/// `check_same_owner` / `set_one_prio_perm` (linux-6.6/kernel/sched/core.c, kernel/sys.c:203): the caller's
/// euid is the target's real or effective uid.
fn same_owner(c: &SysCtx<'_>, t: &Process) -> bool {
    let (me, t) = (c.proc.cred(), t.cred());
    me.euid == t.uid || me.euid == t.euid
}

/// CAP_SYS_NICE (`proc::cred::capable`).
fn capable(c: &SysCtx<'_>) -> bool {
    c.proc.capable(proc::cred::Cap::SysNice)
}

/// B3 for changes to another process (see the module comment).
fn policy_allows(c: &SysCtx<'_>, t: &Process) -> bool {
    if t.pid == c.proc.pid {
        return true;
    }
    match policy::check(&c.proc.subject(), &policy::Access::Signal { target_pid: t.pid, signal: 0 }) {
        Ok(()) => true,
        Err(d) => {
            klog::event!(Info, "sched", "attr.refused", pid = c.proc.pid, target = t.pid, reason = d.reason.as_str());
            false
        }
    }
}

fn attr_err(e: AttrError) -> i64 {
    match e {
        AttrError::NoThread => -ESRCH,
        AttrError::Invalid => -EINVAL,
        AttrError::NoMemory => -ENOMEM,
    }
}

/// `cpumask_size()`: `nr_cpu_ids` bits rounded up to longs, in bytes.
pub(crate) fn cpumask_size() -> u64 {
    attr::mask_words(hal::percpu::possible_cpus()) as u64 * 8
}

fn words_of(bytes: &[u8]) -> Vec<u64> {
    bytes.chunks(8).map(|ch| {
        let mut b = [0u8; 8];
        b[..ch.len()].copy_from_slice(ch);
        u64::from_le_bytes(b)
    }).collect()
}

fn sys_sched_setaffinity(c: &mut SysCtx<'_>) -> i64 {
    let [pid, len, ptr, ..] = c.args;
    let size = cpumask_size();
    // get_user_cpu_mask: a short mask is zero-extended, a long one cut to cpumask_size(); the copy comes first.
    let n = (len as u32 as u64).min(size) as usize;
    let mut bytes = Vec::new();
    if bytes.try_reserve_exact(size as usize).is_err() {
        return -ENOMEM;
    }
    bytes.resize(size as usize, 0u8);
    if n > 0 && c.proc.mm.copy_from_user(&mut bytes[..n], ptr).is_err() {
        return -EFAULT;
    }
    let (t, tid) = match task(c, pid as i32) {
        Ok(x) => x,
        Err(e) => return e,
    };
    if (!same_owner(c, &t) && !capable(c)) || !policy_allows(c, &t) {
        return -EPERM;
    }
    attr::set_affinity(tid, &words_of(&bytes)).map(|_| 0).unwrap_or_else(attr_err)
}
crate::syscall!(SCHED_SETAFFINITY, sys_sched_setaffinity);

/// `len` must hold `nr_cpu_ids` bits and be a multiple of 8; the answer is `min(len, cpumask_size())` bytes of
/// the thread's mask ANDed with the online CPUs. No permission check (Linux has none but LSMs).
fn sys_sched_getaffinity(c: &mut SysCtx<'_>) -> i64 {
    let [pid, len, ptr, ..] = c.args;
    // `unsigned int len` and `len * BITS_PER_BYTE` in 32 bits, as Linux (core.c:8500): a length of 2^29 or
    // more wraps there and fails the first check.
    let len = len as u32;
    let nr_cpu_ids = hal::percpu::possible_cpus().max(1) as u32;
    if len.wrapping_mul(8) < nr_cpu_ids || len & 7 != 0 {
        return -EINVAL;
    }
    let len = len as u64;
    let (_, tid) = match task(c, pid as i32) {
        Ok(x) => x,
        Err(e) => return e,
    };
    let Some(mask) = attr::affinity(tid) else { return -ESRCH };
    let mut bytes = Vec::new();
    if bytes.try_reserve_exact(mask.len() * 8).is_err() {
        return -ENOMEM;
    }
    for w in &mask {
        bytes.extend_from_slice(&w.to_le_bytes());
    }
    let n = len.min(cpumask_size()) as usize;
    c.proc.mm.copy_to_user(ptr, &bytes[..n]).map(|_| n as i64).unwrap_or(-EFAULT)
}
crate::syscall!(SCHED_GETAFFINITY, sys_sched_getaffinity);

/// `getcpu(cpu*, node*, cache)`: both written when given; any fault is EFAULT (Linux ORs the put_user errors).
fn sys_getcpu(c: &mut SysCtx<'_>) -> i64 {
    let [cpup, nodep, ..] = c.args;
    let cpu = sched::current_cpu() as u32;
    let mut bad = false;
    if cpup != 0 {
        bad |= c.proc.mm.copy_to_user(cpup, &cpu.to_le_bytes()).is_err();
    }
    if nodep != 0 {
        bad |= c.proc.mm.copy_to_user(nodep, &0u32.to_le_bytes()).is_err();
    }
    if bad { -EFAULT } else { 0 }
}
crate::syscall!(GETCPU, sys_getcpu);

// ---------------------------------------------------------------------------------------------------------
// Priorities

/// RLIMIT_NICE's soft limit of `t` (Linux `task_rlimit(p, RLIMIT_NICE)`).
fn rlimit_nice(t: &Process) -> u64 {
    t.limits.get(RLIMIT_NICE).map_or(0, |r| r.cur)
}

/// `is_nice_reduction`: nice `nice` as an rlimit value (20 - nice, 1..40) within `t`'s RLIMIT_NICE.
fn is_nice_reduction(t: &Process, nice: i32) -> bool {
    (20 - nice) as u64 <= rlimit_nice(t)
}

/// `set_one_prio`: EPERM for another owner, EACCES for lowering nice without the right; `error` stays unless it
/// was ESRCH (then 0).
fn set_one_prio(c: &SysCtx<'_>, t: &Process, tid: u64, nice: i32, error: i64) -> i64 {
    if (!same_owner(c, t) && !capable(c)) || !policy_allows(c, t) {
        return -EPERM;
    }
    let Some(cur) = attr::nice(tid) else { return error };
    if nice < cur && !(is_nice_reduction(t, nice) || capable(c)) {
        return -EACCES;
    }
    if attr::set_nice(tid, nice).is_err() {
        return error;
    }
    if error == -ESRCH { 0 } else { error }
}

/// The live threads `setpriority`/`getpriority` name, with their processes: PRIO_PROCESS one thread
/// (`find_task_by_vpid`), PRIO_PGRP every thread of the group's processes (`do_each_pid_thread`), PRIO_USER every
/// thread of the uid's processes (`for_each_process_thread`).
fn prio_targets(c: &SysCtx<'_>, which: i32, who: i32) -> Vec<(Arc<Process>, u64)> {
    let all_threads = |keep: &dyn Fn(&Process) -> bool| -> Vec<(Arc<Process>, u64)> {
        let mut v = Vec::new();
        for p in proc::processes().into_iter().filter(|p| !p.is_finished() && keep(p)) {
            for t in p.threads() {
                let k = t.kthread();
                if k != 0 && v.try_reserve(1).is_ok() {
                    v.push((p.clone(), k));
                }
            }
        }
        v
    };
    match which {
        PRIO_PROCESS => task(c, who).into_iter().collect(),
        PRIO_PGRP => {
            if who < 0 {
                return Vec::new();
            }
            let pgrp = if who == 0 { c.proc.pgid() } else { who as u32 };
            all_threads(&|p| p.pgid() == pgrp)
        }
        _ => {
            // PRIO_USER: the real uid (`task_uid`), the caller's own when `who` is 0.
            let uid = if who == 0 { c.proc.cred().uid } else { who as u32 };
            all_threads(&|p| p.real_uid() == uid)
        }
    }
}

fn sys_setpriority(c: &mut SysCtx<'_>) -> i64 {
    let which = c.args[0] as i32;
    let who = c.args[1] as i32;
    let nice = (c.args[2] as i32).clamp(MIN_NICE, MAX_NICE);
    if !(PRIO_PROCESS..=PRIO_USER).contains(&which) {
        return -EINVAL;
    }
    let mut error = -ESRCH;
    for (p, tid) in prio_targets(c, which, who) {
        error = set_one_prio(c, &p, tid, nice, error);
    }
    error
}
crate::syscall!(SETPRIORITY, sys_setpriority);

/// The highest `20 - nice` among the named threads, or ESRCH.
fn sys_getpriority(c: &mut SysCtx<'_>) -> i64 {
    let which = c.args[0] as i32;
    let who = c.args[1] as i32;
    if !(PRIO_PROCESS..=PRIO_USER).contains(&which) {
        return -EINVAL;
    }
    let mut ret = -ESRCH;
    for (_, tid) in prio_targets(c, which, who) {
        if let Some(n) = attr::nice(tid) {
            ret = ret.max(20 - n as i64);
        }
    }
    ret
}
crate::syscall!(GETPRIORITY, sys_getpriority);

// ---------------------------------------------------------------------------------------------------------
// Policies

fn valid_policy(p: i32) -> bool {
    matches!(p, SCHED_NORMAL | SCHED_FIFO | SCHED_RR | SCHED_BATCH | SCHED_IDLE | SCHED_DEADLINE | SCHED_EXT)
}

fn rt_policy(p: i32) -> bool {
    p == SCHED_FIFO || p == SCHED_RR
}

/// `_sched_setscheduler` + `__sched_setscheduler` with `user = true`; SCHED_FIFO/RR by grant (SC-7a).
/// `prio` is `sched_param.sched_priority` (an int, compared as `attr->sched_priority`, a u32).
pub(crate) fn setscheduler(c: &SysCtx<'_>, t: &Process, tid: u64, policy: i32, prio: i32) -> i64 {
    let Some((cur_policy, cur_reset)) = attr::policy(tid) else { return -ESRCH };
    let (policy, reset) = if policy == SETPARAM_POLICY {
        (cur_policy.linux(), cur_reset)
    } else {
        let reset = policy & SCHED_RESET_ON_FORK != 0;
        let p = policy & !SCHED_RESET_ON_FORK;
        if !valid_policy(p) {
            return -EINVAL;
        }
        (p, reset)
    };
    let prio = prio as u32;
    if prio > MAX_RT_PRIO - 1 {
        return -EINVAL;
    }
    // SCHED_DEADLINE through sched_setscheduler has no runtime/deadline: `__checkparam_dl` fails.
    if policy == SCHED_DEADLINE || rt_policy(policy) != (prio != 0) {
        return -EINVAL;
    }
    if rt_policy(policy) {
        // SC-7a: the Interactive grant stands for Linux's privilege (`sched::attr::rt_change_allowed`, checked under
        // the scheduler's lock); another owner or clearing reset-on-fork still needs CAP_SYS_NICE, B3 as below.
        if ((!same_owner(c, t) || (cur_reset && !reset)) && !capable(c)) || !policy_allows(c, t) {
            return -EPERM;
        }
        let new = if policy == SCHED_FIFO { Policy::Fifo } else { Policy::Rr };
        return match attr::set_scheduler(tid, new, prio as u8, reset) {
            Ok(()) => 0,
            Err(SchedError::NoThread) => -ESRCH,
            Err(SchedError::NotGranted) => {
                klog::event!(Debug, "sched", "rt.refused_for", pid = c.proc.pid, program = c.proc.name.as_str(), target = t.pid, policy = policy, priority = prio);
                -EPERM
            }
            Err(SchedError::Invalid) => -EINVAL,
        };
    }
    let Some(new) = Policy::from_linux(policy) else { return -EINVAL };
    // user_check_sched_setscheduler: the nice value does not change here; leaving SCHED_IDLE needs the right
    // to the current nice value; another owner, or clearing reset-on-fork, needs CAP_SYS_NICE.
    let cur_nice = attr::nice(tid).unwrap_or(0);
    let need_priv = (cur_policy == Policy::Idle && new != Policy::Idle && !is_nice_reduction(t, cur_nice)) || !same_owner(c, t) || (cur_reset && !reset);
    if (need_priv && !capable(c)) || !policy_allows(c, t) {
        return -EPERM;
    }
    if new == cur_policy && reset == cur_reset {
        return 0;
    }
    attr::set_policy(tid, new, reset).map(|_| 0).unwrap_or_else(attr_err)
}

/// `do_sched_setscheduler`: param NULL or pid < 0 is EINVAL, then the copy (EFAULT), then the lookup (ESRCH).
fn do_setscheduler(c: &SysCtx<'_>, pid: i32, policy: i32, param: u64) -> i64 {
    if param == 0 || pid < 0 {
        return -EINVAL;
    }
    let mut b = [0u8; 4];
    if c.proc.mm.copy_from_user(&mut b, param).is_err() {
        return -EFAULT;
    }
    let (t, tid) = match task(c, pid) {
        Ok(x) => x,
        Err(e) => return e,
    };
    setscheduler(c, &t, tid, policy, i32::from_le_bytes(b))
}

fn sys_sched_setscheduler(c: &mut SysCtx<'_>) -> i64 {
    let policy = c.args[1] as i32;
    if policy < 0 {
        return -EINVAL;
    }
    do_setscheduler(c, c.args[0] as i32, policy, c.args[2])
}
crate::syscall!(SCHED_SETSCHEDULER, sys_sched_setscheduler);

fn sys_sched_setparam(c: &mut SysCtx<'_>) -> i64 {
    do_setscheduler(c, c.args[0] as i32, SETPARAM_POLICY, c.args[1])
}
crate::syscall_nr!(nr::SCHED_SETPARAM, "sched_setparam", sys_sched_setparam);

fn sys_sched_getscheduler(c: &mut SysCtx<'_>) -> i64 {
    let pid = c.args[0] as i32;
    if pid < 0 {
        return -EINVAL;
    }
    let (_, tid) = match task(c, pid) {
        Ok(x) => x,
        Err(e) => return e,
    };
    match attr::policy(tid) {
        Some((p, reset)) => (p.linux() | if reset { SCHED_RESET_ON_FORK } else { 0 }) as i64,
        None => -ESRCH,
    }
}
crate::syscall_nr!(nr::SCHED_GETSCHEDULER, "sched_getscheduler", sys_sched_getscheduler);

/// The RT priority the program set (0 under a fair policy).
fn sys_sched_getparam(c: &mut SysCtx<'_>) -> i64 {
    let [pid, param, ..] = c.args;
    let pid = pid as i32;
    if param == 0 || pid < 0 {
        return -EINVAL;
    }
    let tid = match task(c, pid) {
        Ok((_, tid)) => tid,
        Err(e) => return e,
    };
    let Some(prio) = attr::rt_priority(tid) else { return -ESRCH };
    c.proc.mm.copy_to_user(param, &(prio as i32).to_le_bytes()).map(|_| 0).unwrap_or(-EFAULT)
}
crate::syscall_nr!(nr::SCHED_GETPARAM, "sched_getparam", sys_sched_getparam);

fn sys_sched_get_priority_max(c: &mut SysCtx<'_>) -> i64 {
    match c.args[0] as i32 {
        SCHED_FIFO | SCHED_RR => (MAX_RT_PRIO - 1) as i64,
        SCHED_DEADLINE | SCHED_NORMAL | SCHED_BATCH | SCHED_IDLE | SCHED_EXT => 0,
        _ => -EINVAL,
    }
}
crate::syscall_nr!(nr::SCHED_GET_PRIORITY_MAX, "sched_get_priority_max", sys_sched_get_priority_max);

fn sys_sched_get_priority_min(c: &mut SysCtx<'_>) -> i64 {
    match c.args[0] as i32 {
        SCHED_FIFO | SCHED_RR => 1,
        SCHED_DEADLINE | SCHED_NORMAL | SCHED_BATCH | SCHED_IDLE | SCHED_EXT => 0,
        _ => -EINVAL,
    }
}
crate::syscall_nr!(nr::SCHED_GET_PRIORITY_MIN, "sched_get_priority_min", sys_sched_get_priority_min);

// ---------------------------------------------------------------------------------------------------------
// I/O priorities (FS-12: the values live in `block::prio`, which maps them onto the request priorities)

/// `ioprio_set` (linux-6.6/block/ioprio.c:69): the value's check first (`ioprio_check_cap`; RT needs
/// CAP_SYS_ADMIN/CAP_SYS_NICE = root), then the targets as setpriority names them (IOPRIO_WHO_* = PRIO_* + 1);
/// another owner without the capability, or B3 refusing, is EPERM and stops the walk (`set_task_ioprio`).
fn sys_ioprio_set(c: &mut SysCtx<'_>) -> i64 {
    let (which, who, ioprio) = (c.args[0] as i32, c.args[1] as i32, c.args[2] as i32);
    if let Err(e) = block::prio::check(ioprio, capable(c)) {
        return e;
    }
    if !(block::prio::IOPRIO_WHO_PROCESS..=block::prio::IOPRIO_WHO_USER).contains(&which) {
        return -EINVAL;
    }
    let mut ret = -ESRCH;
    for (p, tid) in prio_targets(c, which - 1, who) {
        if (!same_owner(c, &p) && !capable(c)) || !policy_allows(c, &p) {
            return -EPERM;
        }
        if let Err(e) = block::prio::set(tid, ioprio) {
            return e;
        }
        ret = 0;
    }
    ret
}
crate::syscall_nr!(nr::IOPRIO_SET, "ioprio_set", sys_ioprio_set);

/// `ioprio_get` (linux-6.6/block/ioprio.c:210): IOPRIO_WHO_PROCESS the raw value (IOPRIO_DEFAULT when none),
/// PGRP/USER the best effective value of the named threads (`ioprio_best`), ESRCH when none.
fn sys_ioprio_get(c: &mut SysCtx<'_>) -> i64 {
    let (which, who) = (c.args[0] as i32, c.args[1] as i32);
    if !(block::prio::IOPRIO_WHO_PROCESS..=block::prio::IOPRIO_WHO_USER).contains(&which) {
        return -EINVAL;
    }
    let mut ret = -ESRCH;
    for (_, tid) in prio_targets(c, which - 1, who) {
        let v = if which == block::prio::IOPRIO_WHO_PROCESS { Some(block::prio::raw(tid)) } else { block::prio::task_ioprio(tid) };
        let Some(v) = v else { continue };
        ret = if ret == -ESRCH { v as i64 } else { block::prio::best(ret as i32, v) as i64 };
    }
    ret
}
crate::syscall_nr!(nr::IOPRIO_GET, "ioprio_get", sys_ioprio_get);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policies_and_masks() {
        for p in [0, 1, 2, 3, 5, 6, 7] {
            assert!(valid_policy(p), "{p}");
        }
        for p in [4, 8, -1, 0x4000_0000] {
            assert!(!valid_policy(p), "{p}");
        }
        assert!(rt_policy(1) && rt_policy(2) && !rt_policy(0) && !rt_policy(6));
        assert_eq!(words_of(&[1, 0, 0, 0, 0, 0, 0, 0x80]), alloc::vec![0x8000_0000_0000_0001]);
        assert_eq!(words_of(&[3; 12]), alloc::vec![0x0303_0303_0303_0303, 0x0303_0303]);
        assert!(words_of(&[]).is_empty());
    }
}
