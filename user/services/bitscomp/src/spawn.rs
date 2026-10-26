//! WY-7: how bitscomp starts a program without forking itself on the loop thread. A fork copies the page tables
//! of the whole compositor (its frame images and caches: thousands of pages, shared copy-on-write, MM-5b) and
//! every page it then draws into faults back once (QEMU 2560x1440: ~3 ms per start, ten Strg+Alt+Enter read in
//! one turn held a frame 30 ms). A `pre_exec` closure (the parent-death signal) forces std onto that fork path,
//! so bitscomp starts its own small trampoline instead (`bitscomp exec PPID PROGRAM ARG...`) with posix_spawn
//! (glibc: clone with CLONE_VM|CLONE_VFORK, no copy; the loop waits only for the trampoline's exec), and the
//! trampoline sets the parent-death signal and becomes the program (execvp, the signal survives the exec as
//! with the closure before). Process group, environment and the pid bitscomp waits for are the same as before.
//!
//! The signal mask: std hands a child the parent's (both its paths), and bitscomp blocks SIGTERM, SIGINT and
//! SIGHUP for its signalfd (calloop `Signals`), so foot never saw the SIGTERM of bitscomp's end and was killed a
//! second later. The child starts with an empty mask now, as a program expects (`unblock_all`).

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// bitscomp's own file for the trampoline (None: /proc/self/exe unreadable: the fork path is used).
pub fn trampoline() -> Option<&'static Path> {
    static EXE: OnceLock<Option<PathBuf>> = OnceLock::new();
    EXE.get_or_init(|| std::env::current_exe().ok().filter(|p| p.is_absolute() && p.is_file())).as_deref()
}

/// Whether `cmd` can be started: a path to an executable file, or a name found in `path` (execvp's search).
/// Checked before the start, so a missing program is still refused by `spawn` (the trampoline would only
/// report it from the child).
pub fn found(cmd: &str, path: &str, exe: impl Fn(&Path) -> bool) -> bool {
    if cmd.is_empty() {
        return false;
    }
    if cmd.contains('/') {
        return exe(Path::new(cmd));
    }
    path.split(':').map(|d| if d.is_empty() { "." } else { d }).any(|d| exe(&Path::new(d).join(cmd)))
}

/// The PATH a started program sees: the one in `env` (applied to the child), else bitscomp's own.
pub fn child_path(env: &[(String, String)], own: Option<String>) -> String {
    env.iter().rev().find(|(k, _)| k == "PATH").map(|(_, v)| v.clone()).or(own).unwrap_or_default()
}

/// An executable regular file (access X_OK, as execvp asks).
pub fn executable(p: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = std::ffi::CString::new(p.as_os_str().as_bytes()) else { return false };
    // SAFETY: `c` is a valid NUL-terminated string that outlives the call; access only reads it.
    p.is_file() && unsafe { libc::access(c.as_ptr(), libc::X_OK) == 0 }
}

/// The trampoline (`bitscomp exec PPID PROGRAM ARG...`): parent-death signal, then the program. Returns the exit
/// code only when it could not become the program.
pub fn exec_child(args: &[String]) -> i32 {
    use std::os::unix::process::CommandExt;
    let (Some(ppid), Some(cmd)) = (args.first().and_then(|p| p.parse::<libc::pid_t>().ok()), args.get(1)) else {
        eprintln!("bitscomp exec: want PPID PROGRAM [ARG]...");
        return 2;
    };
    unblock_all();
    // A crashed or killed bitscomp takes its started programs with it (SIGTERM), as a clean end does.
    // SAFETY: prctl and getppid touch no memory; EINVAL on a kernel without PR_SET_PDEATHSIG is ignored. A
    // parent already gone before the prctl: the program is not started.
    unsafe {
        libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
        if libc::getppid() != ppid {
            libc::raise(libc::SIGTERM);
            return 143;
        }
    }
    let e = std::process::Command::new(cmd).args(&args[2..]).exec();
    eprintln!("bitscomp: cannot start {cmd}: {e}");
    127
}

/// An empty signal mask for this thread (async-signal-safe: also used between fork and exec).
pub fn unblock_all() {
    // SAFETY: `set` is a local sigset_t that sigemptyset fills before pthread_sigmask reads it; no allocation.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::pthread_sigmask(libc::SIG_SETMASK, &set, std::ptr::null_mut());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn found_follows_execvp() {
        let have = |p: &Path| p == Path::new("/usr/bin/foot") || p == Path::new("./tool") || p == Path::new("/opt/x");
        assert!(found("foot", "/bin:/usr/bin", have));
        assert!(!found("foot", "/bin", have));
        assert!(found("tool", "/bin::/usr/bin", have), "an empty PATH entry is the current directory");
        assert!(found("/opt/x", "", have));
        assert!(!found("/opt/y", "/opt", have));
        assert!(!found("", "/usr/bin", |_| true));
    }

    #[test]
    fn the_childs_path_is_searched() {
        let env = vec![("HOME".to_string(), "/".to_string()), ("PATH".to_string(), "/bin".to_string())];
        assert_eq!(child_path(&env, Some("/usr/bin".into())), "/bin");
        assert_eq!(child_path(&env[..1], Some("/usr/bin".into())), "/usr/bin");
        assert_eq!(child_path(&[], None), "");
    }

    #[test]
    fn trampoline_refuses_bad_arguments() {
        assert_eq!(exec_child(&[]), 2);
        assert_eq!(exec_child(&["x".into(), "foot".into()]), 2);
    }

    #[test]
    fn the_test_binary_is_its_own_trampoline_file() {
        assert!(trampoline().is_some_and(|p| p.is_absolute()));
        assert!(executable(trampoline().unwrap()));
        assert!(!executable(Path::new("/nonexistent/bitscomp")));
    }
}
