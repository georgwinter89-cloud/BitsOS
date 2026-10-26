//! The console terminal's ioctls (IN-4), in the order Linux 6.6 looks at them: `tty_ioctl`
//! (drivers/tty/tty_io.c:2652: break, TIOCSTI, window size, exclusive mode, line discipline, TIOCGDEV, job
//! control of tty_jobctrl.c), the VT's own (`vt_ioctl`, drivers/tty/vt/vt_ioctl.c: KDGETMODE/KDSETMODE,
//! KDGKBMODE/KDSKBMODE, KDGKBTYPE; the other VT calls in `vtioctl`), then N_TTY's (`n_tty_ioctl`: TIOCINQ/TIOCOUTQ, TCXONC, TCFLSH and the termios
//! calls of tty_ioctl.c `tty_mode_ioctl`). Anything else: ENOTTY.
//!
//! The argument is read and the answer written through `UserMem` (the caller's memory), so every errno is the
//! one Linux gives, EFAULT included; TCGETS writes exactly the kernel's 36-byte `struct termios`.

use crate::jobs::{self, Procs, SIGWINCH, SIGTTOU, checks_change};
use crate::termios::{self, Termios, Winsize, flag, ioc::*};
use crate::tty::Tty;
use uapi::errno::*;
use vfs::Errno;
use vfs::sys::UserMem;

/// `new_encode_dev(4, 1)`: the console's tty is tty1 (TIOCGDEV on /dev/console and /dev/tty0).
pub const TTY1_DEV: u32 = (4 << 8) | 1;

fn put(mem: &dyn UserMem, arg: u64, data: &[u8]) -> Result<i64, Errno> {
    if mem.write(arg, data) { Ok(0) } else { Err(EFAULT) }
}
fn get<const N: usize>(mem: &dyn UserMem, arg: u64) -> Result<[u8; N], Errno> {
    let mut b = [0u8; N];
    if mem.read(arg, &mut b) { Ok(b) } else { Err(EFAULT) }
}
fn get_i32(mem: &dyn UserMem, arg: u64) -> Result<i32, Errno> {
    Ok(i32::from_le_bytes(get::<4>(mem, arg)?))
}

/// The window size the terminal reports: TIOCSWINSZ's, else the VT's grid (`vc_do_resize`: rows, columns,
/// ypixel = the scan lines).
pub fn winsize(tty: &Tty) -> Winsize {
    tty.winsize().unwrap_or_else(|| default_winsize(tty))
}

/// Without TIOCSWINSZ: the VT's grid, or 0x0 on a pty (a fresh `tty_struct`'s zeroed winsize, IN-7).
fn default_winsize(tty: &Tty) -> Winsize {
    if tty.is_pty() { Winsize::default() } else { vt_winsize() }
}

pub fn vt_winsize() -> Winsize {
    let (cols, rows) = crate::vt::size();
    let cell_h = crate::vt::font().map(|f| f.h).unwrap_or(16);
    Winsize { rows: rows as u16, cols: cols as u16, xpixel: 0, ypixel: (rows * cell_h).min(u16::MAX as u32) as u16 }
}

/// One terminal ioctl for the calling process (`p.current()`).
pub fn ioctl(tty: &Tty, cmd: u64, arg: u64, mem: &dyn UserMem, p: &dyn Procs) -> Result<i64, Errno> {
    let me = p.current();
    let me = me.as_ref();
    if checks_change(cmd) {
        tty.with_ctl(p, |ctl, send| ctl.check_change(me, p, SIGTTOU, send))?;
    }
    match cmd {
        // ---- tty_ioctl
        TIOCSTI => {
            // `dev.tty.legacy_tiocsti = 0` as on CachyOS (the oracle): only CAP_SYS_ADMIN pushes input (EIO for
            // everyone else), and then only into its own terminal unless privileged (always, here).
            if !me.is_some_and(|m| m.privileged()) {
                return Err(EIO);
            }
            let c = get::<1>(mem, arg)?;
            // A program pushing input is injection (R8c): the monitor's physical-only commands refuse it.
            tty.input(&c, true, &mut |e| tty.echo_out(e));
            Ok(0)
        }
        TIOCGWINSZ => put(mem, arg, &winsize(tty).to_bytes()),
        TIOCSWINSZ => {
            let w = Winsize::from_bytes(&get::<8>(mem, arg)?);
            if tty.set_winsize(Some(w), default_winsize(tty)) {
                signal_fg(tty, p, SIGWINCH);
            }
            Ok(0)
        }
        TIOCEXCL | TIOCNXCL => {
            tty.set_exclusive(cmd == TIOCEXCL);
            Ok(0)
        }
        TIOCGEXCL => put(mem, arg, &(tty.exclusive() as i32).to_le_bytes()),
        TIOCGETD => put(mem, arg, &0i32.to_le_bytes()),
        TIOCSETD => {
            // N_TTY is the only line discipline; another one is not built (`tty_ldisc_get` -> EINVAL).
            if get_i32(mem, arg)? == 0 { Ok(0) } else { Err(EINVAL) }
        }
        TIOCVHANGUP => {
            if !me.is_some_and(|m| m.privileged()) {
                return Err(EPERM);
            }
            tty.with_ctl(p, |ctl, send| {
                if let Some(pg) = ctl.pgrp {
                    send.push(pg, jobs::SIGHUP);
                    send.push(pg, jobs::SIGCONT);
                }
                *ctl = jobs::Ctl::default();
            });
            Ok(0)
        }
        TIOCGDEV => put(mem, arg, &tty.dev().to_le_bytes()),
        // The VT has no break: TCSBRK/TCSBRKP/TIOCSBRK/TIOCCBRK answer 0 after the background check.
        TIOCSBRK | TIOCCBRK | TCSBRK | TCSBRKP => Ok(0),
        TIOCNOTTY => tty.with_ctl(p, |ctl, send| ctl.notty(me, send)).map(|_| 0),
        TIOCSCTTY => {
            // `tiocsctty`: a session leader that has another controlling terminal gets EPERM (IN-7: with ptys
            // BitsOS has more than one terminal; a session holds at most one of them).
            let other = me.is_some_and(|m| m.leader() && tty.with_ctl(p, |c, _| c.session) != Some(m.sid) && crate::pty::session_has_tty(m.sid, tty, p));
            if other {
                return Err(EPERM);
            }
            tty.with_ctl(p, |ctl, _| ctl.sctty(me, arg, true)).map(|_| 0)
        }
        // A pty slave is no VT: the KD* and VT_* calls are not its own (`n_tty_ioctl` answers ENOTTY).
        KDGETMODE | KDSETMODE | KDGKBMODE | KDSKBMODE | KDGKBTYPE if tty.is_pty() => Err(ENOTTY),
        TIOCGPGRP => {
            let pg = tty.with_ctl(p, |ctl, _| ctl.gpgrp(me))?;
            put(mem, arg, &(pg as i32).to_le_bytes())
        }
        TIOCSPGRP => {
            match tty.with_ctl(p, |ctl, send| ctl.check_change(me, p, SIGTTOU, send)) {
                Err(EIO) => return Err(ENOTTY),
                Err(e) => return Err(e),
                Ok(()) => {}
            }
            let pg = get_i32(mem, arg)?;
            tty.with_ctl(p, |ctl, _| ctl.spgrp(me, p, pg)).map(|_| 0)
        }
        TIOCGSID => {
            let sid = tty.with_ctl(p, |ctl, _| ctl.gsid(me))?;
            put(mem, arg, &(sid as i32).to_le_bytes())
        }
        // ---- vt_ioctl
        KDGETMODE => put(mem, arg, &(crate::vt::kd_mode() as i32).to_le_bytes()),
        KDSETMODE => {
            if !tty.with_ctl(p, |ctl, _| ctl.vt_perm(me)) {
                return Err(EPERM);
            }
            let graphics = match arg {
                KD_GRAPHICS => true,
                KD_TEXT | KD_TEXT0 | KD_TEXT1 => false,
                _ => return Err(EINVAL),
            };
            crate::vt::set_kd_mode(graphics, me.map(|m| m.pid));
            // DP-2 CR 2b/2c (INT-A2a): the scanout follows the program's mode, asked with none of IN's locks held.
            // KD_GRAPHICS: fbdev shows the program's fb0 at once (false = another owner holds the screen, nothing
            // to undo); KD_TEXT from a program: fbdev gives the screen back within about 100 ms.
            match (graphics, me) {
                (true, Some(m)) => {
                    let _ = display::request_scanout_for(m.pid);
                }
                (false, Some(_)) => display::request_scanout_back(),
                _ => {}
            }
            Ok(0)
        }
        KDGKBMODE => put(mem, arg, &(crate::vt::kb_mode() as i32).to_le_bytes()),
        KDSKBMODE => {
            if !tty.with_ctl(p, |ctl, _| ctl.vt_perm(me)) {
                return Err(EPERM);
            }
            if arg > K_OFF {
                return Err(EINVAL);
            }
            crate::vt::set_kb_mode(arg as u8, me.map(|m| m.pid));
            Ok(0)
        }
        KDGKBTYPE => put(mem, arg, &[KB_101]),
        // ---- n_tty_ioctl
        TIOCINQ => put(mem, arg, &(tty.inq() as u32).to_le_bytes()),
        TIOCOUTQ => put(mem, arg, &0i32.to_le_bytes()),
        TCXONC => match arg {
            TCOOFF | TCOON => {
                tty.tco(arg == TCOOFF);
                Ok(0)
            }
            // `tty_send_xchar`: the STOP/START character goes to the output (the VT shows nothing for it).
            TCIOFF | TCION => Ok(0),
            _ => Err(EINVAL),
        },
        TCFLSH => match arg {
            TCIFLUSH | TCIOFLUSH => {
                tty.flush();
                Ok(0)
            }
            TCOFLUSH => Ok(0),
            _ => Err(EINVAL),
        },
        // ---- tty_mode_ioctl
        TCGETS => put(mem, arg, &tty.termios().to_termios()),
        TCGETS2 => put(mem, arg, &tty.termios().to_termios2()),
        TCGETA => put(mem, arg, &tty.termios().to_termio()),
        TCSETS | TCSETSW | TCSETSF => {
            let b = get::<{ termios::TERMIOS_SIZE }>(mem, arg)?;
            tty.note_termios_owner(me.map(|m| m.pid).unwrap_or(0));
            let mut t = tty.termios();
            t.set_from_termios(&b);
            set(tty, t, cmd == TCSETSF);
            Ok(0)
        }
        TCSETS2 | TCSETSW2 | TCSETSF2 => {
            let b = get::<{ termios::TERMIOS2_SIZE }>(mem, arg)?;
            tty.note_termios_owner(me.map(|m| m.pid).unwrap_or(0));
            let mut t = tty.termios();
            t.set_from_termios2(&b);
            set(tty, t, cmd == TCSETSF2);
            Ok(0)
        }
        TCSETA | TCSETAW | TCSETAF => {
            let b = get::<{ termios::TERMIO_SIZE }>(mem, arg)?;
            tty.note_termios_owner(me.map(|m| m.pid).unwrap_or(0));
            let mut t = tty.termios();
            t.set_from_termio(&b);
            set(tty, t, cmd == TCSETAF);
            Ok(0)
        }
        TIOCGSOFTCAR => put(mem, arg, &((tty.termios().cflag & flag::CLOCAL != 0) as i32).to_le_bytes()),
        TIOCSSOFTCAR => {
            let on = get_i32(mem, arg)? != 0;
            let mut t = tty.termios();
            t.cflag = (t.cflag & !flag::CLOCAL) | if on { flag::CLOCAL } else { 0 };
            set(tty, t, false);
            Ok(0)
        }
        _ if tty.is_pty() => Err(ENOTTY),
        _ => {
            // The rest of `vt_ioctl` (VT_*, LEDs, meta mode, KDKBDREP, TIOCLINUX), else ENOTTY.
            let perm = tty.with_ctl(p, |ctl, _| ctl.vt_perm(me));
            crate::vtioctl::ioctl(cmd, arg, mem, me, perm).unwrap_or(Err(ENOTTY))
        }
    }
}

/// `set_termios` after the read: TCSETSW/TCSETSF wait for the output first (the console's output is written
/// synchronously, nothing to wait for), TCSETSF also flushes the input.
fn set(tty: &Tty, t: Termios, flush: bool) {
    let before = tty.termios();
    tty.set_termios(t, flush);
    let after = tty.termios();
    if (before.lflag ^ after.lflag) & (flag::ICANON | flag::ECHO | flag::ISIG) != 0 {
        klog::event!(Debug, "console", "tty.termios", lflag = klog::Hex(after.lflag as u64), iflag = klog::Hex(after.iflag as u64),
            oflag = klog::Hex(after.oflag as u64), vmin = after.cc[termios::cc::VMIN], vtime = after.cc[termios::cc::VTIME]);
    }
}

/// A signal for the terminal's foreground process group.
pub fn signal_fg(tty: &Tty, p: &dyn Procs, sig: u32) {
    tty.with_ctl(p, |ctl, send| {
        if let Some(pg) = ctl.pgrp {
            send.push(pg, sig);
        }
    });
}
