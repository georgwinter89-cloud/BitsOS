//! The stand-in's only unsafe code: process and signal calls through libc, each block naming what it touches. The process
//! has one thread throughout, so the child may do anything between fork and exec.

use std::ffi::{CString, OsString};
use std::io::Error;
use std::os::unix::ffi::OsStrExt;
use std::sync::atomic::{AtomicI32, Ordering};

const FORWARDED: [libc::c_int; 2] = [libc::SIGTERM, libc::SIGINT];

/// The command's process group, for the handler; 0 until it exists (and always in the child).
static GROUP: AtomicI32 = AtomicI32::new(0);

extern "C" fn forward(sig: libc::c_int) {
    let group = GROUP.load(Ordering::Relaxed);
    if group > 0 {
        // SAFETY: kill is async-signal-safe and takes integers. errno is this thread's and goes back as it was, because
        // the handler may run between wait_any's call and its reading of errno.
        unsafe {
            let errno = libc::__errno_location();
            let was = *errno;
            libc::kill(-group, sig);
            *errno = was;
        }
    }
}

/// The command's argv for exec: built before the fork.
pub struct Argv {
    _words: Vec<CString>,
    pointers: Vec<*const libc::c_char>,
}

impl Argv {
    pub fn new(command: &[OsString]) -> Argv {
        // An argument from the kernel cannot hold a NUL, so nothing is dropped here; no input can panic either.
        let words: Vec<CString> = command.iter().flat_map(|w| CString::new(w.as_bytes())).collect();
        let mut pointers: Vec<*const libc::c_char> = words.iter().map(|w| w.as_ptr()).collect();
        pointers.push(std::ptr::null());
        Argv { _words: words, pointers }
    }
}

pub fn become_subreaper() -> bool {
    // SAFETY: prctl with integer arguments only.
    unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1 as libc::c_ulong) == 0 }
}

fn set_action(sig: libc::c_int, new: &libc::sigaction) -> libc::sigaction {
    // SAFETY: an all-zero sigaction is a valid value of this plain C struct (SIG_DFL, no flags).
    let mut old: libc::sigaction = unsafe { std::mem::zeroed() };
    // SAFETY: both pointers name live sigaction values of the right type for the call's length; the signals are ones
    // that may be caught, so the call only fails on them with invalid pointers.
    unsafe { libc::sigaction(sig, new, &mut old) };
    old
}

fn empty_set() -> libc::sigset_t {
    // SAFETY: sigemptyset initialises the set it is given; zeroed memory is a valid sigset_t to begin with.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        set
    }
}

fn set_mask(how: libc::c_int, set: &libc::sigset_t) -> libc::sigset_t {
    let mut old = empty_set();
    // SAFETY: both pointers name live sigset_t values.
    unsafe { libc::sigprocmask(how, set, &mut old) };
    old
}

/// Forks and starts the command in a process group of its own; the parent then forwards TERM and INT to that group. None when
/// the fork fails. Its own group: Steam's own group holds Steam, which the forwarding must not reach.
pub fn start(argv: &Argv) -> Option<i32> {
    // Blocked until the group is known: a signal before that waits and is then forwarded, none goes astray.
    let mut forwarded = empty_set();
    for sig in FORWARDED {
        // SAFETY: the set is initialised; a valid signal number.
        unsafe { libc::sigaddset(&mut forwarded, sig) };
    }
    let mask_before = set_mask(libc::SIG_BLOCK, &forwarded);

    // SAFETY: an all-zero sigaction is valid (SIG_DFL, no flags, empty mask).
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction = forward as extern "C" fn(libc::c_int) as usize;
    action.sa_flags = libc::SA_RESTART;
    action.sa_mask = forwarded;
    let saved: Vec<(libc::c_int, libc::sigaction)> = FORWARDED.iter().map(|&s| (s, set_action(s, &action))).collect();
    // SAFETY: as above; an all-zero sigaction is SIG_DFL.
    let default: libc::sigaction = unsafe { std::mem::zeroed() };
    // With SIGCHLD ignored the kernel would reap the children itself and the command's status would be lost.
    let chld = set_action(libc::SIGCHLD, &default);

    // SAFETY: one thread, so the child has a consistent copy of everything; it only makes calls below and then execs or exits.
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        run_command(argv, &saved, &chld, &default, &mask_before);
    }
    if pid < 0 {
        return None;
    }
    GROUP.store(pid, Ordering::Relaxed);
    // SAFETY: integer arguments. The child does the same itself; whichever comes first wins and the other's failure means nothing.
    unsafe { libc::setpgid(pid, pid) };
    // As the caller had it: what the caller blocked stays blocked here too, as under Valve's, which never touches the mask.
    set_mask(libc::SIG_SETMASK, &mask_before);
    Some(pid)
}

/// Once the command is gone: when its group has no member left, stops forwarding, because the number may now go to another
/// process and `kill(-group)` would reach strangers. Call after every reaped child, as a member may outlive the command.
pub fn forget_group_when_empty() {
    let group = GROUP.load(Ordering::Relaxed);
    // SAFETY: integer arguments; signal 0 only asks whether the group exists. EPERM means it does, with members out of reach.
    if group > 0 && unsafe { libc::kill(-group, 0) } != 0 && Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
        GROUP.store(0, Ordering::Relaxed);
    }
}

/// The child: as the reaper found its signals, except SIGPIPE, which Rust's start-up set to ignored and which a program
/// expects at its default. Never returns.
fn run_command(argv: &Argv, saved: &[(libc::c_int, libc::sigaction)], chld: &libc::sigaction, default: &libc::sigaction, mask: &libc::sigset_t) -> ! {
    // SAFETY: integer arguments.
    unsafe { libc::setpgid(0, 0) };
    for (sig, old) in saved {
        set_action(*sig, old);
    }
    set_action(libc::SIGCHLD, chld);
    set_action(libc::SIGPIPE, default);
    set_mask(libc::SIG_SETMASK, mask);
    let name = argv.pointers[0];
    if name.is_null() {
        eprintln!("reaper: no sub-command!");
        // SAFETY: ends this process only.
        unsafe { libc::_exit(1) };
    }
    // SAFETY: `pointers` is a NULL-terminated list of NUL-terminated strings held alive by `argv`.
    unsafe { libc::execvp(name, argv.pointers.as_ptr()) };
    let e = Error::last_os_error();
    // SAFETY: `name` is one of those strings. Only the program's name goes to the log, not its arguments.
    let shown = unsafe { std::ffi::CStr::from_ptr(name) }.to_string_lossy();
    eprintln!("reaper: cannot run {shown}: {e}");
    let code = if e.raw_os_error() == Some(libc::ENOENT) { 127 } else { 126 };
    // SAFETY: ends this process only.
    unsafe { libc::_exit(code) }
}

pub enum Waited {
    Ended { pid: i32, status: i32 },
    NoneLeft,
    Failed(Error),
}

/// Blocks until any child ends. A signal's handler runs meanwhile and the wait goes on.
pub fn wait_any() -> Waited {
    loop {
        let mut status = 0;
        // SAFETY: status is a live int; no rusage is asked for. __WALL: a descendant started without SIGCHLD counts too.
        let pid = unsafe { libc::waitpid(-1, &mut status, libc::__WALL) };
        if pid > 0 {
            return Waited::Ended { pid, status };
        }
        let e = Error::last_os_error();
        match e.raw_os_error() {
            Some(libc::EINTR) => continue,
            Some(libc::ECHILD) => return Waited::NoneLeft,
            _ => return Waited::Failed(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    /// The only test in this binary, so nothing else uses GROUP. A group's number cannot be made to come back on demand, so this
    /// shows the step that prevents it: the number stays while a member lives and goes with the last one.
    #[test]
    fn the_group_is_forgotten_only_when_its_last_member_is_gone() {
        let mut member = Command::new("sleep").arg("20").process_group(0).spawn().unwrap();
        let group = member.id() as i32;
        GROUP.store(group, Ordering::Relaxed);
        forget_group_when_empty();
        assert_eq!(GROUP.load(Ordering::Relaxed), group, "a member still lives");
        member.kill().unwrap();
        // A zombie still counts as a member: it goes with the wait, as in main's loop.
        member.wait().unwrap();
        forget_group_when_empty();
        assert_eq!(GROUP.load(Ordering::Relaxed), 0, "no member left");
    }
}
