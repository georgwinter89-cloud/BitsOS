//! The terminal itself: termios, the window size, waiting for a key; and a few words to the kernel about
//! this process and the helper program it may start. The crate's only unsafe code (libc calls on file
//! descriptors 0 and 1 with buffers owned here, and calls without any buffer).
#![allow(unsafe_code)]

use super::Term;
use std::io::Write;
use std::mem::MaybeUninit;

pub fn is_tty(fd: i32) -> bool {
    // SAFETY: isatty only inspects the descriptor.
    unsafe { libc::isatty(fd) == 1 }
}

/// Closes this program's memory to the other programs of its user in the same user namespace (not dumpable,
/// as ssh-agent does it): process_vm_readv/writev and /proc/<pid>/mem of such a program are then refused. The
/// memory holds the provider's key, and the kernel's tools socket admits the assistant only with its memory
/// closed. BitsOS's kernel closes /bin/assist itself at execve, before its first instruction (INT-CL3T);
/// this call is for an assistant at another path and costs nothing where it is already so. It does not
/// close the memory to a program that started this one inside a user namespace it made: that one has every
/// capability over it, and the door and settingsd turn such an assistant away. False: the kernel said no.
pub fn close_memory() -> bool {
    // SAFETY: PR_SET_DUMPABLE takes one integer and touches no memory of ours.
    unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0 as libc::c_ulong) == 0 }
}

/// SIGTERM (or, with `kill`, SIGKILL) for every process of the group `group`. Only a group this program made
/// itself (a child started with a group of its own) is ever named here.
pub fn signal_group(group: u32, kill: bool) {
    // 0 and 1 as a group would mean this program's own group and every process there is.
    let Some(group) = i32::try_from(group).ok().filter(|g| *g > 1) else { return };
    // SAFETY: kill takes two integers and touches no memory of ours.
    unsafe { libc::kill(-group, if kill { libc::SIGKILL } else { libc::SIGTERM }) };
}

/// One process, `kill` as [`signal_group`] has it.
pub fn signal(pid: u32, kill: bool) {
    let Some(pid) = i32::try_from(pid).ok().filter(|p| *p > 1) else { return };
    // SAFETY: kill takes two integers and touches no memory of ours.
    unsafe { libc::kill(pid, if kill { libc::SIGKILL } else { libc::SIGTERM }) };
}

/// True while some process of `group` is still there (a zombie counts until it is reaped).
pub fn group_lives(group: u32) -> bool {
    let Some(group) = i32::try_from(group).ok().filter(|g| *g > 1) else { return false };
    // SAFETY: signal 0 only asks; kill takes two integers and touches no memory of ours.
    unsafe { libc::kill(-group, 0) == 0 }
}

/// True when the other end of the connected socket `fd` has gone away entirely. A peer that only closed
/// its writing side (it said all it had to say) is still there.
pub fn hung_up(fd: i32) -> bool {
    let mut fds = libc::pollfd { fd, events: 0, revents: 0 };
    // SAFETY: one pollfd on the stack, a count of one and no wait.
    unsafe { libc::poll(&mut fds, 1, 0) > 0 && (fds.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL)) != 0 }
}

/// The program `cmd` starts is ended by the kernel when this program goes away without ending it itself
/// (killed, a closed terminal). Where the kernel does not know the request, nothing changes: the helper then
/// still notices at its closed pipes.
pub fn end_with_parent(cmd: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: runs in the child between fork and exec; one system call with two integers, no memory, no lock.
    unsafe {
        cmd.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong);
            Ok(())
        })
    };
}

/// Columns and rows of the terminal on stdout; 80x25 (the VGA text size) when it does not say.
pub fn size() -> (usize, usize) {
    let mut ws = MaybeUninit::<libc::winsize>::zeroed();
    // SAFETY: TIOCGWINSZ writes one winsize into the buffer given.
    let ok = unsafe { libc::ioctl(1, libc::TIOCGWINSZ, ws.as_mut_ptr()) } == 0;
    // SAFETY: zero-initialised, and filled by the kernel when ok.
    let ws = unsafe { ws.assume_init() };
    if ok && ws.ws_col > 0 && ws.ws_row > 0 { (ws.ws_col as usize, ws.ws_row as usize) } else { (80, 25) }
}

fn termios() -> Option<libc::termios> {
    let mut t = MaybeUninit::<libc::termios>::zeroed();
    // SAFETY: tcgetattr writes one termios into the buffer given.
    (unsafe { libc::tcgetattr(0, t.as_mut_ptr()) } == 0).then(|| unsafe { t.assume_init() })
}

fn apply(t: &libc::termios) {
    // SAFETY: tcsetattr reads the termios given.
    unsafe { libc::tcsetattr(0, libc::TCSANOW, t) };
}

/// The terminal as it was, put back when this is dropped (also on a panic's unwind).
pub struct Restore(Option<libc::termios>);

impl Drop for Restore {
    fn drop(&mut self) {
        if let Some(t) = &self.0 {
            apply(t);
        }
    }
}

/// Every key at once and unechoed, no signals from Ctrl-C: the chat reads them as keys.
pub fn raw() -> Restore {
    let saved = termios();
    if let Some(mut t) = saved {
        t.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG | libc::IEXTEN);
        t.c_iflag &= !(libc::IXON | libc::ICRNL | libc::INLCR | libc::IGNCR);
        t.c_cc[libc::VMIN] = 1;
        t.c_cc[libc::VTIME] = 0;
        apply(&t);
    }
    Restore(saved)
}

/// Lines as usual, but what is typed stays invisible (a key being entered).
pub fn no_echo() -> Restore {
    let saved = termios();
    if let Some(mut t) = saved {
        t.c_lflag &= !libc::ECHO;
        apply(&t);
    }
    Restore(saved)
}

/// True when stdin has input within `ms` milliseconds (0: just look).
pub fn wait_input(ms: i32) -> bool {
    let mut fds = libc::pollfd { fd: 0, events: libc::POLLIN, revents: 0 };
    // SAFETY: poll reads and writes the one pollfd given.
    unsafe { libc::poll(&mut fds, 1, ms) > 0 && (fds.revents & (libc::POLLIN | libc::POLLHUP)) != 0 }
}

/// Bytes from stdin; 0 at its end.
pub fn read_input(buf: &mut [u8]) -> usize {
    // SAFETY: read writes at most buf.len() bytes into buf.
    let n = unsafe { libc::read(0, buf.as_mut_ptr().cast(), buf.len()) };
    usize::try_from(n).unwrap_or(0)
}

/// Stdin and stdout of this process.
pub struct Console;

impl Term for Console {
    fn size(&self) -> (usize, usize) {
        size()
    }
    fn wait_input(&mut self, ms: i32) -> bool {
        wait_input(ms)
    }
    fn read_input(&mut self, buf: &mut [u8]) -> usize {
        read_input(buf)
    }
    fn drop_input(&mut self) {
        // SAFETY: tcflush only names the descriptor.
        unsafe { libc::tcflush(0, libc::TCIFLUSH) };
        // Whatever the flush left (a terminal without it, a pipe): read it away.
        let mut buf = [0u8; 256];
        while wait_input(0) && read_input(&mut buf) > 0 {}
    }
    fn write(&mut self, bytes: &[u8]) {
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(bytes);
        let _ = out.flush();
    }
}
