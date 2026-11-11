//! GM-2b: the built stand-in with real child processes on the host (shell scripts as the "game"): orphans, signals, exit codes.
//! Nothing of Steam runs. Every wait is bounded and every script's own loop ends by itself, so a failure cannot stall the machine.

use std::fs;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const REAPER: &str = env!("CARGO_BIN_EXE_reaper");
const LIMIT: Duration = Duration::from_secs(30);
/// Sh: `await_file FILE` waits for a file, at most 20 s.
const UNTIL: &str = r#"await_file() { n=0; while [ ! -e "$1" ] && [ $n -lt 400 ]; do sleep 0.05; n=$((n + 1)); done; }; "#;

fn dir(name: &str) -> PathBuf {
    let d = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("reaper-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn reaper(d: &Path, args: &[&str]) -> Command {
    let mut c = Command::new(REAPER);
    c.args(args).current_dir(d).stdin(Stdio::null());
    c
}

fn output(d: &Path, args: &[&str]) -> Output {
    reaper(d, args).output().expect("the stand-in starts")
}

fn spawn(d: &Path, script: &str) -> Child {
    let script = format!("{UNTIL}{script}");
    reaper(d, &["SteamLaunch", "AppId=480", "--", "sh", "-c", &script]).stdout(Stdio::null()).stderr(Stdio::null()).spawn().expect("the stand-in starts")
}

fn upto(what: &str, mut ok: impl FnMut() -> bool) {
    let start = Instant::now();
    while !ok() {
        assert!(start.elapsed() < LIMIT, "not within {LIMIT:?}: {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn file(d: &Path, name: &str) {
    upto(name, || d.join(name).exists());
}

fn end(c: &mut Child) -> i32 {
    let mut code = None;
    upto("the stand-in ends", || {
        code = c.try_wait().unwrap();
        code.is_some()
    });
    code.unwrap().code().expect("the stand-in ends by exit, not by a signal")
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn signal(pid: u32, sig: i32) {
    // SAFETY: integer arguments; the pid is our own child's, not yet waited for.
    assert_eq!(unsafe { libc::kill(pid as i32, sig) }, 0);
}

fn parent_of(pid: &str) -> Option<u32> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit_once(')')?.1.split_whitespace().nth(1)?.parse().ok()
}

/// Waits until the stand-in has waited for the command, whose script wrote its pid to the file `pid`: a child that was
/// not yet waited for is still in /proc's list of children, zombie or not.
fn command_collected(d: &Path, r: &Child) {
    file(d, "pid");
    let pid = fs::read_to_string(d.join("pid")).unwrap();
    let list = format!("/proc/{0}/task/{0}/children", r.id());
    upto("the stand-in collected the command", || !fs::read_to_string(&list).unwrap().split_whitespace().any(|c| c == pid.trim()));
}

#[test]
fn no_command_says_so_as_valves_does() {
    let d = dir("none");
    for args in [&[][..], &["SteamLaunch", "AppId=480"][..], &["SteamLaunch", "--"][..]] {
        let o = output(&d, args);
        assert_eq!(o.status.code(), Some(1), "{args:?}");
        assert!(text(&o.stderr).contains("reaper: no sub-command!"), "{args:?}: {}", text(&o.stderr));
    }
}

#[test]
fn the_commands_exit_code_comes_back() {
    let d = dir("code");
    for (script, want) in [("exit 0", 0), ("exit 42", 42), ("kill -KILL $$", 137), ("kill -TERM $$", 143)] {
        let o = output(&d, &["--", "sh", "-c", script]);
        assert_eq!(o.status.code(), Some(want), "{script}: {}", text(&o.stderr));
    }
}

#[test]
fn a_command_that_cannot_start_is_named() {
    let d = dir("cannot");
    let o = output(&d, &["--", "./no-such-program", "secret-argument"]);
    assert_eq!(o.status.code(), Some(127));
    let e = text(&o.stderr);
    assert!(e.contains("cannot run ./no-such-program") && !e.contains("secret-argument"), "{e}");
    fs::write(d.join("plain"), "not a program").unwrap();
    assert_eq!(output(&d, &["--", "./plain"]).status.code(), Some(126));
}

#[test]
fn valves_words_are_accepted_named_and_the_argv_passes_as_it_is() {
    let d = dir("argv");
    let o = output(&d, &["SteamLaunch", "AppId=480", "ticket=abc", "--", "sh", "-c", r#"printf '<%s>' "$0" "$@""#, "wrapper", "--verb=x", "--", "a b", ""]);
    assert_eq!(o.status.code(), Some(0));
    assert_eq!(text(&o.stdout), "<wrapper><--verb=x><--><a b><>");
    let e = text(&o.stderr);
    assert!(e.contains("reaper: ignoring SteamLaunch\n") && e.contains("reaper: ignoring AppId=480\n") && e.contains("reaper: ignoring ticket=(value not shown)\n"), "{e}");
    assert!(!e.contains("abc"), "a value in the log: {e}");
}

#[test]
fn it_waits_for_a_grandchild_that_outlives_the_command_and_keeps_the_commands_code() {
    let d = dir("orphan");
    let mut r = spawn(&d, r#"( await_file go; echo done >grandchild-done; exit 99 ) & echo $! >grandchild.new; mv grandchild.new grandchild; exit 5"#);
    file(&d, "grandchild");
    let g = fs::read_to_string(d.join("grandchild")).unwrap();
    let g = g.trim();
    // The orphan comes to the stand-in, not to init: the proof of the subreaper.
    upto("the grandchild's new parent is the stand-in", || parent_of(g) == Some(r.id()));
    std::thread::sleep(Duration::from_millis(300));
    assert!(r.try_wait().unwrap().is_none(), "the stand-in ended with a descendant still running");
    fs::write(d.join("go"), "").unwrap();
    assert_eq!(end(&mut r), 5, "the grandchild's 99 must not count");
    assert!(d.join("grandchild-done").exists(), "the stand-in ended before the grandchild");
}

#[test]
fn a_zombie_orphan_is_collected_too() {
    let d = dir("zombie");
    // The middle process ends first, then the grandchild; the command ends last: two orphan ends in a row.
    let mut r = spawn(&d, r#"sh -c '( sleep 0.1; exit 3 ) & exit 4'; sleep 0.3; exit 6"#);
    assert_eq!(end(&mut r), 6);
}

/// The command and a child of it trap `sig`, each writes a file; the command ends with 7 after its child.
fn forwards(name: &str, sig: i32, word: &str) {
    let d = dir(name);
    let inner = format!(r#"trap 'echo got >child-got; exit 0' {word}; echo up >child-up; n=0; while [ $n -lt 400 ]; do sleep 0.05; n=$((n + 1)); done; :"#);
    let script = format!(r#"trap 'echo got >command-got; exit 7' {word}; sh -c "$CHILD_SCRIPT"; :"#);
    let script = format!("{UNTIL}{script}");
    let mut r = reaper(&d, &["SteamLaunch", "AppId=480", "--", "sh", "-c", &script]).env("CHILD_SCRIPT", &inner).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
    file(&d, "child-up");
    std::thread::sleep(Duration::from_millis(100));
    signal(r.id(), sig);
    file(&d, "child-got");
    file(&d, "command-got");
    assert_eq!(end(&mut r), 7, "the stand-in lives through the signal and gives the command's code");
}

#[test]
fn term_goes_to_the_whole_group() {
    forwards("term", libc::SIGTERM, "TERM");
}

#[test]
fn int_goes_to_the_whole_group() {
    forwards("int", libc::SIGINT, "INT");
}

#[test]
fn a_second_term_goes_on_as_well() {
    let d = dir("twice");
    let mut r = spawn(&d, r#"trap 'echo x >>got; [ "$(wc -l <got)" -lt 2 ] || exit 8' TERM; echo up >up; n=0; while [ $n -lt 400 ]; do sleep 0.05; n=$((n + 1)); done"#);
    file(&d, "up");
    signal(r.id(), libc::SIGTERM);
    file(&d, "got");
    signal(r.id(), libc::SIGTERM);
    assert_eq!(end(&mut r), 8);
}

#[test]
fn the_group_is_its_own_not_the_callers() {
    let d = dir("group");
    let o = output(&d, &["--", "sh", "-c", r#"set -- $(sed 's/.*) //' /proc/$$/stat); [ "$3" = $$ ] && echo own"#]);
    assert_eq!(text(&o.stdout).trim(), "own", "{}", text(&o.stderr));
}

/// A process of the host with the group's number gone is not something a test can provoke, so this shows the outside of it: the
/// command is gone and its group empty, an orphan outside the group goes on, and a TERM then changes nothing.
#[test]
fn a_term_after_the_command_and_its_group_are_gone_touches_nothing() {
    let d = dir("late");
    let mut r = spawn(&d, r#"setsid sh -c 'trap "echo hit >orphan-hit" TERM; n=0; while [ ! -e go ] && [ $n -lt 400 ]; do sleep 0.05; n=$((n + 1)); done; echo done >orphan-done' & echo $$ >pid.new; mv pid.new pid; exit 5"#);
    command_collected(&d, &r);
    signal(r.id(), libc::SIGTERM);
    std::thread::sleep(Duration::from_millis(200));
    assert!(r.try_wait().unwrap().is_none(), "the stand-in ended with a descendant still running");
    fs::write(d.join("go"), "").unwrap();
    assert_eq!(end(&mut r), 5, "the command's code stays");
    file(&d, "orphan-done");
    assert!(!d.join("orphan-hit").exists(), "the orphan left the group and must get no forward");
}

/// A member of the group that outlives the command is why the group is not simply forgotten when the command ends.
#[test]
fn a_term_after_the_command_still_reaches_a_member_of_its_group() {
    let d = dir("member");
    let mut r = spawn(&d, r#"( trap 'echo got >member-got; exit 0' TERM; echo up >up; n=0; while [ $n -lt 400 ]; do sleep 0.05; n=$((n + 1)); done ) & await_file up; echo $$ >pid.new; mv pid.new pid; exit 5"#);
    command_collected(&d, &r);
    signal(r.id(), libc::SIGTERM);
    file(&d, "member-got");
    assert_eq!(end(&mut r), 5);
}

/// The window around the handler's start: however early a TERM comes, the stand-in dies of it (no command yet) or the command
/// does (143); it never hangs on with the command running. Which of the two happens is the machine's business.
#[test]
fn a_term_right_after_the_start_is_not_lost() {
    let d = dir("early");
    for round in 0..40u64 {
        let mut r = reaper(&d, &["--", "sleep", "20"]).stderr(Stdio::null()).spawn().unwrap();
        std::thread::sleep(Duration::from_micros(round * 50));
        signal(r.id(), libc::SIGTERM);
        let mut status = None;
        upto("the stand-in ends after an early TERM", || {
            status = r.try_wait().unwrap();
            status.is_some()
        });
        let status = status.unwrap();
        assert!(status.signal() == Some(libc::SIGTERM) || status.code() == Some(143), "round {round}: {status:?}");
    }
}

/// What the caller blocked stays blocked in the stand-in, and for the command; what it did not block is not blocked.
#[test]
fn the_callers_mask_stays() {
    let d = dir("mask");
    let blocked = |pid: u32| -> u64 {
        let status = fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
        let line = status.lines().find_map(|l| l.strip_prefix("SigBlk:")).expect("a SigBlk line");
        u64::from_str_radix(line.trim(), 16).unwrap()
    };
    let term = 1u64 << (libc::SIGTERM - 1);
    let int = 1u64 << (libc::SIGINT - 1);
    let script = "echo $$ >pid.new; mv pid.new pid; await_file go";

    let mut free = spawn(&d, script);
    file(&d, "pid");
    upto("the stand-in's mask is back", || blocked(free.id()) & (term | int) == 0);
    fs::write(d.join("go"), "").unwrap();
    assert_eq!(end(&mut free), 0);
    fs::remove_file(d.join("go")).unwrap();
    fs::remove_file(d.join("pid")).unwrap();

    let script = format!("{UNTIL}{script}");
    let mut held = reaper(&d, &["--", "sh", "-c", &script]);
    // SAFETY: the closure only makes the two async-signal-safe calls on values of its own, between fork and exec.
    unsafe {
        held.pre_exec(|| {
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            libc::sigaddset(&mut set, libc::SIGTERM);
            libc::sigprocmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
            Ok(())
        });
    }
    let mut held = held.stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
    file(&d, "pid");
    // Long enough for the stand-in to be past its fork and its own mask step (the command's pid file says it has forked).
    std::thread::sleep(Duration::from_millis(200));
    let (stand_in, command) = (blocked(held.id()), blocked(fs::read_to_string(d.join("pid")).unwrap().trim().parse().unwrap()));
    assert_eq!((stand_in & term, stand_in & int), (term, 0), "the stand-in");
    assert_eq!(command & term, term, "the command");
    fs::write(d.join("go"), "").unwrap();
    assert_eq!(end(&mut held), 0);
}

fn signal_lines(c: &mut Command) -> String {
    let o = c.args(["-E", "^Sig(Blk|Ign):", "/proc/self/status"]).stdin(Stdio::null()).output().expect("grep runs");
    assert!(o.status.success());
    text(&o.stdout)
}

#[test]
fn the_command_finds_its_signals_as_without_the_stand_in() {
    let d = dir("signals");
    let plain = signal_lines(&mut Command::new("grep"));
    let mut through = reaper(&d, &["--", "grep"]);
    assert_eq!(signal_lines(&mut through), plain);
}

#[test]
fn an_ignored_signal_stays_ignored_for_the_command() {
    let d = dir("ignored");
    let script = format!(r#"trap '' INT; exec '{REAPER}' -- grep '^SigIgn:' /proc/self/status"#);
    let o = Command::new("sh").args(["-c", &script]).current_dir(&d).stdin(Stdio::null()).output().unwrap();
    let line = text(&o.stdout);
    let mask = u64::from_str_radix(line.split_whitespace().nth(1).expect("a SigIgn line"), 16).unwrap();
    assert_ne!(mask & 1 << (libc::SIGINT - 1), 0, "{line}");
    assert_eq!(mask & 1 << (libc::SIGPIPE - 1), 0, "{line}");
}

#[test]
fn it_opens_nothing_for_the_command() {
    let d = dir("fds");
    // What ls sees without the stand-in, with the same stdio: anything this test inherited is in both (the gate's queue
    // holds its lock file open down to every test), and ls's own open of the directory too.
    let plain = Command::new("ls").arg("/proc/self/fd").current_dir(&d).stdin(Stdio::null()).output().unwrap();
    assert!(plain.status.success(), "{}", text(&plain.stderr));
    let o = output(&d, &["--", "ls", "/proc/self/fd"]);
    assert_eq!(text(&o.stdout), text(&plain.stdout), "{}", text(&o.stderr));
}

/// Static only for the image's target (`--target x86_64-unknown-linux-musl`, as build_user builds it): a plain host
/// `cargo test` links against the host's glibc.
#[test]
fn the_binary_is_64_bit_and_static_for_the_image() {
    let elf = fs::read(REAPER).unwrap();
    let word = |at: usize, n: usize| elf[at..at + n].iter().rev().fold(0u64, |v, b| v << 8 | u64::from(*b)) as usize;
    assert_eq!(&elf[..5], b"\x7fELF\x02", "ELF, 64-bit");
    assert_eq!(word(18, 2), 62, "x86-64");
    let (phoff, phsize, phnum) = (word(32, 8), word(54, 2), word(56, 2));
    const PT_INTERP: usize = 3;
    let dynamic = (0..phnum).any(|i| word(phoff + i * phsize, 4) == PT_INTERP);
    assert_eq!(dynamic, !cfg!(target_env = "musl"), "dynamic loader against the target");
}
