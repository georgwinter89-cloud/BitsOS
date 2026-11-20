//! GM-2a CR 3 and 4: `ptrace` without tracing, and `personality`.
//! ptrace: V2 traces nothing, so no process is ever a tracee, and every request gets the
//! answer Linux gives for a process that is not traced (linux-7.3.1/kernel/ptrace.c:1419-1454, in that order):
//! - PTRACE_TRACEME: EPERM. A deliberate difference from Linux's 0: the caller would believe it has a tracer.
//! - no such pid: ESRCH.
//! - PTRACE_ATTACH, PTRACE_SEIZE: SEIZE with `addr != 0` or unknown option bits EIO (`ptrace_attach`), else EPERM.
//! - everything else, PEEK and POKE and unknown numbers too: ESRCH (`ptrace_check_attach`).

use crate::SysCtx;
use core::sync::atomic::{AtomicBool, Ordering};
use uapi::errno::*;

const PTRACE_TRACEME: i64 = 0;
const PTRACE_ATTACH: i64 = 16;
const PTRACE_SEIZE: i64 = 0x4206;
/// linux-7.3.1/include/uapi/linux/ptrace.h:185 `PTRACE_O_MASK`: up to and including PTRACE_O_SUSPEND_SECCOMP and
/// EXITKILL.
const PTRACE_O_MASK: u64 = 0x0030_00ff;

static LOGGED: AtomicBool = AtomicBool::new(false);

fn sys_ptrace(c: &mut SysCtx<'_>) -> i64 {
    let (request, pid, addr, data) = (c.args[0] as i64, c.args[1] as i32, c.args[2], c.args[3]);
    if !LOGGED.swap(true, Ordering::Relaxed) {
        klog::event!(Info, "syscall", "ptrace.no_tracing", request = request as u64, pid = c.proc.pid as u64; "ptrace answered as for a process nobody traces (logged once)");
    }
    if request == PTRACE_TRACEME {
        return -EPERM;
    }
    if pid <= 0 || (crate::ns_sys::find_thread(c, pid as u32).is_none() && crate::ns_sys::find(c, pid as u32).is_none()) {
        return -ESRCH;
    }
    // DB2: V2 refuses every attach; the access log keeps the attempt on another program.
    if matches!(request, PTRACE_ATTACH | PTRACE_SEIZE) { vfs::access::right(vfs::access::Kind::Ptrace, c.proc.pid, c.proc.uid, pid as u64) }
    match request {
        PTRACE_ATTACH => -EPERM,
        PTRACE_SEIZE if addr != 0 || data & !PTRACE_O_MASK != 0 => -EIO,
        PTRACE_SEIZE => -EPERM,
        _ => -ESRCH,
    }
}
crate::syscall_nr!(101, "ptrace", sys_ptrace);

/// `personality(2)` (linux-7.3.1/kernel/exec_domain.c:38): the old value, 0xffffffff only reads. V2 stores it and shows
/// it in /proc; only PER_CLEAR_ON_SETID's clearing acts on it so far (ADDR_NO_RANDOMIZE does not change exec's layout).
fn sys_personality(c: &mut SysCtx<'_>) -> i64 {
    let (new, pr) = (c.args[0] as u32, c.thread.prctl());
    let old = pr.personality();
    if new != 0xffff_ffff {
        pr.set_personality(new);
    }
    i64::from(old)
}
crate::syscall_nr!(135, "personality", sys_personality);
