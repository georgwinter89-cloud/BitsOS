//! Host tests of IN-4: the N_TTY line discipline against linux-6.6/drivers/tty/n_tty.c's rules, the termios
//! layouts, the terminal ioctls with a fake process layer and fake user memory, and evdev's client ring, bit
//! queries and ioctls on a test sink.

use crate::evdev::{self, AbsInfo, Caps, DeviceInfo, EV_ABS, EV_KEY, EV_LED, EV_REL, EV_SYN, EvdevSink, InputEvent, SYN_REPORT};
use crate::evdevfs::{self, Clock, EVENT_SIZE, Handle, Ring, ioc};
use crate::jobs::{Caller, Procs};
use crate::ldisc::{Effects, Ldisc, SIGINT, SIGQUIT, SIGTSTP};
use crate::termios::{Termios, Winsize, cc::*, flag::*, ioc::*};
use crate::tty::Tty;
use crate::ttyioctl;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec;
use uapi::ab::siginfo::ERESTARTSYS;
use alloc::vec::Vec;
use core::cell::RefCell;
use ksync::SpinLock;
use uapi::errno::*;
use vfs::sys::UserMem;

// ---- fakes

/// User memory: one buffer at `BASE`; everything else faults.
pub(crate) struct Mem(RefCell<Vec<u8>>);
pub(crate) const BASE: u64 = 0x1000;
impl Mem {
    pub(crate) fn new() -> Mem {
        Mem(RefCell::new(vec![0xEE; 4096]))
    }
    pub(crate) fn at(&self, off: usize, n: usize) -> Vec<u8> {
        self.0.borrow()[off..off + n].to_vec()
    }
    pub(crate) fn put(&self, off: usize, b: &[u8]) {
        self.0.borrow_mut()[off..off + b.len()].copy_from_slice(b);
    }
    pub(crate) fn i32_at(&self, off: usize) -> i32 {
        let b = self.at(off, 4);
        i32::from_le_bytes([b[0], b[1], b[2], b[3]])
    }
}
impl UserMem for Mem {
    fn read(&self, addr: u64, buf: &mut [u8]) -> bool {
        let m = self.0.borrow();
        match addr.checked_sub(BASE).and_then(|o| Some((o as usize, (o as usize).checked_add(buf.len())?))) {
            Some((o, end)) if end <= m.len() => {
                buf.copy_from_slice(&m[o..end]);
                true
            }
            _ => false,
        }
    }
    fn write(&self, addr: u64, data: &[u8]) -> bool {
        let mut m = self.0.borrow_mut();
        match addr.checked_sub(BASE).and_then(|o| Some((o as usize, (o as usize).checked_add(data.len())?))) {
            Some((o, end)) if end <= m.len() => {
                m[o..end].copy_from_slice(data);
                true
            }
            _ => false,
        }
    }
    fn read_cstr(&self, _addr: u64, _max: usize) -> Result<Vec<u8>, vfs::Errno> {
        Err(EFAULT)
    }
}

/// Processes: (pid, pgid, sid), the caller, which signals it ignores, the signals sent.
pub(crate) struct Fake {
    pub(crate) procs: SpinLock<Vec<(u32, u32, u32)>>,
    me: SpinLock<Option<u32>>,
    pub(crate) ignored: SpinLock<Vec<u32>>,
    sent: SpinLock<Vec<(u32, u32)>>,
    pub(crate) uid: u32,
    /// Process groups that are orphaned.
    pub(crate) orphaned: SpinLock<Vec<u32>>,
}
impl Fake {
    pub(crate) fn new(procs: &[(u32, u32, u32)], me: u32) -> Fake {
        Fake {
            procs: SpinLock::new(procs.to_vec()),
            me: SpinLock::new(Some(me)),
            ignored: SpinLock::new(Vec::new()),
            sent: SpinLock::new(Vec::new()),
            uid: 1000,
            orphaned: SpinLock::new(Vec::new()),
        }
    }
    pub(crate) fn caller(&self, pid: u32) -> Option<Caller> {
        self.procs.lock().iter().find(|p| p.0 == pid).map(|&(pid, pgid, sid)| Caller {
            pid,
            pgid,
            sid,
            uid: self.uid,
            subject: policy::Subject { program: policy::ProgramId(9), pid, uid: self.uid },
        })
    }
    pub(crate) fn with_uid(mut self, uid: u32) -> Fake {
        self.uid = uid;
        self
    }
    pub(crate) fn be(&self, pid: u32) {
        *self.me.lock() = Some(pid);
    }
    pub(crate) fn sent(&self) -> Vec<(u32, u32)> {
        core::mem::take(&mut *self.sent.lock())
    }
}
impl Procs for Fake {
    fn current(&self) -> Option<Caller> {
        let me = (*self.me.lock())?;
        self.caller(me)
    }
    fn signal_pgrp(&self, pgrp: u32, sig: u32) -> usize {
        self.sent.lock().push((pgrp, sig));
        self.procs.lock().iter().filter(|p| p.1 == pgrp).count()
    }
    fn session_of_pgrp(&self, pgrp: u32) -> Option<u32> {
        let p = self.procs.lock();
        p.iter().find(|x| x.1 == pgrp).or_else(|| p.iter().find(|x| x.0 == pgrp)).map(|x| x.2)
    }
    fn alive(&self, pid: u32) -> bool {
        self.procs.lock().iter().any(|p| p.0 == pid)
    }
    fn ignores(&self, sig: u32) -> bool {
        self.ignored.lock().contains(&sig)
    }
    fn read_user(&self, _addr: u64, _buf: &mut [u8]) -> bool {
        false
    }
    fn pgrp_orphaned(&self, pgrp: u32) -> bool {
        self.orphaned.lock().contains(&pgrp)
    }
}

pub(crate) fn io(t: &Tty, p: &Fake, m: &Mem, cmd: u64, arg: u64) -> Result<i64, vfs::Errno> {
    ttyioctl::ioctl(t, cmd, arg, m, p)
}

// ---- termios layouts

#[test]
fn vt_termios_is_linux_std_plus_iutf8() {
    let t = Termios::vt().to_termios();
    let w = |i: usize| u32::from_le_bytes([t[i], t[i + 1], t[i + 2], t[i + 3]]);
    // tty_std_termios: ICRNL|IXON (+IUTF8), OPOST|ONLCR, B38400|CS8|CREAD|HUPCL, ISIG|ICANON|ECHO|ECHOE|ECHOK|ECHOCTL|ECHOKE|IEXTEN.
    assert_eq!((w(0), w(4), w(8), w(12)), (0o400 | 0o2000 | 0o40000, 0o5, 0o2277, 0o105073));
    assert_eq!(t[16], 0, "c_line N_TTY");
    assert_eq!(&t[17..36], &[3, 0x1C, 0x7F, 0x15, 4, 0, 1, 0, 0x11, 0x13, 0x1A, 0, 0x12, 0x0F, 0x17, 0x16, 0, 0, 0]);
    let t2 = Termios::vt().to_termios2();
    assert_eq!(&t2[36..44], &[0x00, 0x96, 0, 0, 0x00, 0x96, 0, 0], "38400 in both speed fields");
    let mut x = Termios::vt();
    x.set_from_termio(&Termios::std().to_termio());
    assert_eq!(x.iflag, Termios::std().iflag, "termio sets the low 16 bits (IUTF8 is 0o40000, inside them)");
    assert_eq!(x.cc[VMIN], 1);
}

#[test]
fn a_vt_keeps_its_hardware_bits() {
    // tty_termios_copy_hw: only HUPCL, CREAD, CLOCAL change; speeds stay.
    let old = Termios::vt();
    let mut new = old;
    new.cflag = CLOCAL | 0o60; // CS8, no CREAD, no HUPCL, B0
    new.ispeed = 9600;
    new.copy_hw(&old);
    assert_eq!(new.cflag, (old.cflag & !(HUPCL | CREAD | CLOCAL)) | CLOCAL);
    assert_eq!(new.ispeed, 38400);
    let mut b = Termios::vt();
    b.cflag = (b.cflag & !CBAUD) | 0o10002; // B115200
    b.load_speeds();
    assert_eq!((b.ospeed, b.ispeed), (115200, 115200));
}

// ---- the line discipline

fn ld() -> Ldisc {
    let mut l = Ldisc::new(Termios::vt());
    l.publish();
    l
}
fn feed(l: &mut Ldisc, bytes: &[u8]) -> Effects {
    let fx = l.receive(bytes, false);
    l.publish();
    fx
}
fn line(l: &mut Ldisc) -> Vec<u8> {
    let mut b = [0u8; 4096];
    let n = l.read_canon(&mut b);
    b[..n].to_vec()
}

#[test]
fn canonical_input_echo_and_lines() {
    let mut l = ld();
    let fx = feed(&mut l, b"ab\r");
    assert_eq!(fx.echo, b"ab\r\n", "ICRNL turns Enter into LF; ONLCR echoes it as CR LF");
    assert_eq!(line(&mut l), b"ab\n");
    // ECHOCTL: control characters echo as ^X (not TAB).
    let fx = feed(&mut l, &[1, b'\t']);
    assert_eq!(fx.echo, b"^A\t");
    // VERASE over the TAB goes back to where it started (6 columns after ^A), then ^A takes two columns.
    let fx = feed(&mut l, &[0x7F, 0x7F]);
    assert_eq!(fx.echo, b"\x08\x08\x08\x08\x08\x08\x08 \x08\x08 \x08");
    // A UTF-8 character is erased whole (IUTF8): one column.
    let fx = feed(&mut l, "xü".as_bytes());
    assert_eq!(fx.echo, "xü".as_bytes());
    let fx = feed(&mut l, &[0x7F]);
    assert_eq!(fx.echo, b"\x08 \x08");
    assert_eq!(l.editing(), "x");
    // VWERASE (IEXTEN): back over the blanks and then the word (ALTWERASE: `_` is part of a word).
    feed(&mut l, b"y foo_1 ");
    feed(&mut l, &[0x17]);
    assert_eq!(l.editing(), "xy ");
    // VKILL with ECHOK+ECHOKE+ECHOE: erased character by character.
    let fx = feed(&mut l, &[0x15]);
    assert_eq!(fx.echo, b"\x08 \x08\x08 \x08\x08 \x08");
    assert_eq!(l.editing(), "");
}

#[test]
fn canonical_eof_lnext_reprint() {
    let mut l = ld();
    feed(&mut l, b"ab\x04");
    assert_eq!(line(&mut l), b"ab", "EOF ends the line without a byte");
    feed(&mut l, b"\x04");
    assert_eq!(line(&mut l), b"", "EOF on an empty line: read returns 0");
    // canon_skip_eof: a read that fills the buffer exactly in front of the EOF eats it.
    feed(&mut l, b"abc\x04x\r");
    let mut b3 = [0u8; 3];
    assert_eq!(l.read_canon(&mut b3), 3);
    assert_eq!(line(&mut l), b"x\n", "the EOF after abc was skipped");
    // VLNEXT takes the next character as it is (here VINTR), echoes ^\b first.
    let fx = feed(&mut l, &[0x16]);
    assert_eq!(fx.echo, b"^\x08");
    let fx = feed(&mut l, &[3]);
    assert!(fx.signals.is_empty());
    assert_eq!(fx.echo, b"^C");
    feed(&mut l, b"\r");
    assert_eq!(line(&mut l), b"\x03\n");
    // VREPRINT: ^R, newline, the line again.
    feed(&mut l, b"hi");
    let fx = feed(&mut l, &[0x12]);
    assert_eq!(fx.echo, b"^R\r\nhi");
    // Only finished lines are readable; a canonical read stops after one line.
    feed(&mut l, b"\rmore\r");
    assert_eq!(l.inq(), 8);
    assert_eq!(line(&mut l), b"hi\n");
    assert_eq!(line(&mut l), b"more\n");
}

#[test]
fn signals_flush_and_flow_control() {
    let mut l = ld();
    feed(&mut l, b"abc");
    let fx = feed(&mut l, &[3]);
    assert_eq!(fx.signals, [SIGINT]);
    assert_eq!(fx.echo, b"^C");
    assert_eq!(l.pending(), 0, "ISIG flushes the input");
    assert_eq!(feed(&mut l, &[0x1C]).signals, [SIGQUIT]);
    assert_eq!(feed(&mut l, &[0x1A]).signals, [SIGTSTP]);
    // NOFLSH keeps the input.
    let mut t = l.t;
    t.lflag |= NOFLSH;
    l.set_termios(t);
    feed(&mut l, b"keep");
    feed(&mut l, &[3]);
    assert_eq!(l.editing(), "keep");
    // Without ISIG ^C is data.
    t.lflag &= !ISIG;
    l.set_termios(t);
    l.flush_input();
    assert!(feed(&mut l, b"\x03\r").signals.is_empty());
    assert_eq!(line(&mut l), b"\x03\n");
    // IXON: ^S stops output (echo is held), ^Q starts it and hands the held echo over.
    let fx = feed(&mut l, &[0x13]);
    assert!(fx.echo.is_empty() && l.stopped());
    let fx = feed(&mut l, b"z");
    assert!(fx.echo.is_empty(), "echo held while stopped");
    let fx = feed(&mut l, &[0x11]);
    assert!(fx.started && !l.stopped());
    assert_eq!(fx.echo, b"z");
    // IXANY: any character starts output.
    t.iflag |= IXANY;
    l.set_termios(t);
    feed(&mut l, &[0x13]);
    let fx = feed(&mut l, b"q");
    assert!(fx.started && fx.echo == b"q");
    // Clearing IXON while stopped starts output (n_tty_set_termios).
    feed(&mut l, &[0x13]);
    t.iflag &= !IXON;
    assert!(l.set_termios(t));
}

#[test]
fn raw_mode_and_canon_switch() {
    let tty = Tty::new();
    tty.set_raw(true);
    let none = || false;
    tty.input(b"\x03\x7fA\r", false, &mut |_| {});
    let mut b = [0u8; 8];
    assert_eq!(tty.read(&mut b, true, &none, false), Ok(4), "cfmakeraw: bytes as typed, no signals, no CR mapping");
    assert_eq!(&b[..4], b"\x03\x7fA\r");
    // VMIN 0 VTIME 0: returns at once with what is there, 0 when nothing (not EAGAIN, even when non-blocking).
    let mut t = tty.termios();
    t.cc[VMIN] = 0;
    tty.set_termios(t, false);
    assert_eq!(tty.read(&mut b, true, &none, false), Ok(0));
    assert_eq!(tty.read(&mut b, false, &none, false), Ok(0));
    // VMIN 2: poll's IN needs two bytes; a non-blocking read takes what is there.
    t.cc[VMIN] = 2;
    tty.set_termios(t, false);
    tty.input(b"x", false, &mut |_| {});
    assert!(!tty.has_input(), "poll wants VMIN bytes");
    assert_eq!(tty.read(&mut b, true, &none, false), Ok(1));
    // (VMIN 0 VTIME 1, a timed wait, is a ktest: the host has no clock that runs.)
    // Back to canonical with data pending: it becomes a line at once (n_tty_set_termios's push).
    tty.input(b"pend", false, &mut |_| {});
    tty.set_raw(false);
    assert_eq!(tty.read(&mut b, true, &none, false), Ok(4));
    assert_eq!(&b[..4], b"pend");
    assert!(tty.read(&mut b, true, &none, false) == Err(EAGAIN));
}

#[test]
fn output_processing() {
    let mut l = ld();
    assert_eq!(l.output(b"a\nb"), b"a\r\nb", "ONLCR");
    assert_eq!(l.column, 1);
    let mut t = l.t;
    t.oflag |= OCRNL | ONOCR | XTABS | OLCUC;
    t.oflag &= !ONLCR;
    l.set_termios(t);
    assert_eq!(l.output(b"\r"), b"\n", "OCRNL");
    l.output(b"\x08");
    assert_eq!(l.column, 0);
    assert_eq!(l.output(b"\r"), b"", "ONOCR: no CR in column 0");
    assert_eq!(l.output(b"ab\tc"), b"AB      C", "XTABS and OLCUC");
    t.oflag &= !OPOST;
    l.set_termios(t);
    assert_eq!(l.output(b"a\n\t"), b"a\n\t", "without OPOST unchanged");
}

#[test]
fn full_buffer_rules() {
    let mut l = ld();
    let mut t = l.t;
    t.lflag &= !(ICANON | ECHO);
    l.set_termios(t);
    let big = vec![b'r'; 5000];
    let fx = feed(&mut l, &big);
    assert!(fx.dropped);
    assert_eq!(l.pending(), crate::ldisc::BUF_SIZE - 1, "non-canonical: the buffer keeps one byte free");
    assert_eq!(l.dropped_runs, 1, "one run of drops");
}

#[test]
fn input_is_published_after_its_echo() {
    let mut l = Ldisc::new(Termios::vt());
    l.receive(b"ls\r", false);
    assert!(!l.available(false), "not before publish");
    l.publish();
    assert!(l.available(false));
}

// ---- terminal ioctls

#[test]
fn termios_ioctls_read_and_write_exact_sizes() {
    let tty = Tty::new();
    let p = Fake::new(&[(100, 100, 100)], 100);
    let m = Mem::new();
    assert_eq!(io(&tty, &p, &m, TCGETS, BASE), Ok(0));
    assert_eq!(m.at(0, 36), Termios::vt().to_termios());
    assert_eq!(m.at(36, 4), [0xEE; 4], "TCGETS writes exactly 36 bytes");
    assert_eq!(io(&tty, &p, &m, TCGETS, 0x10), Err(EFAULT));
    // TCSETS: raw-ish, read back; the VT keeps its cflag hardware bits.
    let mut t = Termios::vt();
    t.lflag &= !(ICANON | ECHO);
    t.cc[VMIN] = 0;
    t.cflag = CREAD | 0o60 | 0o13; // B9600: not taken by a VT
    m.put(100, &t.to_termios());
    assert_eq!(io(&tty, &p, &m, TCSETSF, BASE + 100), Ok(0));
    assert_eq!(tty.termios().lflag, t.lflag);
    assert_eq!(tty.termios().cflag, (Termios::vt().cflag & !(HUPCL | CREAD | CLOCAL)) | CREAD);
    assert_eq!(io(&tty, &p, &m, TCSETS, 0x10), Err(EFAULT));
    assert_eq!(io(&tty, &p, &m, TCGETS2, BASE + 200), Ok(0));
    assert_eq!(m.at(236, 8), [0x00, 0x96, 0, 0, 0x00, 0x96, 0, 0]);
    assert_eq!(io(&tty, &p, &m, TCGETA, BASE + 300), Ok(0));
    assert_eq!(m.at(318, 2), [0xEE; 2], "termio is 18 bytes");
    // Window size: set and read back.
    m.put(400, &Winsize { rows: 50, cols: 132, xpixel: 1, ypixel: 2 }.to_bytes());
    assert_eq!(io(&tty, &p, &m, TIOCSWINSZ, BASE + 400), Ok(0));
    assert_eq!(io(&tty, &p, &m, TIOCGWINSZ, BASE + 410), Ok(0));
    assert_eq!(m.at(410, 8), Winsize { rows: 50, cols: 132, xpixel: 1, ypixel: 2 }.to_bytes());
    // FIONREAD, the line discipline number, flush and flow arguments, unknown commands.
    tty.set_raw(true);
    tty.input(b"abc", false, &mut |_| {});
    assert_eq!(io(&tty, &p, &m, TIOCINQ, BASE + 500), Ok(0));
    assert_eq!(m.i32_at(500), 3);
    assert_eq!(io(&tty, &p, &m, TCFLSH, TCIFLUSH), Ok(0));
    assert_eq!(io(&tty, &p, &m, TIOCINQ, BASE + 500), Ok(0));
    assert_eq!(m.i32_at(500), 0);
    assert_eq!(io(&tty, &p, &m, TCFLSH, 7), Err(EINVAL));
    assert_eq!(io(&tty, &p, &m, TCXONC, TCOOFF), Ok(0));
    assert!(tty.output_stopped());
    assert_eq!(io(&tty, &p, &m, TCXONC, TCOON), Ok(0));
    assert!(!tty.output_stopped());
    assert_eq!(io(&tty, &p, &m, TCXONC, 9), Err(EINVAL));
    assert_eq!(io(&tty, &p, &m, TIOCGETD, BASE + 600), Ok(0));
    assert_eq!(m.i32_at(600), 0);
    m.put(604, &1i32.to_le_bytes());
    assert_eq!(io(&tty, &p, &m, TIOCSETD, BASE + 604), Err(EINVAL));
    assert_eq!(io(&tty, &p, &m, TCSBRK, 1), Ok(0));
    assert_eq!(io(&tty, &p, &m, 0x5499, BASE), Err(ENOTTY));
    assert_eq!(io(&tty, &p, &m, TIOCSTI, BASE), Err(EIO), "legacy_tiocsti = 0 as on CachyOS");
}

#[test]
fn job_control_ioctls() {
    let tty = Tty::new();
    // Session 100 (leader 100 in group 100, process 101 in group 105), process 200 in its own session.
    let p = Fake::new(&[(100, 100, 100), (101, 105, 100), (200, 200, 200)], 101);
    let m = Mem::new();
    assert_eq!(io(&tty, &p, &m, TIOCGPGRP, BASE), Err(ENOTTY), "not the caller's terminal");
    assert_eq!(io(&tty, &p, &m, TIOCSCTTY, 0), Err(EPERM), "not a session leader");
    p.be(100);
    assert_eq!(io(&tty, &p, &m, TIOCSCTTY, 0), Ok(0));
    assert_eq!(tty.with_ctl(&p, |c, _| *c).session, Some(100));
    assert_eq!(io(&tty, &p, &m, TIOCSCTTY, 0), Ok(0), "again: nothing to do");
    assert_eq!(io(&tty, &p, &m, TIOCGPGRP, BASE), Ok(0));
    assert_eq!(m.i32_at(0), 100);
    assert_eq!(io(&tty, &p, &m, TIOCGSID, BASE), Ok(0));
    assert_eq!(m.i32_at(0), 100);
    // Another session cannot take it (without privilege).
    p.be(200);
    assert_eq!(io(&tty, &p, &m, TIOCSCTTY, 1), Err(EPERM));
    assert_eq!(io(&tty, &p, &m, TIOCGSID, BASE), Err(ENOTTY));
    // TIOCSPGRP: a foreign group EPERM, none ESRCH, negative EINVAL, the own session's group 0.
    p.be(100);
    for (pg, want) in [(200i32, Err(EPERM)), (999, Err(ESRCH)), (-1, Err(EINVAL)), (105, Ok(0))] {
        m.put(8, &pg.to_le_bytes());
        assert_eq!(io(&tty, &p, &m, TIOCSPGRP, BASE + 8), want, "TIOCSPGRP {pg}");
    }
    assert_eq!(tty.with_ctl(&p, |c, _| *c).pgrp, Some(105));
    assert_eq!(io(&tty, &p, &m, TIOCSPGRP, 0x10), Err(ERESTARTSYS), "the leader is in the background now: SIGTTOU first");
    p.sent();
    // TCSETS from the background: SIGTTOU to the caller's group, -ERESTARTSYS (restarted after the stop, AB-6).
    m.put(100, &Termios::vt().to_termios());
    assert_eq!(io(&tty, &p, &m, TCSETS, BASE + 100), Err(ERESTARTSYS));
    assert_eq!(p.sent(), [(100, 22)]);
    // Ignored SIGTTOU: it goes ahead.
    p.ignored.lock().push(22);
    assert_eq!(io(&tty, &p, &m, TCSETS, BASE + 100), Ok(0));
    p.ignored.lock().clear();
    // VT ioctls need the controlling terminal.
    p.be(200);
    assert_eq!(io(&tty, &p, &m, KDSKBMODE, K_OFF), Err(EPERM));
    p.be(101);
    assert_eq!(io(&tty, &p, &m, KDSKBMODE, 7), Err(EINVAL));
    assert_eq!(io(&tty, &p, &m, KDGKBMODE, BASE + 20), Ok(0));
    assert_eq!(m.i32_at(20), K_UNICODE as i32);
    assert_eq!(io(&tty, &p, &m, KDGKBTYPE, BASE + 24), Ok(0));
    assert_eq!(m.at(24, 2), [KB_101, 0xEE]);
    // TIOCNOTTY by the leader: SIGHUP and SIGCONT for the foreground group, the terminal is free.
    p.be(100);
    p.sent();
    assert_eq!(io(&tty, &p, &m, TIOCNOTTY, 0), Ok(0));
    assert_eq!(p.sent(), [(105, 1), (105, 18)]);
    assert_eq!(tty.with_ctl(&p, |c, _| *c).session, None);
    // A session whose leader ended is dropped (SIGHUP, SIGCONT to its foreground group).
    assert_eq!(io(&tty, &p, &m, TIOCSCTTY, 0), Ok(0));
    p.procs.lock().retain(|x| x.0 != 100);
    p.be(101);
    assert_eq!(io(&tty, &p, &m, TIOCGPGRP, BASE), Err(ENOTTY));
    assert_eq!(p.sent(), [(100, 1), (100, 18)]);
}

#[test]
fn background_check_for_reads() {
    let p = Fake::new(&[(100, 100, 100), (101, 101, 100)], 100);
    let mut ctl = crate::jobs::Ctl::default();
    let me = p.caller(100);
    ctl.sctty(me.as_ref(), 0, true).unwrap();
    ctl.pgrp = Some(101);
    let mut send = crate::jobs::Send::default();
    assert_eq!(ctl.check_change(me.as_ref(), &p, crate::jobs::SIGTTIN, &mut send), Err(ERESTARTSYS));
    assert_eq!(send.to, [(100, 21)]);
    p.ignored.lock().push(21);
    let mut send = crate::jobs::Send::default();
    assert_eq!(ctl.check_change(me.as_ref(), &p, crate::jobs::SIGTTIN, &mut send), Err(EIO));
    let fg = p.caller(101);
    assert_eq!(ctl.check_change(fg.as_ref(), &p, crate::jobs::SIGTTIN, &mut send), Ok(()), "the foreground reads");
}

// ---- evdev: the client ring

fn ev(kind: u16, code: u16, value: i32, t: u64) -> InputEvent {
    InputEvent { time_ns: t, kind, code, value, device: 7, injected: false }
}

#[test]
fn ring_packets_overflow_and_flush() {
    let mut r = Ring::new(8);
    assert!(!r.pass(ev(EV_KEY, 30, 1, 1)));
    assert!(!r.readable(), "a packet is readable at its SYN_REPORT");
    assert!(r.pass(ev(EV_SYN, SYN_REPORT, 0, 1)));
    assert!(!r.pass(ev(EV_SYN, SYN_REPORT, 0, 2)), "an empty SYN_REPORT is dropped");
    assert_eq!(r.fetch().map(|e| e.code), Some(30));
    assert_eq!(r.fetch().map(|e| (e.kind, e.code)), Some((EV_SYN, SYN_REPORT)));
    assert!(r.fetch().is_none());
    // Overflow: everything unread goes, SYN_DROPPED and the newest event stay (the 8th event filled the ring of 8
    // with 2 slots used before: ev 7 is the newest then; ev 8 and the report follow).
    for i in 0..9 {
        r.pass(ev(EV_REL, 0, i, 10 + i as u64));
    }
    r.pass(ev(EV_SYN, SYN_REPORT, 0, 30));
    let got: Vec<(u16, u16, i32)> = core::iter::from_fn(|| r.fetch()).map(|e| (e.kind, e.code, e.value)).collect();
    assert_eq!(got, [(EV_SYN, evdevfs::SYN_DROPPED, 0), (EV_REL, 0, 7), (EV_REL, 0, 8), (EV_SYN, SYN_REPORT, 0)]);
    // EVIOCGKEY's flush: the queued EV_KEY events go (a leading report stays, as `__evdev_flush_queue`).
    r.pass(ev(EV_KEY, 30, 1, 40));
    r.pass(ev(EV_SYN, SYN_REPORT, 0, 40));
    r.pass(ev(EV_REL, 1, 5, 41));
    r.pass(ev(EV_SYN, SYN_REPORT, 0, 41));
    r.flush_type(EV_KEY);
    let got: Vec<(u16, u16)> = core::iter::from_fn(|| r.fetch()).map(|e| (e.kind, e.code)).collect();
    assert_eq!(got, [(EV_SYN, SYN_REPORT), (EV_REL, 1), (EV_SYN, SYN_REPORT)]);
    // A clock change with events queued: they go, a SYN_DROPPED takes their place; like Linux it becomes
    // readable with the next packet (`evdev_set_clk_type` leaves `packet_head` before it).
    r.pass(ev(EV_KEY, 2, 1, 50));
    r.pass(ev(EV_SYN, SYN_REPORT, 0, 50));
    r.set_clock(Clock::Mono, 60);
    assert!(r.fetch().is_none());
    r.pass(ev(EV_KEY, 2, 0, 70));
    r.pass(ev(EV_SYN, SYN_REPORT, 0, 70));
    let got: Vec<(u16, u16)> = core::iter::from_fn(|| r.fetch()).map(|e| (e.kind, e.code)).collect();
    assert_eq!(got, [(EV_SYN, evdevfs::SYN_DROPPED), (EV_KEY, 2), (EV_SYN, SYN_REPORT)]);
}

#[test]
fn event_layout_and_bits() {
    let b = evdevfs::event_bytes(&ev(EV_KEY, 30, 1, 5_123_456_789), Clock::Mono, 0);
    assert_eq!(&b[0..8], &5i64.to_le_bytes());
    assert_eq!(&b[8..16], &123_456i64.to_le_bytes());
    assert_eq!(&b[16..24], &[1, 0, 30, 0, 1, 0, 0, 0]);
    assert_eq!(evdevfs::bits_len(0x2ff), 96, "KEY_MAX: 12 longs");
    assert_eq!(evdevfs::bits_len(0x1f), 8);
    assert_eq!(evdevfs::bits_len(0x7f), 16, "FF_MAX");
    assert_eq!(evdevfs::bits_len(0x10), 8, "SW_MAX");
    let bm = evdevfs::bitmap([0, 1, 0x2ff], 0x2ff, 96);
    assert_eq!((bm[0], bm[95]), (3, 0x80));
    assert_eq!(evdevfs::bitmap([3], 0x2ff, 4).len(), 4, "cut to the caller's size");
    let mut kb = Caps::default();
    kb.keys.extend([1u16, 30]);
    assert_eq!(evdevfs::buffer_size(&kb), 64);
    let mut mouse = Caps::default();
    mouse.rel.extend([0u16, 1, 6, 8]);
    assert_eq!(evdevfs::buffer_size(&mouse), 128);
    assert_eq!(ioc::ior(ioc::NAME, 256), 0x8100_4506, "EVIOCGNAME(256)");
    assert_eq!(ioc::EVIOCGRAB, 0x4004_4590);
    assert_eq!(ioc::EVIOCSCLOCKID, 0x4004_45a0);
}

// ---- evdev: open clients on a test sink

pub(crate) fn keyboard_on(sink: &'static EvdevSink, name: &str) -> Arc<evdev::InputDevice> {
    let mut caps = Caps::default();
    caps.keys.extend([1u16, 30, 31, 58]);
    caps.leds.extend([0u16, 1, 2]);
    caps.msc.insert(4);
    caps.repeat = true;
    evdev::register(sink, DeviceInfo { name: name.to_string(), phys: "usb-1/input0".to_string(), bus: 3, vendor: 0x046d, product: 0xc52b, version: 0x111, caps, uniq: Some(String::new()) })
}

pub(crate) fn read_all(h: &Handle) -> Vec<(u16, u16, i32)> {
    let mut b = [0u8; EVENT_SIZE * 16];
    let n = h.read(&mut b, true, &|| false).unwrap_or(0);
    b[..n].chunks(EVENT_SIZE).map(|e| (u16::from_le_bytes([e[16], e[17]]), u16::from_le_bytes([e[18], e[19]]), i32::from_le_bytes([e[20], e[21], e[22], e[23]]))).collect()
}

#[test]
fn evdev_read_and_ioctls() {
    static S: EvdevSink = EvdevSink::new();
    let k = keyboard_on(&S, "test kbd");
    let h = Handle::open(&k).unwrap();
    let m = Mem::new();
    let mut small = [0u8; 23];
    assert_eq!(h.read(&mut small, true, &|| false), Err(EINVAL), "less than one event");
    assert_eq!(h.read(&mut [], true, &|| false), Err(EAGAIN), "count 0 with nothing queued and O_NONBLOCK");
    k.emit(5, &[(EV_KEY, 30, 1), (EV_SYN, SYN_REPORT, 0)]);
    assert!(h.events().contains(vfs::Events::IN));
    assert_eq!(read_all(&h), [(EV_KEY, 30, 1), (EV_SYN, SYN_REPORT, 0)]);
    // Fixed-size queries.
    assert_eq!(h.ioctl(ioc::EVIOCGVERSION, BASE, &m), Ok(0));
    assert_eq!(m.i32_at(0), 0x010001);
    assert_eq!(h.ioctl(ioc::EVIOCGID, BASE, &m), Ok(0));
    assert_eq!(m.at(0, 8), [3, 0, 0x6d, 0x04, 0x2b, 0xc5, 0x11, 0x01]);
    assert_eq!(h.ioctl(ioc::EVIOCGREP, BASE, &m), Ok(0));
    assert_eq!((m.i32_at(0), m.i32_at(4)), (500, 33));
    assert_eq!(h.ioctl(ioc::EVIOCGEFFECTS, BASE, &m), Ok(0));
    assert_eq!(m.i32_at(0), 0);
    // Variable-length queries answer the bytes copied.
    assert_eq!(h.ioctl(ioc::ior(ioc::NAME, 256), BASE, &m), Ok(9));
    assert_eq!(m.at(0, 9), b"test kbd\0");
    assert_eq!(h.ioctl(ioc::ior(ioc::NAME, 4), BASE, &m), Ok(4), "cut to the size, without NUL");
    assert_eq!(h.ioctl(ioc::ior(ioc::PHYS, 64), BASE, &m), Ok(13));
    assert_eq!(h.ioctl(ioc::ior(ioc::UNIQ, 64), BASE, &m), Ok(1), "an empty uniq is one NUL");
    assert_eq!(h.ioctl(ioc::ior(ioc::BIT, 64), BASE, &m), Ok(8));
    assert_eq!(m.at(0, 8), [0x13, 0, 0x12, 0, 0, 0, 0, 0], "SYN KEY MSC | LED REP");
    assert_eq!(h.ioctl(ioc::ior(ioc::BIT + 1, 256), BASE, &m), Ok(96));
    assert_eq!(m.at(0, 8), [0x02, 0, 0, 0xC0, 0, 0, 0, 0x04], "KEY_ESC, KEY_A, KEY_S, KEY_CAPSLOCK");
    assert_eq!(h.ioctl(ioc::ior(ioc::BIT + 0x15, 256), BASE, &m), Ok(16), "EV_FF bits: FF_MAX");
    assert_eq!(h.ioctl(ioc::ior(ioc::BIT + 0x14, 8), BASE, &m), Err(EINVAL), "EV_REP has no bit query");
    assert_eq!(h.ioctl(ioc::ior(ioc::PROP, 8), BASE, &m), Ok(8));
    assert_eq!(h.ioctl(ioc::ior(ioc::ABS, 24), BASE, &m), Err(EINVAL), "a keyboard has no absinfo");
    // EVIOCGKEY: the held keys, and the queued key events are flushed.
    k.emit(6, &[(EV_KEY, 31, 1), (EV_SYN, SYN_REPORT, 0)]);
    assert_eq!(h.ioctl(ioc::ior(ioc::KEY, 96), BASE, &m), Ok(96));
    assert_eq!(m.at(0, 4), [0, 0, 0, 0xC0], "KEY_A and KEY_S held");
    assert_eq!(read_all(&h), [(EV_SYN, SYN_REPORT, 0)], "EVIOCGKEY flushed the key event");
    k.emit(7, &[(EV_KEY, 30, 0), (EV_KEY, 31, 0), (EV_SYN, SYN_REPORT, 0)]);
    read_all(&h);
    // LEDs follow the sink's lock state.
    S.set_locks(1 << 1);
    assert_eq!(h.ioctl(ioc::ior(ioc::LED, 8), BASE, &m), Ok(8));
    assert_eq!(m.at(0, 1), [0x02]);
    // Clock ids, keycodes, force feedback, unknown commands.
    m.put(0, &1i32.to_le_bytes());
    assert_eq!(h.ioctl(ioc::EVIOCSCLOCKID, BASE, &m), Ok(0));
    m.put(0, &5i32.to_le_bytes());
    assert_eq!(h.ioctl(ioc::EVIOCSCLOCKID, BASE, &m), Err(EINVAL));
    assert_eq!(h.ioctl(ioc::EVIOCSCLOCKID, 0x10, &m), Err(EFAULT));
    assert_eq!(h.ioctl(ioc::EVIOCGKEYCODE, BASE, &m), Err(EINVAL));
    assert_eq!(h.ioctl(ioc::EVIOCRMFF, 1, &m), Err(ENOSYS));
    assert_eq!(h.ioctl(ioc::iow(ioc::EVIOCSFF_NR, 48), BASE, &m), Err(ENOSYS));
    assert_eq!(h.ioctl(ioc::iow(ioc::EVIOCSFF_NR, 40), BASE, &m), Err(EFAULT), "a wrong ff_effect size");
    assert_eq!(h.ioctl(0x5401, BASE, &m), Err(EINVAL), "not an 'E' command");
    // Writes: whole events; LED events set the lock LEDs.
    let mut w = [0u8; EVENT_SIZE];
    w[16..18].copy_from_slice(&EV_LED.to_le_bytes());
    w[18..20].copy_from_slice(&0u16.to_le_bytes());
    w[20..24].copy_from_slice(&1i32.to_le_bytes());
    assert_eq!(h.write(&w[..10]), Err(EINVAL));
    assert_eq!(h.write(&w), Ok(EVENT_SIZE));
    assert_eq!(S.locks(), 0b11);
    // Revoke: ENODEV from now on, HUP on poll.
    assert_eq!(h.ioctl(ioc::EVIOCREVOKE, 1, &m), Err(EINVAL));
    assert_eq!(h.ioctl(ioc::EVIOCREVOKE, 0, &m), Ok(0));
    assert_eq!(h.read(&mut [0u8; 24], true, &|| false), Err(ENODEV));
    assert_eq!(h.ioctl(ioc::EVIOCGVERSION, BASE, &m), Err(ENODEV));
    assert!(h.events().contains(vfs::Events::HUP));
    drop(h);
    k.unregister(8);
}

#[test]
fn evdev_grab_and_unplug() {
    static S: EvdevSink = EvdevSink::new();
    let k = keyboard_on(&S, "grab kbd");
    let vt = S.subscribe("test vt");
    let a = Handle::open(&k).unwrap();
    let b = Handle::open(&k).unwrap();
    let m = Mem::new();
    // A key held when the grab starts is released for the VT, so it never stays held there.
    k.emit(1, &[(EV_KEY, 30, 1), (EV_SYN, SYN_REPORT, 0)]);
    while vt.pop().is_some() {}
    read_all(&a);
    read_all(&b);
    assert_eq!(a.ioctl(ioc::EVIOCGRAB, 1, &m), Ok(0));
    assert_eq!(vt.pop().map(|e| (e.code, e.value)), Some((30, 0)), "synthetic release for the VT");
    while vt.pop().is_some() {}
    assert_eq!(b.ioctl(ioc::EVIOCGRAB, 1, &m), Err(EBUSY));
    assert_eq!(b.ioctl(ioc::EVIOCGRAB, 0, &m), Err(EINVAL), "only the grabber ungrabs");
    k.emit(2, &[(EV_KEY, 30, 0), (EV_SYN, SYN_REPORT, 0)]);
    assert_eq!(read_all(&a), [(EV_KEY, 30, 0), (EV_SYN, SYN_REPORT, 0)]);
    assert_eq!(read_all(&b), [], "the other client gets nothing");
    assert!(vt.pop().is_none(), "the VT gets nothing");
    // Closing the grabbing client ends the grab.
    drop(a);
    k.emit(3, &[(EV_KEY, 31, 1), (EV_SYN, SYN_REPORT, 0)]);
    assert_eq!(read_all(&b).len(), 2);
    assert!(vt.pop().is_some());
    // Unplug: ENODEV, HUP|ERR.
    k.unregister(4);
    assert_eq!(b.read(&mut [0u8; 24], true, &|| false), Err(ENODEV));
    assert!(b.events().contains(vfs::Events::HUP) && b.events().contains(vfs::Events::ERR));
    S.unsubscribe(&vt);
}

#[test]
fn evdev_masks_and_abs() {
    static S: EvdevSink = EvdevSink::new();
    let mut caps = Caps::default();
    caps.keys.insert(0x110);
    caps.rel.extend([0u16, 1]);
    caps.abs.insert(0, AbsInfo { min: 0, max: 2559, fuzz: 0, flat: 0, resolution: 10 });
    let d = evdev::register(&S, DeviceInfo { name: "tablet".to_string(), caps, ..DeviceInfo::default() });
    let h = Handle::open(&d).unwrap();
    let m = Mem::new();
    assert_eq!(h.ioctl(ioc::ior(ioc::PHYS, 8), BASE, &m), Err(ENOENT), "no phys");
    assert_eq!(h.ioctl(ioc::ior(ioc::UNIQ, 8), BASE, &m), Err(ENOENT), "no uniq");
    d.emit(1, &[(EV_ABS, 0, 1200), (EV_SYN, SYN_REPORT, 0)]);
    assert_eq!(h.ioctl(ioc::ior(ioc::ABS, 24), BASE, &m), Ok(0));
    assert_eq!((m.i32_at(0), m.i32_at(4), m.i32_at(8), m.i32_at(20)), (1200, 0, 2559, 10));
    assert_eq!(h.ioctl(ioc::ior(ioc::ABS + 1, 24), BASE, &m), Ok(0), "an axis it does not have: zeros");
    assert_eq!(m.i32_at(8), 0);
    // EVIOCSABS with a short struct: resolution 0, the value is set too.
    m.put(100, &[7, 0, 0, 0, 0, 0, 0, 0, 0xFF, 0x03, 0, 0]);
    assert_eq!(h.ioctl(ioc::iow(ioc::SABS, 12), BASE + 100, &m), Ok(0));
    h.ioctl(ioc::ior(ioc::ABS, 24), BASE, &m).unwrap();
    assert_eq!((m.i32_at(0), m.i32_at(8), m.i32_at(20)), (7, 1023, 0));
    assert_eq!(h.ioctl(ioc::iow(ioc::SABS + 0x2f, 24), BASE + 100, &m), Err(EINVAL), "ABS_MT_SLOT");
    // EVIOCSMASK: only EV_REL passes; EVIOCGMASK reads a type without a mask as all bits.
    m.put(200, &(1u64 << EV_REL).to_le_bytes());
    let mut mask = [0u8; 16];
    mask[4..8].copy_from_slice(&8u32.to_le_bytes());
    mask[8..16].copy_from_slice(&(BASE + 200).to_le_bytes());
    m.put(300, &mask);
    assert_eq!(h.ioctl(ioc::EVIOCSMASK, BASE + 300, &m), Ok(0));
    read_all(&h);
    d.emit(2, &[(EV_ABS, 0, 5), (EV_REL, 0, 3), (EV_SYN, SYN_REPORT, 0)]);
    assert_eq!(read_all(&h), [(EV_REL, 0, 3), (EV_SYN, SYN_REPORT, 0)]);
    mask[0..4].copy_from_slice(&1u32.to_le_bytes());
    mask[8..16].copy_from_slice(&(BASE + 400).to_le_bytes());
    m.put(300, &mask);
    assert_eq!(h.ioctl(ioc::EVIOCGMASK, BASE + 300, &m), Ok(0));
    assert_eq!(m.at(400, 8), [0xFF; 8], "no key mask set: all bits");
    drop(h);
    d.unregister(3);
}
