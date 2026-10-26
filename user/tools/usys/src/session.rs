//! Starting a program as the leader of a new session that owns the terminal on its fd 0, the way getty or a
//! terminal emulator starts a login shell (setsid, TIOCSCTTY, tcsetpgrp). ash enables job control only when
//! its process group is the terminal's foreground group (linux-6.6/drivers/tty/tty_jobctrl.c:360 tiocsctty,
//! :488 tiocspgrp).

use std::os::unix::process::CommandExt;
use std::process::Command;

const SETSID: usize = 112;
const GETPID: usize = 39;
const TIOCSCTTY: usize = 0x540E;
const TIOCSPGRP: usize = 0x5410;

/// Makes `cmd`'s child a session leader with fd 0 as its controlling terminal and its own group in the
/// foreground. `steal` = 1 takes the terminal from another session (Linux allows it to CAP_SYS_ADMIN, here
/// root). setsid failing fails the spawn; a terminal that refuses (fd 0 is not a terminal, or it belongs to
/// another session) leaves a working program without job control and a line on fd 2.
pub fn console_session(cmd: &mut Command, steal: bool) {
    let arg = steal as usize;
    let hook = move || -> std::io::Result<()> {
        // Only raw syscalls between fork and exec: async-signal-safe, no allocation, no locks.
        // SAFETY: setsid and getpid take no pointers.
        let sid = unsafe { super::raw(SETSID, [0; 6]) };
        if sid < 0 {
            return Err(std::io::Error::from_raw_os_error(-sid as i32));
        }
        // SAFETY: TIOCSCTTY takes an integer argument, not a pointer.
        let r = unsafe { super::raw(super::nr::IOCTL, [0, TIOCSCTTY, arg, 0, 0, 0]) };
        if r < 0 {
            note(b"init: console shell without job control (TIOCSCTTY failed, errno ", -r);
            return Ok(());
        }
        // SAFETY: getpid takes no pointers.
        let pgrp = unsafe { super::raw(GETPID, [0; 6]) } as i32;
        // SAFETY: TIOCSPGRP reads one int (pid_t) from &pgrp, a local that lives across the call.
        let r = unsafe { super::raw(super::nr::IOCTL, [0, TIOCSPGRP, &pgrp as *const i32 as usize, 0, 0, 0]) };
        if r < 0 {
            note(b"init: console shell not in the foreground (TIOCSPGRP failed, errno ", -r);
        }
        Ok(())
    };
    // SAFETY: the hook runs in the forked child before exec and only makes raw system calls on locals (see
    // above); it touches no lock, allocator or state shared with the parent.
    unsafe {
        cmd.pre_exec(hook);
    }
}

/// Writes `msg`, the decimal `errno` and ")\n" to fd 2 from a stack buffer (usable between fork and exec).
fn note(msg: &[u8], errno: isize) {
    let mut buf = [0u8; 128];
    let mut n = 0;
    for &b in msg.iter().take(100) {
        buf[n] = b;
        n += 1;
    }
    let mut digits = [0u8; 20];
    let mut d = 0;
    let mut v = errno.unsigned_abs();
    loop {
        digits[d] = b'0' + (v % 10) as u8;
        d += 1;
        v /= 10;
        if v == 0 || d == digits.len() {
            break;
        }
    }
    while d > 0 && n < buf.len() - 2 {
        d -= 1;
        buf[n] = digits[d];
        n += 1;
    }
    buf[n] = b')';
    buf[n + 1] = b'\n';
    n += 2;
    // SAFETY: write reads n bytes from buf, a local array of at least n bytes.
    unsafe {
        super::raw(super::nr::WRITE, [2, buf.as_ptr() as usize, n, 0, 0, 0]);
    }
}
