//! AB-6: signals on the way back to user mode (linux-6.6/arch/x86/kernel/signal.c `arch_do_signal_or_restart`
//! :331, `handle_signal` :234; signal_64.c `x64_setup_rt_frame` :160, `rt_sigreturn` :241; kernel/signal.c
//! `get_signal` :2668, `do_signal_stop` :2486, `do_notify_parent_cldstop` :2180), the faults that raise
//! them (arch/x86/kernel/traps.c, mm/fault.c), and the parent's job-control notices.
//!
//! Every thread passes `exit_to_user` before it runs user code again after a syscall, a fault or an
//! interrupt that found its signal flag set. There it takes part in a group stop (parked until SIGCONT or
//! SIGKILL), takes its pending signals one by one, runs the default action or builds a handler frame
//! (`hal::signal_frame`), and decides what an interrupted syscall answers (ERESTARTSYS & co.).

use crate::group::{Group, kick};
use crate::signal::{AltStack, Cld, Next, SA_ONSTACK, SA_RESTART, SA_RESTORER, SA_SIGINFO, SS_AUTODISARM, SigAction, SigInfo, To};
use crate::status;
use crate::task::Thread;
use crate::{ExitStatus, FAULTED, Process};
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use hal::entry::UserContext;
use hal::signal_frame as sf;
use uapi::ab::siginfo::*;
use uapi::ab::signal::*;
use uapi::errno::EINTR;

klog::counter!(DELIVERED, "signal.delivered");
klog::counter!(FRAME_FAILED, "signal.frame_failed");
klog::counter!(BAD_FRAMES, "signal.bad_frames");
klog::counter!(STOPS, "signal.group_stops");

/// Bad frames recorded so far (the hostile-sigreturn ktest checks it grew).
pub fn bad_frame_count() -> u64 {
    BAD_FRAMES.get()
}

/// The V2 fault message (R3) of a fault signal, logged and kept as the end message when the fault's default
/// action ends the group (a handled fault is only a Debug event: JITs and GCs fault on purpose).
pub struct FaultNote {
    pub sig: u32,
    pub vector: u8,
    pub rip: u64,
    pub addr: u64,
    pub reason: &'static str,
    pub msg: String,
}

/// A syscall's answer as the way out sees it (Linux `orig_ax` and `ax`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SysExit {
    pub nr: usize,
    pub ret: i64,
}

fn is_restart(ret: i64) -> bool {
    matches!(ret.wrapping_neg(), ERESTARTSYS | ERESTARTNOINTR | ERESTARTNOHAND | ERESTART_RESTARTBLOCK)
}

/// What an interrupted call answers in Linux when its wait returned EINTR for a signal (the answer the
/// wait would give there; FS's and NW's waits give EINTR and this table puts Linux's back). Pure.
pub fn restart_class(nr: usize) -> Option<i64> {
    use uapi::ab::nr_more as n;
    use uapi::nr::*;
    Some(match nr {
        // fs/read_write.c, pipe_read/pipe_write, fifo open, the tty, sockets, wait4/waitid, locks (-ERESTARTSYS).
        READ | WRITE | OPEN | IOCTL | PREAD64 | PWRITE64 | READV | WRITEV | WAIT4 | FCNTL | OPENAT => ERESTARTSYS,
        40 | 42 | 43 | 44 | 45 | 46 | 47 | 85 | 275 | 276 | 288 | 295 | 296 | 299 | 307 | 326 | 327 | 328 | 437 => ERESTARTSYS,
        n::FLOCK | n::WAITID => ERESTARTSYS,
        // select/pselect6/ppoll, pause, sigsuspend (-ERESTARTNOHAND: EINTR when a handler runs).
        23 | 270 | 271 => ERESTARTNOHAND,
        n::PAUSE | n::RT_SIGSUSPEND => ERESTARTNOHAND,
        // poll keeps a restart block on Linux (the time left); V2 restarts it with its arguments when no
        // handler runs (stop/continue), as ERESTARTNOHAND.
        POLL => ERESTARTNOHAND,
        _ => return None,
    })
}

/// The syscall exit (`syscall_exit_to_user_mode`): EINTR from an interrupted wait becomes Linux's restart
/// answer, then signals are delivered. rt_sigreturn has no syscall to restart (`orig_ax = -1`).
pub fn syscall_exit(t: &Arc<Thread>, ctx: &mut UserContext, nr: usize, ret: i64) {
    if nr == uapi::nr::RT_SIGRETURN {
        exit_to_user(t, ctx, None);
        return;
    }
    let mut ret = ret;
    // An interrupted call may have left a saved mask (ppoll/pselect/epoll_pwait/sigsuspend): the way out
    // must put it back even when a sibling thread took the signal meanwhile (Linux's own
    // TIF_RESTORE_SIGMASK work bit, include/linux/sched/signal.h `set_restore_sigmask`).
    let interrupted = ret == -EINTR || is_restart(ret);
    if ret == -EINTR
        && !t.group().group_exited()
        && t.signal_pending()
        && let Some(code) = restart_class(nr)
    {
        ret = -code;
        ctx.set_syscall_return(ret);
    }
    exit_inner(t, ctx, Some(SysExit { nr, ret }), interrupted);
}

/// `arch_do_signal_or_restart` in a loop, as `exit_to_user_mode_loop` runs it while the flag is set. The
/// run loop ends the thread afterwards when the group is ending.
pub fn exit_to_user(t: &Arc<Thread>, ctx: &mut UserContext, sys: Option<SysExit>) {
    exit_inner(t, ctx, sys, false);
}

/// `exit_to_user` with `may_have_saved`: the slow path runs even without a signal so a saved mask goes back.
fn exit_inner(t: &Arc<Thread>, ctx: &mut UserContext, sys: Option<SysExit>, may_have_saved: bool) {
    let mut sys = sys.filter(|s| is_restart(s.ret));
    if sys.is_none() && !may_have_saved && !t.signal_pending() {
        return;
    }
    let g = t.group();
    let key = t.sig_key();
    // The outer loop is `exit_to_user_mode_loop` re-reading its work bits: a signal sent after `next_for`
    // found nothing (or one the restored mask lets through) is taken before the return to user mode.
    'work: loop {
        loop {
            if g.group_exited() {
                return;
            }
            if let Some(c) = g.signals.raw(|s| s.take_cld()) {
                notify_parent_cld(g, c);
            }
            if let Some((notify, code)) = g.signals.raw(|s| s.participate(key).map(|n| (n, s.stop_code))) {
                if notify {
                    STOPS.inc();
                    klog::event!(Info, "proc", "signal.stopped", pid = g.tgid, signal = uapi::ab::signal_names::name(code));
                    notify_parent_cld(g, Cld::Stopped(code));
                }
                park(t);
                continue;
            }
            let Some(next) = g.signals.raw(|s| s.next_for(key)) else { break };
            match next {
                Next::Handler(info, act) => {
                    if let Some(s) = sys.take() {
                        restart_with_handler(ctx, s, &act);
                    }
                    if setup_frame(t, ctx, &info, &act) {
                        DELIVERED.inc();
                    } else {
                        FRAME_FAILED.inc();
                        klog::event!(Info, "proc", "signal.frame_failed", pid = g.tgid, tid = t.tid(), signal = uapi::ab::signal_names::name(info.signo()), sp = klog::Hex(ctx.regs.rsp));
                        force_sigsegv(t, info.signo());
                    }
                }
                Next::Stop(info) => {
                    let sig = info.signo();
                    // POSIX: a terminal stop signal to an orphaned process group is discarded (nobody could
                    // continue it); SIGSTOP always stops.
                    if sig != SIGSTOP && crate::job::pgrp_orphaned(g.pgid(), None) {
                        continue;
                    }
                    let w = g.signals.raw(|s| s.begin_group_stop(key, sig));
                    kick(&w);
                }
                Next::Fatal(info) => {
                    fatal(t, &info);
                    return;
                }
            }
        }
        if let Some(s) = sys.take() {
            restart_without_handler(ctx, s);
        }
        let (w, again) = g.signals.raw(|s| {
            let w = s.restore_saved(key);
            let on = s.recalc(key);
            (w, on && (s.deliverable(key) || s.stop_pending(key)))
        });
        kick(&w);
        if !again || g.group_exited() {
            break 'work;
        }
    }
}

/// `handle_signal`'s restart part: a handler interrupts the call.
fn restart_with_handler(ctx: &mut UserContext, s: SysExit, act: &SigAction) {
    match -s.ret {
        ERESTART_RESTARTBLOCK | ERESTARTNOHAND => ctx.set_syscall_return(-EINTR),
        ERESTARTSYS if act.flags & SA_RESTART == 0 => ctx.set_syscall_return(-EINTR),
        ERESTARTSYS | ERESTARTNOINTR => restart(ctx, s.nr as u64),
        _ => {}
    }
}

/// No handler ran: the call starts again (the restart block's `restart_syscall` for a sleep).
fn restart_without_handler(ctx: &mut UserContext, s: SysExit) {
    match -s.ret {
        ERESTARTNOHAND | ERESTARTSYS | ERESTARTNOINTR => restart(ctx, s.nr as u64),
        ERESTART_RESTARTBLOCK => restart(ctx, uapi::ab::nr_more::RESTART_SYSCALL as u64),
        _ => {}
    }
}

/// rax = the number again, rip back over the 2-byte `syscall` (the return then takes IRET: rcx != rip).
fn restart(ctx: &mut UserContext, nr: u64) {
    ctx.regs.rax = nr;
    ctx.regs.rip = ctx.regs.rip.wrapping_sub(2);
}

/// JOBCTL_STOPPED: parked until SIGCONT clears the stop or the group ends (a Normal-class user thread; the
/// wait looks again every second).
fn park(t: &Arc<Thread>) {
    let g = t.group();
    let key = t.sig_key();
    let stopped = || g.signals.raw(|s| s.is_stopped(key));
    while !g.group_exited() && stopped() {
        g.wq.wait_until_timeout(|| g.group_exited() || !stopped(), 1_000_000_000);
    }
}

/// The default action ends the group (`do_group_exit`). A fault's own message (R3) goes with it.
fn fatal(t: &Arc<Thread>, info: &SigInfo) {
    let g = t.group();
    let sig = info.signo();
    if info.is_kernel_fault()
        && let Some(n) = t.take_fault_msg()
        && n.sig == sig
    {
        FAULTED.inc();
        let name = g.image().map(|p| p.name.clone()).unwrap_or_default();
        klog::event!(Warn, "proc", "fault", pid = g.tgid, program = name.as_str(), thread = t.tid(), vector = n.vector, rip = klog::Hex(n.rip), address = klog::Hex(n.addr), reason = n.reason, signal = uapi::ab::signal_names::name(sig); "{}", n.msg.as_str());
        // Only the end it caused keeps its message: after a kill or exit_group the fault is noise of the way
        // out. Readers look after the threads ended (`done`), so setting it after `end_with` is seen by all.
        if g.end_with(ExitStatus::Signaled(sig)) {
            g.set_end_message(n.msg);
        }
        return;
    }
    g.end_by_signal(sig);
}

/// `force_sigsegv`: the frame for `sig` could not be built.
fn force_sigsegv(t: &Arc<Thread>, sig: u32) {
    t.group().force_info(t.sig_key(), SigInfo::kernel(SIGSEGV), sig == SIGSEGV);
}

/// `x64_setup_rt_frame` + `signal_delivered`: false when the frame cannot be written (no SA_RESTORER, an
/// alternate stack that would overflow, memory the program cannot write).
fn setup_frame(t: &Arc<Thread>, ctx: &mut UserContext, info: &SigInfo, act: &SigAction) -> bool {
    let g = t.group();
    let Some(p) = g.image() else { return false };
    // x86-64 has no kernel-provided return path: the libc's restorer is required (EFAULT on Linux).
    if act.flags & SA_RESTORER == 0 {
        return false;
    }
    let shape = sf::shape();
    let alt = t.alt_stack();
    let a = sf::Alt { sp: alt.sp, size: alt.size, autodisarm: alt.flags & SS_AUTODISARM != 0 };
    let Some((frame, fp)) = sf::place(ctx.regs.rsp, act.flags & SA_ONSTACK != 0, a, shape.frame_bytes()) else { return false };
    let Some(img) = sf::save_user_fpu() else { return false };
    if p.mm.copy_to_user(fp, &img).is_err() {
        return false;
    }
    let key = t.sig_key();
    let mask = g.signals.raw(|s| s.mask_to_save(key));
    let [trapno, err, cr2] = t.trap();
    let head = sf::FrameHead {
        restorer: act.restorer,
        uc_flags: sf::UC_FP_XSTATE | sf::UC_SIGCONTEXT_SS | sf::UC_STRICT_RESTORE_SS,
        stack: AltStack { sp: alt.sp, flags: alt.flags, size: alt.size }.to_bytes(),
        mctx: sf::Mcontext { regs: ctx.regs, cs: hal::gdt::USER_CS, ss: hal::gdt::USER_SS, err, trapno, oldmask: mask, cr2, fpstate: fp },
        sigmask: mask,
    };
    if p.mm.copy_to_user(frame, &sf::encode(&head)).is_err() {
        return false;
    }
    if act.flags & SA_SIGINFO != 0 && p.mm.copy_to_user(frame + sf::INFO_OFFSET, &info.to_user()).is_err() {
        return false;
    }
    let sig = info.signo();
    let r = &mut ctx.regs;
    r.rdi = sig as u64;
    r.rax = 0;
    r.rsi = frame + sf::INFO_OFFSET;
    r.rdx = frame + sf::UC_OFFSET;
    r.rip = act.handler;
    r.rsp = frame;
    // DF clear for the function entry, RF and TF off (no debug trap at the handler's first instruction).
    r.rflags &= !(0x400 | 0x1_0000 | 0x100);
    sf::init_user_fpu();
    let w = g.signals.raw(|s| s.delivered(key, sig, act));
    kick(&w);
    if alt.flags & SS_AUTODISARM != 0 {
        t.reset_alt_stack();
    }
    klog::event!(Debug, "proc", "signal.delivered", pid = g.tgid, tid = t.tid(), signal = uapi::ab::signal_names::name(sig), code = info.code(), handler = klog::Hex(act.handler), frame = klog::Hex(frame));
    true
}

// ---------------------------------------------------------------------------------------------------------
// sigreturn

/// `rt_sigreturn`: the mask, the registers (RFLAGS through FIX_EFLAGS, selectors checked), the FPU image
/// (checked as `hal::signal_frame` says) and the alternate stack come back from the frame at rsp - 8.
/// Returns the restored rax (the syscall's answer). A bad frame forces SIGSEGV and answers 0, as Linux.
pub fn sigreturn(t: &Arc<Thread>, ctx: &mut UserContext) -> i64 {
    let g = t.group();
    let key = t.sig_key();
    let Some(p) = g.image() else { return 0 };
    let frame = ctx.regs.rsp.wrapping_sub(8);
    if frame.checked_add(sf::FRAME_SIZE).is_none_or(|e| e > hal::USER_TOP) {
        return bad_frame(t, ctx, frame, "frame outside user space");
    }
    let mut head = [0u8; sf::HEAD_SIZE];
    if p.mm.copy_from_user(&mut head[8..], frame + 8).is_err() {
        return bad_frame(t, ctx, frame, "frame unreadable");
    }
    let h = sf::decode(&head);
    let w = g.signals.raw(|s| s.set_blocked(key, h.sigmask));
    kick(&w);
    // `restart_block.fn = do_no_restart_syscall`: a pending restart answers EINTR from now on.
    t.set_restart(None);
    let m = h.mctx;
    let flags = sf::restored_rflags(ctx.regs.rflags, m.regs.rflags);
    ctx.regs = m.regs;
    ctx.regs.rflags = flags;
    if !sf::selectors_ok(m.cs, m.ss, h.uc_flags) {
        return bad_frame(t, ctx, frame, "selectors");
    }
    if let Err(why) = restore_fpu(&p, m.fpstate) {
        sf::init_user_fpu();
        return bad_frame(t, ctx, frame, why);
    }
    // `restore_altstack`: errors other than a fault are ignored.
    let _ = t.sigaltstack(Some(AltStack::from_bytes(&h.stack)), ctx.regs.rsp);
    ctx.regs.rax as i64
}

/// `fpu__restore_sig` (fpu/signal.c:497): NULL = the initial state; otherwise the software bytes decide
/// between the full image and the legacy area, and only what the restore reads is copied and checked.
fn restore_fpu(p: &Process, buf: u64) -> Result<(), &'static str> {
    if buf == 0 {
        sf::init_user_fpu();
        return Ok(());
    }
    let s = sf::shape();
    if buf.checked_add(s.frame_bytes()).is_none_or(|e| e > hal::USER_TOP) {
        return Err("fpstate outside user space");
    }
    let mut sw = [0u8; 48];
    p.mm.copy_from_user(&mut sw, buf + sf::SW_BYTES_AT as u64).map_err(|_| "fpstate unreadable")?;
    let mut plan = sf::plan_from_sw(&sw, s);
    if let Some(at) = plan.magic2_at {
        let mut m = [0u8; 4];
        p.mm.copy_from_user(&mut m, buf + at as u64).map_err(|_| "fpstate unreadable")?;
        plan = sf::plan_with_magic2(plan, u32::from_le_bytes(m));
    }
    let bv = if plan.fx_only { 0 } else { p.mm.read_u64(buf + sf::XSTATE_BV_AT as u64).map_err(|_| "fpstate unreadable")? };
    let n = sf::bytes_needed(plan, bv, s);
    let mut img: Vec<u8> = Vec::new();
    img.try_reserve_exact(n).map_err(|_| "no memory")?;
    img.resize(n, 0);
    p.mm.copy_from_user(&mut img, buf).map_err(|_| "fpstate unreadable")?;
    let area = sf::check_image(&img, buf, plan, s, sf::mxcsr_mask()).map_err(|e| e.text())?;
    sf::restore_user_fpu(&area).map_err(|e| e.text())
}

/// `signal_fault`: SIGSEGV to the thread (handled if it can be: `force_sig`), the answer 0.
fn bad_frame(t: &Arc<Thread>, ctx: &mut UserContext, frame: u64, why: &str) -> i64 {
    let n = BAD_FRAMES.get() + 1;
    BAD_FRAMES.inc();
    // A program that loops on bad frames under a SIGSEGV handler must not flood the log.
    if n.is_power_of_two() {
        klog::event!(Info, "proc", "signal.bad_frame", pid = t.group().tgid, tid = t.tid(), frame = klog::Hex(frame), rip = klog::Hex(ctx.regs.rip), reason = why, count = n);
    }
    t.group().force_info(t.sig_key(), SigInfo::kernel(SIGSEGV), false);
    0
}

// ---------------------------------------------------------------------------------------------------------
// faults

/// A user page fault the address space could not serve (`bad_area`, `do_sigbus`): SIGSEGV with SEGV_MAPERR
/// (nothing mapped) or SEGV_ACCERR (mapped, access not allowed), SIGBUS/BUS_ADRERR past a file's end; trap
/// info as `set_signal_archinfo` (a kernel address is reported as a protection fault).
pub(crate) fn page_fault(p: &Arc<Process>, t: &Arc<Thread>, ctx: &UserContext, error: u64, cr2: u64, why: mm::FaultReason) {
    use mm::FaultReason as F;
    let (sig, code) = match why {
        F::Bus => (SIGBUS, BUS_ADRERR),
        F::NotMapped | F::KernelAddress | F::NoMemory | F::Repeated => (SIGSEGV, SEGV_MAPERR),
        F::ProtNone | F::ReadOnly | F::NoExec | F::Unexpected => (SIGSEGV, SEGV_ACCERR),
    };
    let err = error | mm::fault::PF_USER | if cr2 >= hal::USER_TOP { 1 } else { 0 };
    t.set_trap(14, err, cr2);
    raise(p, t, ctx, 14, sig, code, cr2, why.text());
}

/// Another CPU exception in user mode (traps.c): the signal, si_code and si_addr Linux gives it. A spurious
/// x87/SIMD trap (no unmasked exception) raises nothing.
pub(crate) fn trap(p: &Arc<Process>, t: &Arc<Thread>, ctx: &UserContext, vector: u8, error: u64) {
    let rip = ctx.regs.rip;
    let (sig, code, addr) = match vector {
        0 => (SIGFPE, FPE_INTDIV, rip),
        1 => (SIGTRAP, TRAP_TRACE, rip),
        6 => (SIGILL, ILL_ILLOPN, rip),
        16 | 19 => match sf::fpu_exception_code(vector) {
            0 => return,
            c => (SIGFPE, c, rip),
        },
        17 => (SIGBUS, BUS_ADRALN, 0),
        21 => (SIGSEGV, SEGV_CPERR, 0),
        _ => (status::trap_signal(vector), SI_KERNEL, 0),
    };
    let [_, _, cr2] = t.trap();
    t.set_trap(vector as u64, error, cr2);
    raise(p, t, ctx, vector, sig, code, addr, hal::idt::exception_name(vector as u64));
}

#[allow(clippy::too_many_arguments)]
fn raise(p: &Arc<Process>, t: &Arc<Thread>, ctx: &UserContext, vector: u8, sig: u32, code: i32, addr: u64, reason: &'static str) {
    let tid = t.tid();
    let at = if vector == 14 { addr } else { 0 };
    let msg = status::fault_message(&p.name, p.pid, tid as u64, vector, at, ctx.regs.rip, reason);
    klog::event!(Debug, "proc", "signal.fault", pid = p.pid, tid = tid, vector = vector, rip = klog::Hex(ctx.regs.rip), address = klog::Hex(addr), signal = uapi::ab::signal_names::name(sig), code = code);
    t.set_fault_msg(FaultNote { sig, vector, rip: ctx.regs.rip, addr: at, reason, msg });
    t.group().force_info(t.sig_key(), SigInfo::fault(sig, code, addr), false);
}

// ---------------------------------------------------------------------------------------------------------
// the parent

/// `do_notify_parent_cldstop` for group `g`: SIGCHLD with CLD_STOPPED (status = the stop signal) or
/// CLD_CONTINUED (status = SIGCONT) unless the parent ignores SIGCHLD or set SA_NOCLDSTOP; its wait4 wakes
/// either way.
pub(crate) fn notify_parent_cld(g: &Group, why: Cld) {
    let ppid = g.ppid();
    if ppid == 0 {
        return;
    }
    let Some(pp) = crate::find(ppid) else { return };
    let (code, status) = match why {
        Cld::Stopped(s) => (CLD_STOPPED, s as i32),
        Cld::Continued => (CLD_CONTINUED, SIGCONT as i32),
    };
    let times = g.cpu_times();
    let info = SigInfo::chld(SIGCHLD, code, g.tgid, g.real_uid(), status, ticks(times.user_ns), ticks(times.system_ns));
    let a = pp.signals.raw(|s| s.action(SIGCHLD));
    if a.handler != crate::signal::SIG_IGN && a.flags & crate::signal::SA_NOCLDSTOP == 0 && !pp.is_finished() {
        let _ = pp.send_info(info, To::Group, false);
    }
    pp.child_event();
}

/// Nanoseconds as clock ticks (USER_HZ 100, `nsec_to_clock_t`).
pub(crate) fn ticks(ns: u64) -> u64 {
    ns / 10_000_000
}

/// `kill_pgrp(pgrp, sig, 1)` from the kernel (the terminal's SIGINT/SIGQUIT/SIGTSTP, IN-4): SI_KERNEL to every
/// live member of group `pgrp`; the number of members it reached.
pub fn kill_pgrp(pgrp: u32, sig: u32) -> usize {
    let members: Vec<Arc<Process>> = crate::processes().into_iter().filter(|p| p.pgid() == pgrp && !p.is_finished()).collect();
    let mut n = 0;
    for p in &members {
        if p.send_info(SigInfo::kernel(sig), To::Group, false).is_ok() {
            n += 1;
        }
    }
    n
}

/// Whether the calling thread has a signal to look at (Linux `signal_pending(current)`; the group's end
/// counts). False in kernel threads.
pub fn current_signal_pending() -> bool {
    crate::task::current_thread().is_some_and(|t| t.signal_pending())
}

/// The calling thread sends itself a signal (`send_sig(sig, current, 0)`, FS-3's SIGPIPE). False in kernel
/// threads.
pub fn signal_current_thread(sig: u32) -> bool {
    match crate::task::current_thread() {
        Some(t) => {
            t.send_signal(sig);
            true
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interrupted_calls_answer_as_linux() {
        use uapi::nr::*;
        assert_eq!(restart_class(READ), Some(ERESTARTSYS));
        assert_eq!(restart_class(WAIT4), Some(ERESTARTSYS));
        assert_eq!(restart_class(288), Some(ERESTARTSYS), "accept4");
        assert_eq!(restart_class(271), Some(ERESTARTNOHAND), "ppoll");
        assert_eq!(restart_class(uapi::ab::nr_more::RT_SIGSUSPEND), Some(ERESTARTNOHAND));
        assert_eq!(restart_class(EPOLL_WAIT), None, "epoll_wait is EINTR on Linux, never restarted");
        assert_eq!(restart_class(uapi::ab::nr_more::RT_SIGTIMEDWAIT), None);
        assert!(is_restart(-ERESTARTSYS) && is_restart(-ERESTART_RESTARTBLOCK) && !is_restart(-(EINTR)));
    }

    #[test]
    fn restarts_follow_handle_signal() {
        let act = |f: u64| SigAction { handler: 0x1000, flags: f | SA_RESTORER, restorer: 0x2000, mask: 0 };
        let call = |ret: i64| SysExit { nr: 0, ret: -ret };
        let mut c = UserContext::new(0x4010, 0x7000);
        c.regs.rax = (-ERESTARTSYS) as u64;
        restart_with_handler(&mut c, call(ERESTARTSYS), &act(0));
        assert_eq!((c.regs.rax as i64, c.regs.rip), (-EINTR, 0x4010), "no SA_RESTART: EINTR");
        let mut c = UserContext::new(0x4010, 0x7000);
        restart_with_handler(&mut c, call(ERESTARTSYS), &act(SA_RESTART));
        assert_eq!((c.regs.rax, c.regs.rip), (0, 0x400e), "SA_RESTART: the call again");
        let mut c = UserContext::new(0x4010, 0x7000);
        restart_with_handler(&mut c, call(ERESTARTNOHAND), &act(SA_RESTART));
        assert_eq!(c.regs.rax as i64, -EINTR, "NOHAND is EINTR with any handler");
        let mut c = UserContext::new(0x4010, 0x7000);
        restart_with_handler(&mut c, call(ERESTARTNOINTR), &act(0));
        assert_eq!(c.regs.rip, 0x400e, "NOINTR always restarts");
        let mut c = UserContext::new(0x4010, 0x7000);
        restart_without_handler(&mut c, SysExit { nr: 35, ret: -ERESTART_RESTARTBLOCK });
        assert_eq!((c.regs.rax, c.regs.rip), (219, 0x400e), "restart_syscall");
        let mut c = UserContext::new(0x4010, 0x7000);
        restart_without_handler(&mut c, SysExit { nr: 7, ret: -ERESTARTNOHAND });
        assert_eq!((c.regs.rax, c.regs.rip), (7, 0x400e));
    }
}
