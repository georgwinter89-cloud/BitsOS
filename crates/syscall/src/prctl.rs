//! prctl(2) (AB-5; linux-6.6/kernel/sys.c `SYSCALL_DEFINE5(prctl)` :2408, security/commoncap.c
//! `cap_task_prctl`, security/yama `yama_task_prctl`). The state lives in `proc::prctl_state` (per task and
//! per address space). Every option of 6.6 answers as Linux does on x86-64 with V2's constant credentials
//! (uid 0 = all capabilities, others none), the LSM options first (commoncap, Yama), unknown ones EINVAL.
//!
//! Built: PR_SET/GET_PDEATHSIG (sent by proc when the parent thread or process ends), PR_SET/GET_DUMPABLE,
//! PR_SET/GET_NAME (the thread's comm), PR_SET/GET_NO_NEW_PRIVS, PR_SET/GET_CHILD_SUBREAPER (proc reparents
//! orphans to it), PR_SET/GET_TIMERSLACK, PR_SET/GET_THP_DISABLE, PR_MCE_KILL/_GET, PR_GET/SET_TIMING,
//! PR_GET/SET_TSC (always enabled), PR_GET_TID_ADDRESS, PR_GET_AUXV, PR_TASK_PERF_EVENTS_*, PR_GET_SECCOMP,
//! PR_CAPBSET_READ/DROP, PR_GET/SET_SECUREBITS, PR_GET/SET_KEEPCAPS, PR_CAP_AMBIENT, PR_SET_PTRACER,
//! PR_SET/GET_IO_FLUSHER, PR_GET_SPECULATION_CTRL/SET (V2 does no per-task speculation control: "not
//! affected" / ENXIO), PR_SET_SYSCALL_USER_DISPATCH off, PR_SET_MM_MAP_SIZE.
//! Refused like a kernel without the feature (EINVAL, logged once each): PR_SET_SECCOMP (SX lane),
//! PR_SET_SYSCALL_USER_DISPATCH on, PR_SET/GET_MDWE (needs MM to refuse W+X), PR_SET_VMA (anonymous VMA
//! names), PR_SCHED_CORE, PR_SET/GET_MEMORY_MERGE (no KSM); arch options of other CPUs EINVAL as on x86.

use crate::SysCtx;
use core::sync::atomic::{AtomicU64, Ordering};
use proc::prctl_state::{CAP_LAST_CAP, COMM_LEN, MCE_EARLY, MCE_PROCESS, comm_of};
use uapi::ab::prctl::*;
use uapi::errno::*;

/// `_NSIG`: `valid_signal` accepts 0..=64.
const NSIG: u64 = 64;
/// `SUID_DUMP_DISABLE`, `SUID_DUMP_USER`.
const SUID_DUMP_USER: u64 = 1;
/// The six securebits and their locks (`SECURE_ALL_BITS | SECURE_ALL_LOCKS`).
const SECURE_ALL_BITS: u64 = 0x15;
const SECURE_ALL_LOCKS: u64 = 0x2a;
const SECURE_KEEP_CAPS: u32 = 4;
const SECURE_KEEP_CAPS_LOCKED: u32 = 5;
/// `sizeof(mm->saved_auxv)`: AT_VECTOR_SIZE = 2 * (3 + 22 + 1) words on x86-64 with IA32 emulation (CachyOS).
const SAVED_AUXV_BYTES: usize = 2 * (3 + 22 + 1) * 8;
/// `sizeof(struct prctl_mm_map)` (PR_SET_MM_MAP_SIZE).
const PRCTL_MM_MAP_SIZE: u32 = 104;
const PR_SPEC_NOT_AFFECTED: i64 = 0;

/// Options refused as "not built" (one Info event per option, then Debug).
static REFUSED: AtomicU64 = AtomicU64::new(0);

fn not_built(c: &SysCtx<'_>, option: i32, bit: u32, what: &str) -> i64 {
    if REFUSED.fetch_or(1 << bit, Ordering::Relaxed) & (1 << bit) == 0 {
        klog::event!(Info, "syscall", "prctl.not_built", option = option, what = what, program = c.proc.name.as_str(); "answered EINVAL as a kernel without the feature (logged once)");
    }
    -EINVAL
}

/// `capable(cap)` (AB-13: `proc::cred::capable`, the one place).
fn root(c: &SysCtx<'_>, cap: proc::cred::Cap) -> bool {
    c.proc.capable(cap)
}

fn put_i32(c: &SysCtx<'_>, addr: u64, v: i32) -> i64 {
    c.proc.mm.copy_to_user(addr, &v.to_le_bytes()).map(|_| 0).unwrap_or(-EFAULT)
}

/// `cap_valid`.
fn cap_valid(cap: u64) -> bool {
    cap <= CAP_LAST_CAP
}

/// `security_task_prctl`: commoncap and Yama answer these; `None` = not theirs (Linux's -ENOSYS).
fn lsm_prctl(c: &SysCtx<'_>, option: i32, a: [u64; 4]) -> Option<i64> {
    let pr = c.thread.prctl();
    let [arg2, arg3, arg4, arg5] = a;
    Some(match option {
        PR_CAPBSET_READ => {
            if !cap_valid(arg2) {
                return Some(-EINVAL);
            }
            (pr.cap_bset() >> arg2 & 1) as i64
        }
        PR_CAPBSET_DROP => {
            // `cap_prctl_drop`: CAP_SETPCAP, then a valid cap.
            if !root(c, proc::cred::Cap::Setpcap) {
                return Some(-EPERM);
            }
            if !cap_valid(arg2) {
                return Some(-EINVAL);
            }
            pr.drop_cap(arg2);
            0
        }
        PR_SET_SECUREBITS => {
            let old = pr.securebits() as u64;
            if ((old & SECURE_ALL_LOCKS) >> 1) & (old ^ arg2) != 0 || (old & SECURE_ALL_LOCKS & !arg2) != 0 || arg2 & !(SECURE_ALL_LOCKS | SECURE_ALL_BITS) != 0 || !root(c, proc::cred::Cap::Setpcap) {
                return Some(-EPERM);
            }
            pr.set_securebits(arg2 as u32);
            0
        }
        PR_GET_SECUREBITS => pr.securebits() as i64,
        PR_GET_KEEPCAPS => (pr.securebits() >> SECURE_KEEP_CAPS & 1) as i64,
        PR_SET_KEEPCAPS => {
            if arg2 > 1 {
                return Some(-EINVAL);
            }
            let b = pr.securebits();
            if b >> SECURE_KEEP_CAPS_LOCKED & 1 != 0 {
                return Some(-EPERM);
            }
            pr.set_securebits(if arg2 != 0 { b | 1 << SECURE_KEEP_CAPS } else { b & !(1 << SECURE_KEEP_CAPS) });
            0
        }
        PR_CAP_AMBIENT => {
            if arg2 == PR_CAP_AMBIENT_CLEAR_ALL {
                if arg3 | arg4 | arg5 != 0 {
                    return Some(-EINVAL);
                }
                return Some(0);
            }
            if !cap_valid(arg3) || arg4 | arg5 != 0 {
                return Some(-EINVAL);
            }
            match arg2 {
                // V2 keeps no ambient set: nothing is raised.
                PR_CAP_AMBIENT_IS_SET => 0,
                // Raising needs the cap in the permitted and inheritable sets; V2's inheritable set is empty
                // (as root's is on Linux by default).
                PR_CAP_AMBIENT_RAISE => -EPERM,
                PR_CAP_AMBIENT_LOWER => 0,
                _ => -EINVAL,
            }
        }
        // Yama: nothing to remember without ptrace; an unknown tracer pid is EINVAL.
        PR_SET_PTRACER => {
            if arg2 == 0 || arg2 == u64::MAX || arg2 as i32 == -1 {
                0
            } else if proc::find_thread(arg2 as u32).is_some() {
                0
            } else {
                -EINVAL
            }
        }
        _ => return None,
    })
}

fn sys_prctl(c: &mut SysCtx<'_>) -> i64 {
    let option = c.args[0] as i32;
    let [arg2, arg3, arg4, arg5] = [c.args[1], c.args[2], c.args[3], c.args[4]];
    if let Some(r) = lsm_prctl(c, option, [arg2, arg3, arg4, arg5]) {
        return r;
    }
    let pr = c.thread.prctl();
    match option {
        PR_SET_PDEATHSIG => {
            if arg2 > NSIG {
                return -EINVAL;
            }
            pr.set_pdeath_signal(arg2 as u32);
            0
        }
        PR_GET_PDEATHSIG => put_i32(c, arg2, pr.pdeath_signal() as i32),
        PR_GET_DUMPABLE => c.proc.mm_flags.dumpable() as i64,
        PR_SET_DUMPABLE => {
            if arg2 > SUID_DUMP_USER {
                return -EINVAL;
            }
            c.proc.mm_flags.set_dumpable(arg2 as u32);
            0
        }
        // x86 has no unaligned-access, FP-emulation, FP-exception, endianness or FP-mode controls.
        PR_SET_UNALIGN | PR_GET_UNALIGN | PR_SET_FPEMU | PR_GET_FPEMU | PR_SET_FPEXC | PR_GET_FPEXC | PR_GET_ENDIAN | PR_SET_ENDIAN | PR_SET_FP_MODE | PR_GET_FP_MODE => -EINVAL,
        PR_GET_TIMING => 0,
        PR_SET_TIMING => {
            if arg2 != 0 {
                -EINVAL
            } else {
                0
            }
        }
        PR_SET_NAME => set_name(c, arg2),
        PR_GET_NAME => c.proc.mm.copy_to_user(arg2, &pr.comm()).map(|_| 0).unwrap_or(-EFAULT),
        PR_GET_SECCOMP => 0,
        PR_SET_SECCOMP => not_built(c, option, 0, "seccomp"),
        // `GET_TSC_CTL`: rdtsc is always allowed in V2.
        PR_GET_TSC => put_i32(c, arg2, PR_TSC_ENABLE),
        PR_SET_TSC => {
            if arg2 == PR_TSC_ENABLE as u64 {
                0
            } else if arg2 == PR_TSC_SIGSEGV as u64 {
                not_built(c, option, 1, "rdtsc trapping")
            } else {
                -EINVAL
            }
        }
        PR_TASK_PERF_EVENTS_DISABLE | PR_TASK_PERF_EVENTS_ENABLE => 0,
        // The slack as a long (a value above LONG_MAX reads as an error, as on Linux).
        PR_GET_TIMERSLACK => pr.timer_slack() as i64,
        PR_SET_TIMERSLACK => {
            pr.set_timer_slack(arg2);
            0
        }
        PR_MCE_KILL => mce_kill(c, arg2, arg3, arg4, arg5),
        PR_MCE_KILL_GET => {
            if arg2 | arg3 | arg4 | arg5 != 0 {
                return -EINVAL;
            }
            let f = pr.mce();
            if f & MCE_PROCESS != 0 { if f & MCE_EARLY != 0 { PR_MCE_KILL_EARLY as i64 } else { PR_MCE_KILL_LATE as i64 } } else { PR_MCE_KILL_DEFAULT as i64 }
        }
        PR_SET_MM => {
            // `prctl_set_mm`: the size query and PR_SET_MM_MAP come before the capability check.
            if arg2 == PR_SET_MM_MAP_SIZE {
                return c.proc.mm.copy_to_user(arg3, &PRCTL_MM_MAP_SIZE.to_le_bytes()).map(|_| 0).unwrap_or(-EFAULT);
            }
            if arg2 == PR_SET_MM_MAP {
                // `prctl_set_mm_map`: the size check first; rewriting the mm's layout (checkpoint/restore) is
                // not in V2.
                return if arg4 != PRCTL_MM_MAP_SIZE as u64 { -EINVAL } else { not_built(c, option, 2, "PR_SET_MM_MAP") };
            }
            if !root(c, proc::cred::Cap::SysResource) {
                return -EPERM;
            }
            not_built(c, option, 2, "PR_SET_MM")
        }
        PR_GET_TID_ADDRESS => c.proc.mm.write_u64(arg2, c.thread.clear_child_tid.load(Ordering::Acquire)).map(|_| 0).unwrap_or(-EFAULT),
        PR_SET_CHILD_SUBREAPER => {
            c.proc.set_child_subreaper(arg2 != 0);
            0
        }
        PR_GET_CHILD_SUBREAPER => put_i32(c, arg2, c.proc.is_child_subreaper() as i32),
        PR_SET_NO_NEW_PRIVS => {
            if arg2 != 1 || arg3 | arg4 | arg5 != 0 {
                return -EINVAL;
            }
            pr.set_no_new_privs();
            0
        }
        PR_GET_NO_NEW_PRIVS => {
            if arg2 | arg3 | arg4 | arg5 != 0 {
                return -EINVAL;
            }
            pr.no_new_privs() as i64
        }
        PR_GET_THP_DISABLE => {
            if arg2 | arg3 | arg4 | arg5 != 0 {
                return -EINVAL;
            }
            c.proc.mm_flags.thp_disabled() as i64
        }
        PR_SET_THP_DISABLE => {
            if arg3 | arg4 | arg5 != 0 {
                return -EINVAL;
            }
            c.proc.mm_flags.set_thp_disabled(arg2 != 0);
            0
        }
        PR_MPX_ENABLE_MANAGEMENT | PR_MPX_DISABLE_MANAGEMENT => -EINVAL,
        PR_SVE_SET_VL | PR_SVE_GET_VL | PR_SME_SET_VL | PR_SME_GET_VL => -EINVAL,
        PR_GET_SPECULATION_CTRL => {
            if arg3 | arg4 | arg5 != 0 {
                return -EINVAL;
            }
            match arg2 {
                PR_SPEC_STORE_BYPASS | PR_SPEC_INDIRECT_BRANCH | PR_SPEC_L1D_FLUSH => PR_SPEC_NOT_AFFECTED,
                _ => -ENODEV,
            }
        }
        PR_SET_SPECULATION_CTRL => {
            if arg4 | arg5 != 0 {
                return -EINVAL;
            }
            match arg2 {
                // `ssb_prctl_set` & co. on a CPU reported as not affected / without prctl control.
                PR_SPEC_STORE_BYPASS | PR_SPEC_INDIRECT_BRANCH | PR_SPEC_L1D_FLUSH => -ENXIO,
                _ => -ENODEV,
            }
        }
        PR_PAC_RESET_KEYS | PR_PAC_SET_ENABLED_KEYS | PR_PAC_GET_ENABLED_KEYS | PR_SET_TAGGED_ADDR_CTRL | PR_GET_TAGGED_ADDR_CTRL => {
            // The argument checks come first in Linux, then the arch answer (x86: EINVAL either way).
            -EINVAL
        }
        PR_SET_IO_FLUSHER => {
            if !root(c, proc::cred::Cap::SysResource) {
                return -EPERM;
            }
            if arg3 | arg4 | arg5 != 0 {
                return -EINVAL;
            }
            match arg2 {
                1 => pr.set_io_flusher(true),
                0 => pr.set_io_flusher(false),
                _ => return -EINVAL,
            }
            0
        }
        PR_GET_IO_FLUSHER => {
            if !root(c, proc::cred::Cap::SysResource) {
                return -EPERM;
            }
            if arg2 | arg3 | arg4 | arg5 != 0 {
                return -EINVAL;
            }
            pr.io_flusher() as i64
        }
        PR_SET_SYSCALL_USER_DISPATCH => match arg2 {
            // `set_syscall_user_dispatch(OFF)`: offset, len and selector must be 0.
            PR_SYS_DISPATCH_OFF => {
                if arg3 | arg4 | arg5 != 0 {
                    -EINVAL
                } else {
                    0
                }
            }
            PR_SYS_DISPATCH_ON => not_built(c, option, 3, "syscall user dispatch"),
            _ => -EINVAL,
        },
        PR_SCHED_CORE => not_built(c, option, 4, "core scheduling"),
        PR_SET_MDWE | PR_GET_MDWE => not_built(c, option, 5, "memory-deny-write-execute"),
        PR_SET_VMA => not_built(c, option, 6, "anonymous VMA names"),
        PR_SET_MEMORY_MERGE | PR_GET_MEMORY_MERGE => not_built(c, option, 7, "KSM"),
        PR_RISCV_V_SET_CONTROL | PR_RISCV_V_GET_CONTROL => -EINVAL,
        PR_GET_AUXV => {
            if arg4 | arg5 != 0 {
                return -EINVAL;
            }
            get_auxv(c, arg2, arg3)
        }
        _ => -EINVAL,
    }
}
crate::syscall_nr!(uapi::ab::nr_more::PRCTL, "prctl", sys_prctl);

/// PR_SET_NAME: up to 15 bytes of the string (`strncpy_from_user(comm, arg2, 15)`; EFAULT only when no byte
/// of the first 15 could be read up to a NUL).
fn set_name(c: &SysCtx<'_>, addr: u64) -> i64 {
    let mut v = alloc::vec::Vec::new();
    match hal::usercopy::strncpy_from_user(c.proc.mm.root(), addr, &mut v, COMM_LEN - 1) {
        Ok(_) | Err(hal::usercopy::StrError::TooLong) => {
            let name = comm_of(&v[..v.len().min(COMM_LEN - 1)]);
            c.thread.prctl().set_comm(name);
            klog::event!(Debug, "proc", "comm.set", pid = c.proc.pid, tid = c.thread.tid(), comm = c.thread.prctl().comm_str().as_str());
            0
        }
        Err(hal::usercopy::StrError::Fault) => -EFAULT,
    }
}

/// PR_MCE_KILL (`PF_MCE_PROCESS`/`PF_MCE_EARLY`).
fn mce_kill(c: &SysCtx<'_>, arg2: u64, arg3: u64, arg4: u64, arg5: u64) -> i64 {
    if arg4 | arg5 != 0 {
        return -EINVAL;
    }
    let pr = c.thread.prctl();
    match arg2 {
        PR_MCE_KILL_CLEAR => {
            if arg3 != 0 {
                return -EINVAL;
            }
            pr.set_mce(pr.mce() & !MCE_PROCESS);
            0
        }
        PR_MCE_KILL_SET => {
            let f = pr.mce() | MCE_PROCESS;
            let f = match arg3 {
                PR_MCE_KILL_EARLY => f | MCE_EARLY,
                PR_MCE_KILL_LATE => f & !MCE_EARLY,
                PR_MCE_KILL_DEFAULT => f & !(MCE_EARLY | MCE_PROCESS),
                _ => {
                    // Linux has set PF_MCE_PROCESS before it refuses the third argument.
                    pr.set_mce(f);
                    return -EINVAL;
                }
            };
            pr.set_mce(f);
            0
        }
        _ => -EINVAL,
    }
}

/// PR_GET_AUXV (`prctl_get_auxv`): min(len, sizeof saved_auxv) bytes of the vector the image started with
/// (zero after its AT_NULL), the answer is always the full size.
fn get_auxv(c: &SysCtx<'_>, addr: u64, len: u64) -> i64 {
    let mut b = [0u8; SAVED_AUXV_BYTES];
    for (i, w) in c.proc.saved_auxv.lock().iter().take(SAVED_AUXV_BYTES / 8).enumerate() {
        b[i * 8..i * 8 + 8].copy_from_slice(&w.to_le_bytes());
    }
    let n = (len.min(SAVED_AUXV_BYTES as u64)) as usize;
    if n > 0 && c.proc.mm.copy_to_user(addr, &b[..n]).is_err() {
        return -EFAULT;
    }
    SAVED_AUXV_BYTES as i64
}
