//! Signal state of a thread group (AB-3 basics, AB-6 delivery), with Linux 6.6's rules
//! (linux-6.6/kernel/signal.c). Pure: host tests drive `Signals` directly; `crate::deliver` runs it on the
//! threads and `hal::signal_frame` builds the frames.
//!
//! What the group keeps under one lock (`proc.signals`, Linux's `sighand->siglock`):
//! - the actions (`rt_sigaction`, `do_sigaction` :4123; CLONE_SIGHAND shares them between the threads),
//! - the shared pending queue (process-directed signals: kill, the kernel's group signals),
//! - per thread (`ThreadSig`, keyed by `Thread::sig_key`): the blocked mask, the mask saved by sigsuspend /
//!   ppoll / pselect (`saved_sigmask` + TIF_RESTORE_SIGMASK), `real_blocked` of sigtimedwait, the thread's own
//!   pending queue (tkill, tgkill, faults), its job-control state and its TIF_SIGPENDING flag (an atomic the
//!   thread reads without the lock),
//! - the group's job-control state (group stop, the CLD_* notification owed to the parent) and
//!   SIGNAL_UNKILLABLE (init).
//!
//! Queues carry a 48-byte kernel siginfo per entry (Linux `kernel_siginfo`): standard signals coalesce
//! (`legacy_queue`), real-time ones queue once per send, bounded by RLIMIT_SIGPENDING (counted per group;
//! Linux counts per user). The alternate signal stack is the thread's (`Thread::alt_stack`).
//!
//! The `blocked` field is a view for callers that know only the process (FS's ppoll mask, IN's job checks): in
//! `SignalState::with` it is the calling thread's mask (or the main thread's for kernel-thread callers), and
//! a change made to it is written back to that thread.

use uapi::ab::signal::*;
use uapi::errno::*;
use vfs::Errno;

mod info;
mod state;

pub use info::*;
pub use state::*;
pub use uapi::ab::sigabi::*;

/// What a signal's default action is (`SIG_KERNEL_*_MASK` in linux-6.6/include/linux/signal.h:380).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DefaultAction {
    Terminate,
    /// Terminate with a core dump on Linux; V2 writes no core files ("core as terminate", wait status without
    /// the 0x80 bit).
    Core,
    Ignore,
    Stop,
    Continue,
}

pub fn default_action(sig: u32) -> DefaultAction {
    match sig {
        SIGCHLD | SIGURG | SIGWINCH => DefaultAction::Ignore,
        SIGCONT => DefaultAction::Continue,
        SIGSTOP | SIGTSTP | SIGTTIN | SIGTTOU => DefaultAction::Stop,
        SIGQUIT | SIGILL | SIGTRAP | SIGABRT | SIGBUS | SIGFPE | SIGSEGV | SIGXCPU | SIGXFSZ | SIGSYS => DefaultAction::Core,
        _ => DefaultAction::Terminate,
    }
}

/// The default action ends the process.
pub fn default_kills(sig: u32) -> bool {
    matches!(default_action(sig), DefaultAction::Terminate | DefaultAction::Core)
}

/// The bit of `sig` in a sigset (signal 1 = bit 0).
pub const fn bit(sig: u32) -> u64 {
    1u64 << (sig - 1)
}

/// SIGKILL and SIGSTOP: never blocked, caught or ignored (`sig_kernel_only`).
pub const UNBLOCKABLE: u64 = bit(SIGKILL) | bit(SIGSTOP);
/// `SIG_KERNEL_STOP_MASK`.
pub const STOP_MASK: u64 = bit(SIGSTOP) | bit(SIGTSTP) | bit(SIGTTIN) | bit(SIGTTOU);
/// `SYNCHRONOUS_MASK` (kernel/signal.c:204): delivered before the others so the frame names the faulting
/// instruction.
pub const SYNCHRONOUS_MASK: u64 = bit(SIGSEGV) | bit(SIGBUS) | bit(SIGILL) | bit(SIGTRAP) | bit(SIGFPE) | bit(SIGSYS);

pub fn valid(sig: u32) -> bool {
    (1..=NSIG).contains(&sig)
}

pub(crate) fn bit_checked(sig: u32) -> u64 {
    if valid(sig) { bit(sig) } else { 0 }
}

/// `struct sigaction` as the x86-64 kernel sees it (`struct k_sigaction` without the arch part): handler,
/// flags, restorer, mask; 32 bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SigAction {
    pub handler: u64,
    pub flags: u64,
    pub restorer: u64,
    pub mask: u64,
}

impl SigAction {
    pub const SIZE: usize = 32;
    pub fn from_bytes(b: &[u8; 32]) -> Self {
        let w = |i: usize| u64::from_le_bytes(b[i * 8..i * 8 + 8].try_into().unwrap_or([0; 8]));
        SigAction { handler: w(0), flags: w(1), restorer: w(2), mask: w(3) }
    }
    pub fn to_bytes(&self) -> [u8; 32] {
        let mut b = [0u8; 32];
        for (i, v) in [self.handler, self.flags, self.restorer, self.mask].into_iter().enumerate() {
            b[i * 8..i * 8 + 8].copy_from_slice(&v.to_le_bytes());
        }
        b
    }
    /// A handler runs (neither SIG_DFL nor SIG_IGN).
    pub fn is_handler(&self) -> bool {
        !matches!(self.handler, SIG_DFL | SIG_IGN)
    }
}

/// `stack_t` (24 bytes: ss_sp, ss_flags + padding, ss_size).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AltStack {
    pub sp: u64,
    pub flags: u32,
    pub size: u64,
}

impl AltStack {
    pub const SIZE: usize = 24;
    /// No alternate stack (a new thread's, and after execve).
    pub const DISABLED: AltStack = AltStack { sp: 0, flags: SS_DISABLE, size: 0 };
    pub fn from_bytes(b: &[u8; 24]) -> Self {
        AltStack {
            sp: u64::from_le_bytes(b[0..8].try_into().unwrap_or([0; 8])),
            flags: u32::from_le_bytes(b[8..12].try_into().unwrap_or([0; 4])),
            size: u64::from_le_bytes(b[16..24].try_into().unwrap_or([0; 8])),
        }
    }
    pub fn to_bytes(&self) -> [u8; 24] {
        let mut b = [0u8; 24];
        b[0..8].copy_from_slice(&self.sp.to_le_bytes());
        b[8..12].copy_from_slice(&self.flags.to_le_bytes());
        b[16..24].copy_from_slice(&self.size.to_le_bytes());
        b
    }

    /// `__on_sig_stack`: sp in (ss_sp, ss_sp + ss_size] (the stack grows down; its top is on it).
    pub fn contains(&self, sp: u64) -> bool {
        sp > self.sp && sp - self.sp <= self.size
    }

    /// `on_sig_stack`: with SS_AUTODISARM a thread is never "on" it (Linux: by construction it cannot be, and a
    /// corrupted sp near its end must not stop a signal).
    pub fn on(&self, sp: u64) -> bool {
        self.flags & SS_AUTODISARM == 0 && self.contains(sp)
    }

    /// `sas_ss_flags`: SS_DISABLE without a stack, SS_ONSTACK when on it, else 0.
    pub fn ss_flags(&self, sp: u64) -> u32 {
        if self.size == 0 {
            SS_DISABLE
        } else if self.on(sp) {
            SS_ONSTACK
        } else {
            0
        }
    }

    /// `do_sigaltstack` (kernel/signal.c:4206) with the caller's user stack pointer `sp`; returns the old stack
    /// as Linux reports it. sigreturn's `restore_altstack` calls it too and ignores the errors.
    pub fn change(&mut self, new: Option<AltStack>, sp: u64) -> Result<AltStack, Errno> {
        let cur = *self;
        let old = AltStack { sp: cur.sp, size: cur.size, flags: cur.ss_flags(sp) | (cur.flags & SS_FLAG_BITS) };
        if let Some(n) = new {
            if cur.on(sp) {
                return Err(EPERM);
            }
            let mode = n.flags & !SS_FLAG_BITS;
            if mode != SS_DISABLE && mode != SS_ONSTACK && mode != 0 {
                return Err(EINVAL);
            }
            if cur.sp == n.sp && cur.size == n.size && cur.flags == n.flags {
                return Ok(old);
            }
            if mode == SS_DISABLE {
                *self = AltStack { sp: 0, size: 0, flags: n.flags };
            } else {
                if n.size < MINSIGSTKSZ {
                    return Err(ENOMEM);
                }
                *self = n;
            }
        }
        Ok(old)
    }
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod timer_tests;
