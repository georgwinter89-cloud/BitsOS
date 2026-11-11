//! GM-2b: a 64-bit stand-in for Valve's 32-bit `steamrt64/reaper`, which Steam puts before every game
//! (`reaper SteamLaunch AppId=N -- <command>`, steamrt64/steamclient.so). V2 runs no dynamic i386 programs (Georg 27.10.).
//! Same as Valve's: the words before `--` mean nothing, its three error texts, child subreaper, fork and exec, waits until
//! no descendant is left. On purpose not as Valve's: TERM and INT go on to the command's process group, the exit code is the
//! command's (Valve's always 0), and a command that cannot start ends with 127 or 126 (Valve's child falls into the wait loop).
//! Word list and Valve's `main`: plan/notes/sx-GM-2b.md. Linux: kernel/sys.c PR_SET_CHILD_SUBREAPER, kernel/exit.c:583.

mod sys;

use std::process::exit;

fn main() {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let Some((ignored, command)) = reaper::split(&args) else {
        eprintln!("reaper: no sub-command!");
        exit(1);
    };
    for word in ignored {
        eprintln!("reaper: ignoring {}", reaper::label(word));
    }
    let argv = sys::Argv::new(command);
    if !sys::become_subreaper() {
        eprintln!("reaper: prctl() failed!");
        exit(1);
    }
    let Some(command_pid) = sys::start(&argv) else {
        eprintln!("reaper: fork() failed!");
        exit(1);
    };
    // Every descendant, not only the command: orphans come here (`become_subreaper`) and may outlive it by far.
    let mut code = None;
    loop {
        match sys::wait_any() {
            sys::Waited::Ended { pid, status } => {
                if pid == command_pid {
                    code = Some(reaper::exit_code(status));
                }
                if code.is_some() {
                    sys::forget_group_when_empty();
                }
            }
            sys::Waited::NoneLeft => break,
            sys::Waited::Failed(e) => {
                eprintln!("reaper: wait failed: {e}");
                exit(1);
            }
        }
    }
    exit(code.unwrap_or_else(|| {
        eprintln!("reaper: no status of the command came back");
        1
    }));
}
