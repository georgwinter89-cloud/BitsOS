//! UL-3a: BusyBox (`/bin/busybox` and its applet links, built by `user/ports/busybox/build.sh`). Each case is a
//! shell script from `user/ports/busybox/tests/` run as `/bin/sh -c <script> sh <scratch>` with captured output and
//! empty stdin; its stdout must equal `<case>.out`, the transcript of the same busybox on CachyOS
//! (`tests/run-host.sh`), except lines named in `<case>.known` (V2 gaps with their owners). No busybox in the
//! image (sysroot or tarball missing on the building machine): skip, with the reason.

use super::{MS, SEC, SIGKILL, finish, scratch, start, start_as, text, wait_for};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};
use ktest::{TestResult, kassert, ktest};
use proc::stdio::Capture;
use proc::{Process, SpawnOptions};
use uapi::ab::signal::SIGSTOP;

/// Port pairs for net.sh, one per run (Safe tests run several times per boot): 28600.. in lane UL's range.
static PORTS: AtomicU32 = AtomicU32::new(0);

/// What `judge` found besides a match.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Verdict<'a> {
    /// Known lines that were missing (each `.known` entry excuses one missing line, where it is missing).
    pub missing: Vec<&'a str>,
    /// Known entries that excused nothing: the gap is gone (or the entry never matched), remove it.
    pub stale: Vec<&'a str>,
}

/// Compares a transcript with the expected one: every expected line in order, except that each entry of
/// `known` ("<line>: <owner and reason>") lets one missing line with that text be absent at the place it is
/// missing.
pub(crate) fn judge<'a>(expected: &'a str, got: &str, known: &'a str) -> Result<Verdict<'a>, String> {
    let mut known: Vec<(&str, bool)> =
        known.lines().filter(|l| !l.starts_with('#') && !l.trim().is_empty()).map(|l| (l.split_once(": ").map_or(l, |(a, _)| a), false)).collect();
    let mut got_lines = got.lines().peekable();
    let mut v = Verdict::default();
    for (n, want) in expected.lines().enumerate() {
        if got_lines.peek() == Some(&want) {
            got_lines.next();
        } else if let Some(k) = known.iter_mut().find(|(l, used)| !*used && *l == want) {
            k.1 = true;
            v.missing.push(want);
        } else {
            return Err(format!("line {}: expected '{want}', got '{}'", n + 1, got_lines.peek().copied().unwrap_or("<end>")));
        }
    }
    if let Some(extra) = got_lines.next() {
        return Err(format!("extra line '{extra}'"));
    }
    v.stale = known.iter().filter(|(_, used)| !*used).map(|(l, _)| *l).collect();
    Ok(v)
}

/// Kills what is left of session `sid` (a script's background jobs: httpd, nc, sleep keep their shell's session)
/// and waits for the ends; returns how many were still running.
fn sweep_session(sid: u32) -> usize {
    let live = || proc::processes().into_iter().filter(|p| p.sid() == sid && !p.has_exited()).collect::<Vec<_>>();
    let left = live();
    for p in &left {
        p.kill(SIGKILL);
    }
    wait_for(5 * SEC, || live().is_empty());
    left.len()
}

fn case(name: &str, script: &str, expected: &str, known: &str) -> TestResult {
    if vfs::root_ns().lookup("/bin/busybox").is_err() || vfs::root_ns().lookup("/bin/sh").is_err() {
        return ktest::skip("no /bin/busybox in this image (user/ports/busybox/build.sh did not run where the image was built)");
    }
    let dir = scratch(name);
    let k = PORTS.fetch_add(1, Ordering::Relaxed) % 200;
    let (hp, np) = (format!("{}", 28600 + 2 * k), format!("{}", 28601 + 2 * k));
    // The kernel's child leads its own session, so everything the script starts can be found and ended.
    // A case whose first line is "# as: root" runs as uid 0 (on the host: as root of a user namespace, run-host.sh).
    let uid = script.starts_with("# as: root\n").then_some(0);
    let s = start_as("/bin/sh", &["sh", "-c", script, "sh", &dir, &hp, &np], None, false, uid)?;
    let code = finish(&s, 60 * SEC);
    let left = sweep_session(s.p.pid);
    let _ = vfs::root_ns().remove_tree(&dir);
    let code = code?;
    let mut got = text(&s.out);
    if !got.is_empty() && !got.ends_with('\n') {
        got.push('\n');
    }
    got.push_str(&format!("exit {code}\n"));
    let r = judge(expected, &got, known);
    let (missing, stale) = r.as_ref().map(|v| (v.missing.join(" | "), v.stale.join(" | "))).unwrap_or_default();
    klog::event!(Info, "ul", "bb.result", case = name, code = code, ok = r.is_ok(), left = left, known = missing.as_str());
    if !stale.is_empty() {
        // Not a failure: another lane closing a gap must not turn this test red; the entry goes from .known.
        klog::event!(Warn, "ul", "bb.known_stale", case = name, lines = stale.as_str(); "a known V2 gap works now: remove it from the .known file");
    }
    r.map_err(|e| format!("busybox case {name} differs from CachyOS: {e}\n{}", s.both()))?;
    kassert!(left == 0, "busybox case {name}: {left} of its processes still ran after sh ended (killed)");
    Ok(())
}

macro_rules! bb_case {
    ($fn:ident, $name:literal, $test:literal) => {
        fn $fn() -> TestResult {
            case(
                $name,
                include_str!(concat!("../../../../user/ports/busybox/tests/", $name, ".sh")),
                include_str!(concat!("../../../../user/ports/busybox/tests/", $name, ".out")),
                include_str!(concat!("../../../../user/ports/busybox/tests/", $name, ".known")),
            )
        }
        ktest!($test, Safe, $fn);
    };
}

bb_case!(pipeline, "pipeline", "ul.bb.pipeline");
bb_case!(files, "files", "ul.bb.files");
bb_case!(jobs, "jobs", "ul.bb.jobs");
bb_case!(system, "system", "ul.bb.system");
bb_case!(net, "net", "ul.bb.net");
bb_case!(errors, "errors", "ul.bb.errors");
// UL-3b: the A2 subset beyond UL-3a's cases (text tools, coreutils, processes and /proc, network basics, every applet).
bb_case!(texttools, "text", "ul.bb.text");
bb_case!(coreutils, "coreutils", "ul.bb.coreutils");
bb_case!(procs, "procs", "ul.bb.procs");
bb_case!(netbasics, "netbasics", "ul.bb.netbasics");
bb_case!(applets, "applets", "ul.bb.applets");
bb_case!(root, "root", "ul.bb.root");

/// Safe tests run several times per boot (MCP on the Mini-PC): every case once more in the same boot, with new
/// scratch dirs and ports; nothing a run leaves (processes, sockets, files) may be in the way of the next.
/// Two halves, so one test's worst case stays six 60 s case limits (one MCP call), not twelve.
fn rerun() -> TestResult {
    for f in [pipeline, files, jobs, system, net, errors] {
        f()?;
    }
    Ok(())
}
ktest!("ul.bb.rerun", Safe, rerun);

fn rerun2() -> TestResult {
    for f in [texttools, coreutils, procs, netbasics, applets, root] {
        f()?;
    }
    Ok(())
}
ktest!("ul.bb.rerun2", Safe, rerun2);

fn judge_rules() -> TestResult {
    let v = judge("a\n640\nb\n", "a\nb\n", "# c\n640: FS\n")?;
    kassert!(v.missing == ["640"] && v.stale.is_empty(), "{v:?}");
    kassert!(judge("640\n640\n", "", "640: FS\n").is_err(), "one entry excuses one missing line only");
    kassert!(judge("640\nb\n", "600\nb\n", "640: FS\n").is_err(), "a wrong line in place of a known one");
    let v = judge("a\n640\n", "a\n640\n", "640: FS\n")?;
    kassert!(v.missing.is_empty() && v.stale == ["640"], "a gap that works now is stale: {v:?}");
    kassert!(judge("a\n", "a\nx\n", "").is_err(), "an extra line");
    Ok(())
}
ktest!("ul.bb.judge", Safe, judge_rules);

fn live_children(parent: u32, name: &str) -> Vec<Arc<Process>> {
    proc::processes().into_iter().filter(|p| p.ppid() == parent && p.name == name && !p.has_exited()).collect()
}

/// The system init's console shell, stopped while the test's own shell owns the console: a reader without the
/// console as controlling terminal reads it freely (as on Linux), so a running one would take the test's keys.
/// A stopped shell is no exit for init's wait4 (no WUNTRACED), so init starts no new one meanwhile.
struct BootShell {
    init: Option<Arc<Process>>,
    shells: Vec<Arc<Process>>,
}

impl BootShell {
    fn pause() -> Result<Self, String> {
        let init = crate::init_process().filter(|p| !p.has_exited());
        let shells = init.as_ref().map(|i| live_children(i.pid, "sh")).unwrap_or_default();
        for s in &shells {
            s.send_signal(SIGSTOP);
        }
        let ok = wait_for(5 * SEC, || shells.iter().all(|s| proc::job::pgrp_has_stopped(s.pgid())));
        let me = BootShell { init, shells };
        kassert!(ok, "the system's console shell did not stop on SIGSTOP");
        Ok(me)
    }

    /// Gives the console back: ends the stopped shell, so init starts a new one, which takes the console
    /// (TIOCSCTTY) and must lead its own session and group again.
    fn resume(self) -> TestResult {
        let Some(init) = self.init else { return Ok(()) };
        let old: Vec<u32> = self.shells.iter().map(|s| s.pid).collect();
        for s in &self.shells {
            s.kill(SIGKILL);
        }
        if old.is_empty() {
            return Ok(());
        }
        let mut new = None;
        // init's back-off for a shell that ran less than 30 s doubles up to 60 s (initlib::Backoff).
        let found = wait_for(70 * SEC, || {
            new = live_children(init.pid, "sh").into_iter().find(|p| !old.contains(&p.pid));
            new.is_some()
        });
        kassert!(found, "the system's init started no new console shell within 70 s");
        let sh = new.unwrap();
        kassert!(wait_for(5 * SEC, || sh.sid() == sh.pid && sh.pgid() == sh.pid), "new system shell pid {} sid {} pgid {}", sh.pid, sh.sid(), sh.pgid());
        Ok(())
    }
}

/// Types `bytes` on the console as MCP send_keys does (echo not drawn).
fn keys(bytes: &[u8]) {
    console::tty().input(bytes, true, &mut |_| {});
    sched::sleep_ns(150 * MS);
}

/// Waits until stderr holds `marker` at least `n` times.
fn seen(err: &Capture, marker: &str, n: usize) -> bool {
    wait_for(10 * SEC, || text(err).matches(marker).count() >= n)
}

fn session_list(sid: u32) -> String {
    let all: Vec<String> = proc::processes().into_iter().filter(|p| p.sid() == sid).map(|p| format!("{} {} pgid {} exited {}", p.pid, p.name, p.pgid(), p.has_exited())).collect();
    all.join(", ")
}

fn in_session(sid: u32, name: &str) -> Option<Arc<Process>> {
    proc::processes().into_iter().find(|p| p.sid() == sid && p.name == name && !p.has_exited())
}

/// The console shell at work, keys typed through the line discipline: history, tab completion, job control
/// (Ctrl+Z stops the foreground job, $? 148, fg, Ctrl+C ends it, $? 130), vi and less; then init's restart of a
/// killed shell. Markers are computed by the shell (`A$((1+1))A` prints A2A), so the typed text never matches.
fn session_keys(sh: &Arc<Process>, err: &Capture) -> TestResult {
    let sid = sh.pid;
    let e = || text(err);
    // The full gate (UL-5, all console ktests in one boot) once handed ash a stray '{' before the first line
    // ("/bin/sh: {echo: not found"; cause not found, an open bug in plan/notes/ul-UL-5.md): let the VT drain its
    // events, drop what the line discipline holds (tcflush(TCIFLUSH)), Ctrl+U clears what ash's line editor read
    // already. A failed first line is typed once more with a marker of its own (A3A), and only when the first
    // one's answer has still not come: so a slow answer is never typed twice, and the history check below asks
    // for the marker of the line typed last, twice. The retry is a Warn event; the gate's summary shows it.
    wait_for(5 * SEC, || console::vt::input_pending() == 0);
    console::tty().flush();
    keys(b"\x15");
    keys(b"echo A$((1+1))A >&2\n");
    let mut last = "A2A";
    if !seen(err, last, 1) {
        klog::event!(Warn, "ul", "bb.first_line_retry", stderr = e().as_str());
        if !e().contains(last) {
            keys(b"\x15");
            keys(b"echo A$((1+2))A >&2\n");
            last = "A3A";
        }
        kassert!(seen(err, last, 1), "no answer to a typed line; stderr:\n{}", e());
    }
    keys(b"\x1b[A");
    keys(b"\n");
    kassert!(seen(err, last, 2), "history (Up, Enter) did not repeat the line; stderr:\n{}", e());
    keys(b"ech\t");
    keys(b"B$((2+1))B >&2\n");
    kassert!(seen(err, "B3B", 1), "tab completion of 'ech'; stderr:\n{}", e());

    keys(b"/bin/sleep 30\n"); // a path: plain `sleep` is an ash builtin (ASH_SLEEP) and would stop the shell itself
    let mut sl = None;
    kassert!(wait_for(10 * SEC, || { sl = in_session(sid, "sleep"); sl.is_some() }), "sleep did not start; session: {}; stderr:\n{}", session_list(sid), e());
    let sl = sl.unwrap();
    kassert!(sl.pgid() == sl.pid, "a job runs in a group of its own (job control): sleep pid {} pgid {}", sl.pid, sl.pgid());
    keys(&[0x1a]);
    kassert!(wait_for(10 * SEC, || proc::job::pgrp_has_stopped(sl.pid)), "Ctrl+Z did not stop the foreground job");
    keys(b"echo S$?S >&2\n");
    kassert!(seen(err, "S148S", 1), "$? after Ctrl+Z (128 + SIGTSTP); stderr:\n{}", e());
    keys(b"fg >&2\n");
    kassert!(wait_for(10 * SEC, || !proc::job::pgrp_has_stopped(sl.pid)), "fg did not continue the job; stderr:\n{}", e());
    sched::sleep_ns(300 * MS);
    keys(&[0x03]);
    kassert!(wait_for(10 * SEC, || sl.has_exited()), "Ctrl+C did not end the foreground job");
    keys(b"echo C$?C >&2\n");
    kassert!(seen(err, "C130C", 1), "$? after Ctrl+C (128 + SIGINT); stderr:\n{}", e());

    let f = scratch("vi");
    keys(format!("vi {f}\n").as_bytes());
    kassert!(wait_for(10 * SEC, || in_session(sid, "vi").is_some()), "vi did not start; stderr:\n{}", e());
    sched::sleep_ns(500 * MS);
    keys(b"ihello vi");
    keys(&[0x1b]); // alone: Esc with more bytes right after it reads as an escape sequence
    sched::sleep_ns(500 * MS);
    keys(b":wq\n");
    kassert!(wait_for(10 * SEC, || in_session(sid, "vi").is_none()), "vi did not end on :wq");
    keys(format!("cat {f} >&2\n").as_bytes());
    let wrote = seen(err, "hello vi", 1);
    keys(format!("less {f}\n").as_bytes());
    let less = wait_for(10 * SEC, || in_session(sid, "less").is_some());
    sched::sleep_ns(500 * MS);
    keys(b"q");
    let quit = wait_for(10 * SEC, || in_session(sid, "less").is_none());
    let _ = vfs::root_ns().remove_tree(&f);
    kassert!(wrote, "the file vi wrote; stderr:\n{}", e());
    kassert!(less && quit, "less started {less}, ended on q {quit}");
    keys(b"echo L$?L >&2\n");
    kassert!(seen(err, "L0L", 1), "less status; stderr:\n{}", e());
    Ok(())
}

/// init starts `/bin/sh` (busybox ash) as the console shell: a session leader with the console as controlling
/// terminal and its group in the foreground, so ash turns job control on. ash is interactive only when fd 0 and 1
/// are terminals, so init (and through it the shell) keeps the console on both, as at boot; fd 2 is captured, where
/// ash would say "job control turned off" and init's spawn hook would complain about TIOCSCTTY/TIOCSPGRP. Then
/// keys typed on the console (`session_keys`), then the shell is killed and init's next one must again lead its
/// session. Cleanup: init first (so it cannot start another shell that takes the console), then every process
/// of the shells' sessions, then the system's shell is restarted (`BootShell::resume`).
/// QemuOnly: it takes the console from whoever has it, which on the Mini-PC is Georg's shell.
fn console_shell() -> TestResult {
    if vfs::root_ns().lookup("/bin/busybox").is_err() {
        return ktest::skip("no /bin/busybox in this image");
    }
    let boot = BootShell::pause()?;
    let err = Capture::new(1 << 16);
    let mut o = SpawnOptions::new(&["/bin/init", "--shell", "/bin/sh"], &["HOME=/", "PATH=/bin"], policy::ProgramId(1));
    o.stderr = Some(err.clone() as Arc<dyn vfs::File>);
    o.uid = 0; // as at boot: root may take the console from an older session (TIOCSCTTY with 1)
    let init = match proc::spawn_with("/bin/init", &o) {
        Ok(i) => i,
        Err(e) => {
            let _ = boot.resume();
            return Err(format!("spawn init: {e}"));
        }
    };
    let mut sessions: Vec<u32> = Vec::new();
    let r = (|| -> TestResult {
        let mut shell = None;
        let found = wait_for(20 * SEC, || {
            shell = live_children(init.pid, "sh").into_iter().next();
            shell.is_some()
        });
        kassert!(found, "init did not start /bin/sh; stderr:\n{}", text(&err));
        let sh = shell.unwrap();
        sessions.push(sh.pid);
        // Long enough for ash to set up job control (or to give up and say so).
        sched::sleep_ns(1500 * MS);
        let e = text(&err);
        kassert!(!sh.has_exited(), "the shell ended: {:?}; stderr:\n{e}", sh.status());
        kassert!(sh.sid() == sh.pid && sh.pgid() == sh.pid, "shell pid {} sid {} pgid {}", sh.pid, sh.sid(), sh.pgid());
        kassert!(!e.contains("job control turned off") && !e.contains("init: console shell"), "stderr:\n{e}");
        // UL-3b: init's links for the paths Linux scripts name (`#!/usr/bin/env sh`, /sbin/ip, /var/run/*.pid).
        let env = start("/usr/bin/env", &["env", "sh", "-c", "echo via-usr-bin; test -d /var/run && test -x /sbin/ip && echo links"], None, false)?;
        let code = finish(&env, 20 * SEC)?;
        kassert!(code == 0 && text(&env.out) == "via-usr-bin\nlinks\n", "/usr/bin/env: code {code}\n{}", env.both());
        session_keys(&sh, &err)?;

        // A killed shell: init starts the next one (back-off 1 s), which takes the console again.
        let before = text(&err).len();
        sh.kill(SIGKILL);
        let mut next = None;
        let found = wait_for(20 * SEC, || {
            next = live_children(init.pid, "sh").into_iter().find(|p| p.pid != sh.pid);
            next.is_some()
        });
        kassert!(found, "init did not restart the killed shell; stderr:\n{}", text(&err));
        let sh2 = next.unwrap();
        sessions.push(sh2.pid);
        sched::sleep_ns(1500 * MS);
        let e = text(&err);
        let after = e.get(before..).unwrap_or("");
        kassert!(sh2.sid() == sh2.pid && sh2.pgid() == sh2.pid, "restarted shell pid {} sid {} pgid {}", sh2.pid, sh2.sid(), sh2.pgid());
        kassert!(!after.contains("job control turned off") && !after.contains("init: console shell"), "restarted shell stderr:\n{after}");
        keys(b"echo R$((3+4))R >&2\n");
        kassert!(seen(&err, "R7R", 1), "the restarted shell does not read the console; stderr:\n{}", text(&err));
        Ok(())
    })();
    init.kill(SIGKILL);
    let _ = init.wait(Some(10 * SEC));
    let left: usize = sessions.iter().map(|&s| sweep_session(s)).sum();
    klog::event!(Info, "ul", "bb.console_cleanup", sessions = sessions.len(), left = left);
    let back = boot.resume();
    r.and(back)
}
ktest!("ul.bb.console_shell", QemuOnly, console_shell);
