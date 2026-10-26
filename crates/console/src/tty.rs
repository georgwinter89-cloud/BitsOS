//! The console terminal: the N_TTY line discipline (`ldisc`) with termios, its job control (`jobs`), and the
//! waiting a terminal does (IN-4). What `/dev/console`, `/dev/tty` and `/dev/tty0` read and write (FS-2's devfs
//! nodes) and what the monitor reads lines from.
//!
//! Keys arrive as the bytes a Linux VT sends (`keymap::key_bytes`: Enter is `\r`, Backspace DEL, arrows
//! `ESC [ A`...), so with the VT's start settings (`Termios::vt`: Linux's `tty_std_termios` plus IUTF8, as
//! linux-6.6/drivers/tty/vt/vt.c:3554-3556) a program sees exactly what it sees on a Linux console: cooked lines
//! with echo, Ctrl+C/Ctrl+\/Ctrl+Z raising SIGINT/SIGQUIT/SIGTSTP for the foreground process group, Ctrl+D as end
//! of file, Ctrl+S/Ctrl+Q stopping output; and raw mode, VMIN/VTIME and the rest after `tcsetattr`.
//!
//! Reads (`n_tty_read`, linux-6.6/drivers/tty/n_tty.c:2151) wait in slices of at most 50 ms that ask `abort`
//! (the reader's program was ended; B4: no wait without an end), and do the background check of job control
//! except on `/dev/console` (`job_control` :2117, `redirected_tty_write`). Writes run through OPOST and wait
//! while output is stopped.
//!
//! Locking: `tty.state` is a subsystem lock; under it only memory work. Echo goes to the caller's `draw` after
//! the lock is released and **before** the readers are woken (the monitor's answer never lands on the typed
//! line); signals go out after the lock too.

use crate::jobs::{self, Ctl, Procs, Send, SIGTTIN, SIGTTOU};
use crate::keymap::{self, Key};
use crate::ldisc::{Effects, Ldisc};
pub use crate::ldisc::{BUF_SIZE, Line};
use crate::termios::{Termios, Winsize, cc, flag};
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use ksync::{SpinLock, WaitQueue};
use uapi::errno::*;
use vfs::Errno;

struct St {
    ld: Ldisc,
    ctl: Ctl,
    /// Set by TIOCSWINSZ; None = the VT's own grid (the next font switch drops it again).
    winsize: Option<Winsize>,
    excl: bool,
    /// The process that last changed the termios (0: the kernel); see `sane_if_orphaned`.
    termios_pid: u32,
}

pub struct Tty {
    st: SpinLock<St>,
    /// VINTR count as readers may see it: published after the ^C echo is drawn (the monitor's fresh prompt comes
    /// after the echo, never before it).
    intr: core::sync::atomic::AtomicU64,
    /// Readers and poll: woken when input may be there (a `PollQueue` so poll/select/epoll wake at once, FS-4
    /// CR4).
    rq: vfs::poll::PollQueue,
    /// Writers waiting for stopped output.
    wq: WaitQueue,
    /// Where a terminal that is not the VT sends its echo (IN-7: a pty slave's master); None = the VT.
    port: Option<Arc<dyn Port>>,
}

/// The other end of a terminal that is not the VT (IN-7: a pty slave's master side).
pub trait Port: core::marker::Send + Sync {
    /// Echo and released held echo (the VT draws them on the screen; a pty puts them in the master's read
    /// queue).
    fn echo(&self, bytes: &[u8]);
    /// TIOCGDEV's `new_encode_dev` of this terminal.
    fn dev(&self) -> u32;
}

/// How long a blocked read or write sleeps before it asks whether its program is being ended.
const SLICE_NS: u64 = 50_000_000;

/// What `read_line_or_interrupt` saw.
#[derive(Debug, PartialEq, Eq)]
pub enum LineWait {
    Line(Line),
    /// VINTR was typed (the line was dropped).
    Interrupted,
    Timeout,
}

impl Tty {
    pub const fn new() -> Tty {
        Tty {
            st: SpinLock::named("tty.state", St { ld: Ldisc::new(Termios::vt()), ctl: Ctl { session: None, pgrp: None }, winsize: None, excl: false, termios_pid: 0 }),
            intr: core::sync::atomic::AtomicU64::new(0),
            rq: vfs::poll::PollQueue::new(),
            wq: WaitQueue::new(),
            port: None,
        }
    }

    /// A terminal that is not the VT (IN-7: a pty slave) with start settings `t` (no VT hardware bits, window size
    /// 0x0 until TIOCSWINSZ).
    pub fn with_port(t: Termios, port: Arc<dyn Port>) -> Tty {
        let mut tty = Tty::new();
        tty.st.get_mut().ld = Ldisc::new(t);
        tty.port = Some(port);
        tty
    }

    /// Not the VT (a pty slave).
    pub fn is_pty(&self) -> bool {
        self.port.is_some()
    }

    /// Wakes every reader, writer and poller (a hang-up: their next check sees it).
    pub fn wake_all(&self) {
        self.rq.wake_all();
        self.wq.wake_all();
    }

    /// TIOCGDEV: tty1 for the console, the pty's own number for a pty slave.
    pub fn dev(&self) -> u32 {
        self.port.as_ref().map(|p| p.dev()).unwrap_or(crate::ttyioctl::TTY1_DEV)
    }

    /// Echo from outside the line discipline's own path (TIOCSTI): to the VT or the pty's master.
    pub fn echo_out(&self, bytes: &[u8]) {
        match &self.port {
            Some(p) => p.echo(bytes),
            None => crate::vt::write_program(bytes),
        }
    }

    pub fn termios(&self) -> Termios {
        self.st.lock().ld.t
    }

    /// How many bytes OPOST makes of one written byte at most: 8 with XTABS (a tab to the next tab stop), 2 with
    /// ONLCR (`\n` to `\r\n`), else 1 (`do_output_char`, linux-6.6/drivers/tty/n_tty.c:417).
    pub fn output_growth(&self) -> usize {
        use crate::termios::flag::{ONLCR, OPOST, TABDLY, XTABS};
        let t = self.termios();
        if !t.o(OPOST) {
            1
        } else if t.oflag & TABDLY == XTABS {
            8
        } else if t.o(ONLCR) {
            2
        } else {
            1
        }
    }

    /// Sets the termios as TCSETS does (VT: the hardware bits stay; `copy_hw`) and wakes readers and writers.
    pub fn set_termios(&self, mut t: Termios, flush: bool) {
        let started = {
            let mut s = self.st.lock();
            let old = s.ld.t;
            t.load_speeds();
            if self.port.is_some() {
                // `pty_set_termios` (linux-6.6/drivers/tty/pty.c:276): CS8 and CREAD always, no parity.
                t.cflag = (t.cflag & !(flag::CSIZE | flag::PARENB)) | flag::CS8 | flag::CREAD;
            } else {
                t.copy_hw(&old);
            }
            if flush {
                s.ld.flush_input();
            }
            s.ld.set_termios(t)
        };
        if started {
            self.release_held_echo();
        }
        self.rq.wake_all();
        self.wq.wake_all();
    }

    /// Records which process set the termios (TCSETS and friends).
    pub fn note_termios_owner(&self, pid: u32) {
        self.st.lock().termios_pid = pid;
    }

    /// The program that changed the termios ended and no session holds the terminal: back to the VT's start
    /// settings (what a shell or getty would do; the monitor's line reading needs canonical mode). True when reset.
    pub fn sane_if_orphaned(&self, p: &dyn Procs) -> bool {
        let pid = self.st.lock().termios_pid;
        if pid == 0 || p.alive(pid) || self.owned() {
            return false;
        }
        let changed = self.termios() != Termios::vt();
        self.st.lock().termios_pid = 0;
        if changed {
            self.set_termios(Termios::vt(), false);
            klog::event!(Info, "console", "tty.sane", pid = pid; "the program that changed the terminal settings ended: back to the console's settings");
        }
        changed
    }

    /// `cfmakeraw` (true) or the VT's start settings (false); tests and the kernel's own users.
    pub fn set_raw(&self, raw: bool) {
        let mut t = self.termios();
        if raw {
            t.iflag &= !(flag::IGNBRK | flag::BRKINT | flag::PARMRK | flag::ISTRIP | flag::INLCR | flag::IGNCR | flag::ICRNL | flag::IXON);
            t.oflag &= !flag::OPOST;
            t.lflag &= !(flag::ECHO | flag::ECHONL | flag::ICANON | flag::ISIG | flag::IEXTEN);
            t.cflag = (t.cflag & !(flag::CSIZE | 0o400)) | flag::CS8;
            t.cc[cc::VMIN] = 1;
            t.cc[cc::VTIME] = 0;
        } else {
            t = Termios::vt();
        }
        self.set_termios(t, false);
    }

    /// Not canonical (a program switched ICANON off).
    pub fn is_raw(&self) -> bool {
        !self.st.lock().ld.canonical()
    }

    /// VINTR characters typed so far with ISIG on (the monitor compares counts).
    pub fn interrupts(&self) -> u64 {
        self.intr.load(core::sync::atomic::Ordering::Acquire)
    }

    /// poll's IN: a read would return now (VMIN bytes when VTIME is 0, as `input_available_p(tty, 1)`).
    pub fn has_input(&self) -> bool {
        self.st.lock().ld.available(true)
    }

    /// The line being typed (not yet entered).
    pub fn editing(&self) -> String {
        self.st.lock().ld.editing()
    }

    /// TIOCINQ.
    pub fn inq(&self) -> usize {
        self.st.lock().ld.inq()
    }

    /// Drops everything typed and not read (TCIFLUSH, tests).
    pub fn flush(&self) {
        self.st.lock().ld.flush_input();
    }

    pub fn wait_queue(&self) -> &WaitQueue {
        &self.rq
    }

    /// The readers' queue as poll/select/epoll hang on it.
    pub fn poll_queue(&self) -> &vfs::poll::PollQueue {
        &self.rq
    }

    // ---- job control state

    /// Runs `f` on the session state after a refresh (a session whose leader ended is dropped, with SIGHUP), then
    /// sends the signals `f` asked for. The process hooks are called without `tty.state` held (they take the
    /// process table's locks): `f` works on a copy that is stored only if nobody changed the state meanwhile,
    /// else it runs again on the new state.
    pub fn with_ctl<R>(&self, p: &dyn Procs, mut f: impl FnMut(&mut Ctl, &mut Send) -> R) -> R {
        loop {
            let before = self.st.lock().ctl;
            let mut c = before;
            let mut send = Send::default();
            c.refresh_as(p, &mut send, self.port.is_none());
            let r = f(&mut c, &mut send);
            let mut s = self.st.lock();
            if s.ctl == before {
                s.ctl = c;
                drop(s);
                send.deliver(p);
                return r;
            }
        }
    }

    /// The session and foreground group now.
    pub fn ctl(&self) -> Ctl {
        self.with_ctl(jobs::procs(), |c, _| *c)
    }

    /// A live session holds the terminal (the monitor stays away from its input meanwhile).
    pub fn owned(&self) -> bool {
        self.ctl().session.is_some()
    }

    pub fn winsize(&self) -> Option<Winsize> {
        self.st.lock().winsize
    }
    /// Sets the window size (TIOCSWINSZ, None = the VT's grid again); true when it changed (SIGWINCH is due).
    pub fn set_winsize(&self, w: Option<Winsize>, current: Winsize) -> bool {
        let mut s = self.st.lock();
        let before = s.winsize.unwrap_or(current);
        s.winsize = w;
        w.unwrap_or(current) != before
    }
    pub fn exclusive(&self) -> bool {
        self.st.lock().excl
    }
    pub fn set_exclusive(&self, on: bool) {
        self.st.lock().excl = on;
    }

    /// TCXONC TCOOFF/TCOON.
    pub fn tco(&self, stop: bool) {
        let started = self.st.lock().ld.tco(stop);
        if started {
            self.release_held_echo();
            self.wq.wake_all();
        }
    }

    pub fn output_stopped(&self) -> bool {
        self.st.lock().ld.stopped()
    }

    fn release_held_echo(&self) {
        let held = self.st.lock().ld.take_held_echo();
        if !held.is_empty() {
            self.echo_out(&held);
        }
    }

    // ---- input

    /// One key from the keyboard; returns what was echoed (tests; the VT uses `key_with`).
    pub fn key(&self, k: Key, injected: bool) -> String {
        let mut echo = String::new();
        self.key_with(k, injected, &mut |e| echo.push_str(&String::from_utf8_lossy(e)));
        echo
    }

    /// One key: its bytes go through the line discipline; `draw` gets the echo (no lock held) before readers are
    /// woken and signals go out.
    pub fn key_with(&self, k: Key, injected: bool, draw: &mut dyn FnMut(&[u8])) {
        let mut b = [0u8; 8];
        let bytes = keymap::key_bytes(k, &mut b);
        if !bytes.is_empty() {
            self.input(bytes, injected, draw);
        }
    }

    /// Bytes as if typed (keys, K_MEDIUMRAW codes, TIOCSTI).
    pub fn input(&self, bytes: &[u8], injected: bool, draw: &mut dyn FnMut(&[u8])) {
        self.input_inner(bytes, injected, draw, false);
    }

    /// `input` with flow control (IN-7, a pty master's writes): takes bytes only while the line discipline has
    /// room and returns how many it took (the rest waits in the pty's buffer, as in Linux's flip buffer).
    pub fn input_flow(&self, bytes: &[u8], injected: bool, draw: &mut dyn FnMut(&[u8])) -> usize {
        self.input_inner(bytes, injected, draw, true)
    }

    fn input_inner(&self, bytes: &[u8], injected: bool, draw: &mut dyn FnMut(&[u8]), flow: bool) -> usize {
        let (fx, dropped, intr, taken): (Effects, bool, u64, usize) = {
            let mut s = self.st.lock();
            let before = s.ld.dropped_runs;
            let (fx, taken) = if flow { s.ld.receive_flow(bytes, injected) } else { (s.ld.receive(bytes, injected), bytes.len()) };
            let d = s.ld.dropped_runs != before;
            (fx, d, s.ld.interrupts, taken)
        };
        if !fx.echo.is_empty() {
            draw(&fx.echo);
        }
        // Readers see the input only now, after its echo.
        self.st.lock().ld.publish();
        self.intr.fetch_max(intr, core::sync::atomic::Ordering::AcqRel);
        if fx.echo_lost {
            klog::event!(Warn, "console", "tty.echo_dropped", kept = crate::ldisc::HELD_ECHO_MAX;
                "output is stopped (Ctrl+S) and the held echo is full: the oldest echo is dropped");
        }
        if dropped {
            klog::event!(Warn, "console", "tty.input_dropped", injected = injected, buffer = BUF_SIZE;
                "the terminal's reader is not reading: its input buffer is full, keys are dropped");
        }
        if !fx.signals.is_empty() {
            let p = jobs::procs();
            self.with_ctl(p, |ctl, send| {
                if let Some(pg) = ctl.pgrp {
                    for &sig in &fx.signals {
                        send.push(pg, sig);
                    }
                }
            });
        }
        if fx.readable || !fx.signals.is_empty() {
            self.rq.wake_all();
        }
        if fx.started {
            self.wq.wake_all();
        }
        taken
    }

    /// The next finished line for the kernel (the monitor), waiting up to `timeout_ns`.
    pub fn read_line(&self, timeout_ns: u64) -> Option<Line> {
        match self.read_line_or_interrupt(timeout_ns, self.interrupts()) {
            LineWait::Line(l) => Some(l),
            _ => None,
        }
    }

    /// Waits up to `timeout_ns` for a line, or until VINTR is typed (the count moved past `seen`).
    pub fn read_line_or_interrupt(&self, timeout_ns: u64, seen: u64) -> LineWait {
        let ready = || self.interrupts() != seen || {
            let s = self.st.lock();
            s.ld.canonical() && s.ld.available(false)
        };
        self.rq.wait_until_timeout(ready, timeout_ns);
        if let Some(l) = self.st.lock().ld.take_line() {
            return LineWait::Line(l);
        }
        if self.interrupts() != seen { LineWait::Interrupted } else { LineWait::Timeout }
    }

    // ---- reading and writing for programs

    /// `read(2)`: `job` = the background check applies (every node but `/dev/console`).
    pub fn read(&self, out: &mut [u8], nonblock: bool, abort: &dyn Fn() -> bool, job: bool) -> Result<usize, Errno> {
        if job {
            let p = jobs::procs();
            let me = p.current();
            self.with_ctl(p, |ctl, send| ctl.check_change(me.as_ref(), p, SIGTTIN, send))?;
        }
        if out.is_empty() {
            return Ok(0);
        }
        // MIN and TIME are taken once, ICANON on every pass (n_tty_read: a tcsetattr by another thread switches a
        // blocked read between line and raw reading).
        let (minimum, inter_ns, mut deadline) = {
            let s = self.st.lock();
            if s.ld.canonical() {
                (0usize, 0u64, None)
            } else {
                let min = s.ld.t.c(cc::VMIN) as usize;
                let time = s.ld.t.c(cc::VTIME) as u64 * 100_000_000;
                if min > 0 { (min, time, None) } else { (1, 0, Some(time::now_ns().saturating_add(time))) }
            }
        };
        let mut n = 0;
        while n < out.len() {
            let canon;
            let got = {
                let mut s = self.st.lock();
                canon = s.ld.canonical();
                if !s.ld.available(false) {
                    None
                } else if canon {
                    Some(s.ld.read_canon(&mut out[n..]))
                } else {
                    Some(s.ld.read_raw(&mut out[n..]))
                }
            };
            if let Some(k) = got {
                n += k;
                if canon || n >= minimum {
                    break;
                }
                if inter_ns > 0 {
                    deadline = Some(time::now_ns().saturating_add(inter_ns));
                }
                continue;
            }
            let now = time::now_ns();
            if deadline.is_some_and(|d| now >= d) {
                break;
            }
            if nonblock {
                return if n > 0 { Ok(n) } else { Err(EAGAIN) };
            }
            if abort() {
                return if n > 0 { Ok(n) } else { Err(EINTR) };
            }
            let slice = deadline.map(|d| d.saturating_sub(now)).unwrap_or(SLICE_NS).clamp(1, SLICE_NS);
            self.rq.wait_until_timeout(|| self.st.lock().ld.available(false), slice);
        }
        Ok(n)
    }

    /// `write(2)` up to the device: the background check with TOSTOP (`job`), the wait while output is stopped,
    /// then OPOST. Returns the bytes for the screen.
    pub fn write(&self, data: &[u8], nonblock: bool, abort: &dyn Fn() -> bool, job: bool) -> Result<Vec<u8>, Errno> {
        if job && self.termios().l(flag::TOSTOP) {
            let p = jobs::procs();
            let me = p.current();
            self.with_ctl(p, |ctl, send| ctl.check_change(me.as_ref(), p, SIGTTOU, send))?;
        }
        loop {
            {
                let mut s = self.st.lock();
                if !s.ld.stopped() {
                    return Ok(s.ld.output(data));
                }
            }
            if nonblock {
                return Err(EAGAIN);
            }
            if abort() {
                return Err(EINTR);
            }
            self.wq.wait_until_timeout(|| !self.st.lock().ld.stopped(), SLICE_NS);
        }
    }
}

impl Default for Tty {
    fn default() -> Self {
        Self::new()
    }
}
