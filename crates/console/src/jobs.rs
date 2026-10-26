//! Job control of the console terminal (IN-4; linux-6.6/drivers/tty/tty_jobctrl.c): the controlling session and
//! the foreground process group, TIOCSCTTY/TIOCNOTTY/TIOCGPGRP/TIOCSPGRP/TIOCGSID, the background check that
//! sends SIGTTIN/SIGTTOU (`__tty_check_change`), and the signals the terminal raises (ISIG characters, SIGWINCH,
//! SIGHUP when the session leader is gone).
//!
//! The console sits below `proc`, so it reaches processes through `Procs`, installed by `console-cmds` (above
//! `proc`). Until AB-6's signal delivery, a signal reaches a process through `Process::send_signal` (AB-3: default
//! actions; a handled signal stays pending until AB-6 delivers it). "The caller's controlling terminal is this
//! tty" is "the caller's session is the tty's session" (Linux keeps it per process; only TIOCNOTTY by a non-leader
//! differs, see `notty`). With IN-7's ptys there are several terminals; a session holds at most one of them
//! (`pty::session_has_tty`, checked by TIOCSCTTY and the slave's open).

use crate::termios::ioc::*;
use ksync::OnceRef;
use uapi::errno::*;
use vfs::Errno;

pub const SIGHUP: u32 = 1;
pub const SIGCONT: u32 = 18;
pub const SIGSTOP: u32 = 19;
pub const SIGTTIN: u32 = 21;
pub const SIGTTOU: u32 = 22;
pub const SIGWINCH: u32 = 28;

/// The process making a terminal call.
#[derive(Clone, Copy, Debug)]
pub struct Caller {
    pub pid: u32,
    pub pgid: u32,
    pub sid: u32,
    pub uid: u32,
    pub subject: policy::Subject,
}

impl Caller {
    pub fn leader(&self) -> bool {
        self.pid == self.sid
    }
    /// CAP_SYS_ADMIN / CAP_SYS_TTY_CONFIG: root (no capabilities apart from uid 0 yet).
    pub fn privileged(&self) -> bool {
        self.uid == 0
    }
}

/// What the terminal needs from the process layer.
pub trait Procs: Sync {
    /// The calling process (None in kernel threads).
    fn current(&self) -> Option<Caller>;
    /// `kill_pgrp(pgrp, sig, 1)`: returns how many processes got it.
    fn signal_pgrp(&self, pgrp: u32, sig: u32) -> usize;
    /// `session_of_pgrp`: the session of a member of `pgrp`, else of process `pgrp` itself.
    fn session_of_pgrp(&self, pgrp: u32) -> Option<u32>;
    /// Process `pid` lives (not ended).
    fn alive(&self, pid: u32) -> bool;
    /// `is_ignored`: the caller blocks `sig` or its action is SIG_IGN.
    fn ignores(&self, sig: u32) -> bool;
    /// Copies the caller's memory at `addr` (false on a fault or without a caller).
    fn read_user(&self, addr: u64, buf: &mut [u8]) -> bool;
    /// Writes into the caller's memory at `addr` (false on a fault or without a caller).
    fn write_user(&self, _addr: u64, _data: &[u8]) -> bool {
        false
    }
    /// `is_current_pgrp_orphaned` for group `pgrp` (`will_become_orphaned_pgrp`, kernel/exit.c): no member has a
    /// parent in another group of the same session (children of init, here of the kernel, do not count).
    fn pgrp_orphaned(&self, _pgrp: u32) -> bool {
        false
    }
    /// A kernel signal to the one process `pid` (SEND_SIG_PRIV; a hangup's SIGHUP and SIGCONT for the session
    /// leader, IN-7). False when there is no such live process.
    fn signal_proc(&self, _pid: u32, _sig: u32) -> bool {
        false
    }
    /// A member of `pgrp` finished a group stop (SIGSTOP and friends).
    fn pgrp_stopped(&self, _pgrp: u32) -> bool {
        false
    }
}

/// No processes (kernel threads, host tests): nothing to signal, no caller.
pub struct NoProcs;
impl Procs for NoProcs {
    fn current(&self) -> Option<Caller> {
        None
    }
    fn signal_pgrp(&self, _pgrp: u32, _sig: u32) -> usize {
        0
    }
    fn session_of_pgrp(&self, _pgrp: u32) -> Option<u32> {
        None
    }
    fn alive(&self, _pid: u32) -> bool {
        false
    }
    fn ignores(&self, _sig: u32) -> bool {
        false
    }
    fn read_user(&self, _addr: u64, _buf: &mut [u8]) -> bool {
        false
    }
}
static NO_PROCS: NoProcs = NoProcs;

static PROCS: OnceRef<dyn Procs> = OnceRef::new();

/// Installed once by `console-cmds` (above `proc`).
pub fn set_procs(p: &'static dyn Procs) {
    if !PROCS.set(p) {
        klog::event!(Warn, "console", "jobs.procs_twice"; "a process hook is installed already; this one is not used");
    }
}

pub fn procs() -> &'static dyn Procs {
    PROCS.get().unwrap_or(&NO_PROCS)
}

/// The terminal's controlling session and foreground process group (`tty->ctrl`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Ctl {
    pub session: Option<u32>,
    pub pgrp: Option<u32>,
}

/// Signals to send once the terminal's lock is released.
#[derive(Default, Debug)]
pub struct Send {
    pub to: alloc::vec::Vec<(u32, u32)>,
}

impl Send {
    pub fn push(&mut self, pgrp: u32, sig: u32) {
        self.to.push((pgrp, sig));
    }
    pub fn deliver(self, p: &dyn Procs) {
        for (pg, sig) in self.to {
            let n = p.signal_pgrp(pg, sig);
            klog::event!(Debug, "console", "tty.signal", pgrp = pg, signal = uapi::ab::signal_names::name(sig), processes = n);
        }
    }
}

impl Ctl {
    /// The session leader is gone: the terminal loses its session (`disassociate_ctty(1)`), its foreground group
    /// gets SIGHUP and SIGCONT.
    pub fn refresh(&mut self, p: &dyn Procs, out: &mut Send) {
        self.refresh_as(p, out, true);
    }

    /// `refresh` for a VT (`cont`: SIGHUP and SIGCONT) or a pty slave (SIGHUP only: `disassociate_ctty(1)` on a
    /// pty kills the foreground group with SIGHUP alone, linux-6.6/drivers/tty/tty_jobctrl.c:274-284).
    pub fn refresh_as(&mut self, p: &dyn Procs, out: &mut Send, cont: bool) {
        if let Some(sid) = self.session
            && !p.alive(sid)
        {
            if let Some(pg) = self.pgrp {
                out.push(pg, SIGHUP);
                if cont {
                    out.push(pg, SIGCONT);
                }
            }
            klog::event!(Info, "console", "tty.session_ended", session = sid; "the session leader ended: the terminal has no controlling session");
            *self = Ctl::default();
        }
    }

    /// The console is `me`'s controlling terminal.
    pub fn is_ctty(&self, me: &Caller) -> bool {
        self.session == Some(me.sid)
    }

    /// `__tty_check_change` (tty_jobctrl.c:36): a background process of this terminal's session asks to read
    /// (SIGTTIN) or to change it or write with TOSTOP (SIGTTOU). Err(EIO) when SIGTTIN is ignored or the caller's
    /// group is orphaned (nobody could continue it after the stop); Err(ERESTARTSYS) after the signal went to its
    /// group (restarted after the stop, or EINTR when a handler without SA_RESTART ran; AB-6, INT-A2a).
    pub fn check_change(&self, me: Option<&Caller>, p: &dyn Procs, sig: u32, out: &mut Send) -> Result<(), Errno> {
        let Some(me) = me else { return Ok(()) };
        if !self.is_ctty(me) {
            return Ok(());
        }
        match self.pgrp {
            Some(fg) if fg != me.pgid => {
                if p.ignores(sig) {
                    return if sig == SIGTTIN { Err(EIO) } else { Ok(()) };
                }
                if p.pgrp_orphaned(me.pgid) {
                    klog::event!(Debug, "console", "tty.orphaned_background", pgrp = me.pgid, signal = sig);
                    return Err(EIO);
                }
                out.push(me.pgid, sig);
                Err(uapi::ab::siginfo::ERESTARTSYS)
            }
            _ => Ok(()),
        }
    }

    /// `tiocsctty`.
    pub fn sctty(&mut self, me: Option<&Caller>, arg: u64, readable: bool) -> Result<(), Errno> {
        let Some(me) = me else { return Err(EPERM) };
        if me.leader() && self.session == Some(me.sid) {
            return Ok(());
        }
        // A session leader with another controlling terminal cannot exist here (there is one terminal).
        if !me.leader() {
            return Err(EPERM);
        }
        if let Some(other) = self.session {
            if arg == 1 && me.privileged() {
                klog::event!(Info, "console", "tty.stolen", from = other, to = me.sid);
            } else {
                return Err(EPERM);
            }
        }
        if !readable && !me.privileged() {
            return Err(EPERM);
        }
        self.session = Some(me.sid);
        self.pgrp = Some(me.pgid);
        klog::event!(Info, "console", "tty.ctty", session = me.sid, pgrp = me.pgid, pid = me.pid);
        Ok(())
    }

    /// The kernel makes `sid`/`pgrp` the terminal's session (the monitor's `run`, as a shell would with
    /// setsid + TIOCSCTTY for its child).
    pub fn assign(&mut self, sid: u32, pgrp: u32) {
        self.session = Some(sid);
        self.pgrp = Some(pgrp);
    }

    /// TIOCNOTTY. A leader gives the terminal up for its session (SIGHUP and SIGCONT to the foreground group,
    /// `disassociate_ctty(0)`); a non-leader would lose only its own controlling terminal, which BitsOS does not
    /// keep per process: 0 without a change.
    pub fn notty(&mut self, me: Option<&Caller>, out: &mut Send) -> Result<(), Errno> {
        let Some(me) = me else { return Err(ENOTTY) };
        if !self.is_ctty(me) {
            return Err(ENOTTY);
        }
        if me.leader() {
            if let Some(pg) = self.pgrp {
                out.push(pg, SIGHUP);
                out.push(pg, SIGCONT);
            }
            *self = Ctl::default();
        }
        Ok(())
    }

    /// TIOCGPGRP: ENOTTY unless it is the caller's terminal; 0 without a foreground group.
    pub fn gpgrp(&self, me: Option<&Caller>) -> Result<u32, Errno> {
        match me {
            Some(me) if self.is_ctty(me) => Ok(self.pgrp.unwrap_or(0)),
            _ => Err(ENOTTY),
        }
    }

    /// TIOCSPGRP after the background check and the read of `pgrp`.
    pub fn spgrp(&mut self, me: Option<&Caller>, p: &dyn Procs, pgrp: i32) -> Result<(), Errno> {
        if pgrp < 0 {
            return Err(EINVAL);
        }
        let Some(me) = me else { return Err(ENOTTY) };
        if !self.is_ctty(me) {
            return Err(ENOTTY);
        }
        match p.session_of_pgrp(pgrp as u32) {
            None => Err(ESRCH),
            Some(s) if s != me.sid => Err(EPERM),
            Some(_) => {
                self.pgrp = Some(pgrp as u32);
                Ok(())
            }
        }
    }

    /// TIOCGSID.
    pub fn gsid(&self, me: Option<&Caller>) -> Result<u32, Errno> {
        match (me, self.session) {
            (Some(me), Some(s)) if self.is_ctty(me) => Ok(s),
            _ => Err(ENOTTY),
        }
    }

    /// The VT ioctls' permission (`vt_ioctl`: the caller's controlling terminal, or CAP_SYS_TTY_CONFIG).
    pub fn vt_perm(&self, me: Option<&Caller>) -> bool {
        me.is_some_and(|m| self.is_ctty(m) || m.privileged())
    }
}

/// Which ioctls do the background check first (`tty_ioctl`'s prep and the mode ioctls' `set_termios`,
/// `tty_perform_flush`, TCXONC).
pub fn checks_change(cmd: u64) -> bool {
    matches!(
        cmd,
        TCSETS | TCSETSW | TCSETSF | TCSETS2 | TCSETSW2 | TCSETSF2 | TCSETA | TCSETAW | TCSETAF | TCSBRK | TCSBRKP | TIOCSBRK | TIOCCBRK | TCFLSH | TCXONC | TIOCSETD
    )
}
