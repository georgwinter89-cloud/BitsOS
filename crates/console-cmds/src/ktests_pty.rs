//! IN-7 ktests with real processes: `abi-pty jobs` (controlling terminal on open, SIGWINCH from the master's
//! TIOCSWINSZ, a background read with SIGTTIN ignored, ^C from the master ending the child with SIGINT) and
//! `abi-pty hangup` (the master's close sends SIGHUP to the session leader; the hung-up slave reads 0 and writes
//! EIO), `abi-pty basics` (lock, numbers, start settings, echo, poll and epoll on both ends, FIONREAD, the master's
//! write without a slave; compared line by line with CachyOS's transcript `user/abi-tests/expected/pty-basics.txt`).
//! The program's lines are the ones CachyOS prints (`cargo xtask abi-diff abi-pty`). Missing program: skip.

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use ktest::{TestResult, kassert, ktest};
use proc::SpawnOptions;
use proc::stdio::Capture;

const SEC: u64 = 1_000_000_000;

/// Runs `/bin/abi-pty <case>` with stdout captured, as MCP's `run` would (no terminal of its own).
fn run(case: &str) -> Result<String, String> {
    if vfs::root_ns().lookup("/bin/abi-pty").is_err() {
        return Err(String::from("skip"));
    }
    let out = Capture::new(1 << 16);
    let env = ["HOME=/", "PATH=/bin"];
    let argv = ["abi-pty", case];
    let mut o = SpawnOptions::new(&argv, &env, crate::CONSOLE_PROGRAM);
    o.stdout = Some(out.clone() as Arc<dyn vfs::File>);
    let p = proc::spawn_with("/bin/abi-pty", &o).map_err(|e| format!("spawn: {e}"))?;
    let text = || String::from_utf8_lossy(&out.contents()).into_owned();
    if p.wait(Some(30 * SEC)).is_none() {
        p.kill(uapi::ab::signal::SIGKILL);
        let _ = p.wait(Some(5 * SEC));
        return Err(format!("abi-pty {case} did not end within 30 s: {}", text()));
    }
    Ok(text())
}

fn program_jobs() -> TestResult {
    let out = match run("jobs") {
        Err(e) if e == "skip" => return ktest::skip("no /bin/abi-pty (user/abi-tests not in this image)"),
        r => r?,
    };
    for want in ["c t t y   m i n e   f g   m i n e", "w r o n l y   o p e n   c t t y   n o", "b g   r e a d   - 1   e r r n o   5", "w i n c h   2 4 x 8 0", "master TIOCGPGRP is the child: yes", "child: signaled 1 sig 2"] {
        kassert!(out.contains(want), "no {want:?} in: {out}");
    }
    Ok(())
}
ktest!("in.pty.program_jobs", Safe, program_jobs);

fn program_hangup() -> TestResult {
    let out = match run("hangup") {
        Err(e) if e == "skip" => return ktest::skip("no /bin/abi-pty (user/abi-tests not in this image)"),
        r => r?,
    };
    let want = "hup: read 0 write -1 errno 5 poll 0x1d node gone 1 ioctl -1 errno 5\nchild exit 0\n";
    kassert!(out == want, "got: {out:?}");
    Ok(())
}
ktest!("in.pty.program_hangup", Safe, program_hangup);

fn program_basics() -> TestResult {
    let out = match run("basics") {
        Err(e) if e == "skip" => return ktest::skip("no /bin/abi-pty (user/abi-tests not in this image)"),
        r => r?,
    };
    let want = include_str!("../../../user/abi-tests/expected/pty-basics.txt");
    for (i, (g, w)) in out.lines().zip(want.lines()).enumerate() {
        kassert!(g == w, "line {}: got {g:?}, CachyOS {w:?}", i + 1);
    }
    kassert!(out.lines().count() == want.lines().count(), "{} lines, CachyOS {}: {out}", out.lines().count(), want.lines().count());
    Ok(())
}
ktest!("in.pty.program_basics", Safe, program_basics);

/// INT-A2b (IN-7 CR1): `/dev/tty` of a session whose controlling terminal is a pty opens that pty.
fn program_devtty() -> TestResult {
    let out = match run("devtty") {
        Err(e) if e == "skip" => return ktest::skip("no /bin/abi-pty (user/abi-tests not in this image)"),
        r => r?,
    };
    for want in ["d e v t t y   o p e n   o k   s i d   m i n e   y e s", "v i a   / d e v / t t y"] {
        kassert!(out.contains(want), "no {want:?} in: {out}");
    }
    Ok(())
}
ktest!("in.pty.program_devtty", Safe, program_devtty);

/// INT-A2b (IN-7 CR2): TIOCGPTPEER through `sys_ioctl` (the lines CachyOS prints for `abi-pty peer`).
fn program_peer() -> TestResult {
    let out = match run("peer") {
        Err(e) if e == "skip" => return ktest::skip("no /bin/abi-pty (user/abi-tests not in this image)"),
        r => r?,
    };
    let want = "locked: errno 5\npeer: opened cloexec 1\nsame node: 1\nread-only peer: access mode 0\nmaster reads (4): h i \\r \\n\non the slave: -1 errno 5\non /dev/null: -1 ENOTTY\n";
    kassert!(out == want, "got: {out:?}");
    Ok(())
}
ktest!("in.pty.program_peer", Safe, program_peer);
