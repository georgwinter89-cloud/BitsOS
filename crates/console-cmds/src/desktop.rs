//! DS-4: the desktop starts by itself after boot (CONFIG `desktop=`, resolved by `console::bootscreen`). The
//! `desktop` thread waits for init (pid 1 alive and user 1000's runtime directory made), then starts the desktop
//! program the way init's console shell starts it when Georg types `bitscomp` (root, the shell's environment; the
//! program gives up root itself). The start counts as done when the program takes the screen (`vt::graphics`);
//! a program that ends before is a failed start. After `ATTEMPTS` failed starts (1 s, then 2 s apart) the console
//! shows the text console with a message; typing `bitscomp` in the shell still works, `desktop start` on the
//! monitor tries again. A desktop that ends after it took the screen is not restarted here (also when it ends
//! within the same poll): the console takes the screen back. The decisions are pure functions (`init_wait`,
//! `poll`, `next`) so host tests cover them without QEMU.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use console::bootsteps::Init;
use console::monitor::Ctx;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

pub(crate) const ATTEMPTS: u32 = 3;
const POLL_NS: u64 = 100_000_000;
/// How long the start waits for init and its runtime directory.
pub(crate) const WAIT_INIT_NS: u64 = 120_000_000_000;
/// A running desktop that has not taken the screen by then: the text console comes back (it keeps running).
pub(crate) const TAKE_SCREEN_NS: u64 = 20_000_000_000;
/// User 1000's runtime directory (init makes it; bitscomp's sockets).
const RUNTIME_DIR: &str = "/run/user/1000";
/// init's console shell PATH (user/init: SHELL_PATH).
const SHELL_PATH: &str = "PATH=/bin:/sbin:/usr/bin:/boot/BITSOS2/bin";
/// glibc locales as init picks LANG (user/init LANGS): the first whose LC_CTYPE exists.
const LANGS: [(&str, &str); 2] = [("LANG=de_DE.UTF-8", "/usr/lib/locale/de_DE.utf8/LC_CTYPE"), ("LANG=C.UTF-8", "/usr/lib/locale/C.utf8/LC_CTYPE")];

static RUNNING: AtomicBool = AtomicBool::new(false);
static PID: AtomicU32 = AtomicU32::new(0);
static FAILED: AtomicU32 = AtomicU32::new(0);

fn start() -> Result<(), &'static str> {
    if console::bootscreen::desktop_on() {
        spawn_start();
    }
    Ok(())
}
kreg::initcall!(kreg::Level::Late, "console-desktop", start);

/// Starts the `desktop` thread unless one runs; false if it could not start.
fn spawn_start() -> bool {
    if RUNNING.swap(true, Ordering::AcqRel) {
        return true;
    }
    match sched::spawn("desktop", sched::Class::Normal, thread) {
        Ok(_) => true,
        Err(e) => {
            RUNNING.store(false, Ordering::Release);
            klog::event!(Error, "console", "desktop.spawn_failed", error = format!("{e:?}"));
            false
        }
    }
}

/// Waiting for init: what to do now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InitWait {
    Ready,
    Wait,
    /// init is off or gave up: no desktop.
    NoInit,
    /// init did not get ready within `WAIT_INIT_NS`.
    Timeout,
}

pub(crate) fn init_wait(state: Init, ready: bool, waited_ns: u64) -> InitWait {
    match state {
        Init::Off | Init::GivenUp => InitWait::NoInit,
        Init::Running if ready => InitWait::Ready,
        _ if waited_ns > WAIT_INIT_NS => InitWait::Timeout,
        _ => InitWait::Wait,
    }
}

/// How one start ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// The program (pid) has the screen.
    Up(u32),
    /// It runs but has not taken the screen within `TAKE_SCREEN_NS`.
    Slow(u32),
    /// It could not start, or ended before it took the screen (the reason word).
    Failed(&'static str),
    /// It took the screen and ended (the console takes the screen back; no restart).
    TookThenEnded(&'static str),
}

/// One look at a started program: `ended` its exit reason once it ended, `has_screen` it is the screen's owner
/// now, `took` it took the screen at some point. None: keep waiting.
pub(crate) fn poll(pid: u32, ended: Option<&'static str>, has_screen: bool, took: bool, waited_ns: u64) -> Option<Outcome> {
    match ended {
        Some(r) if took || has_screen => Some(Outcome::TookThenEnded(r)),
        Some(r) => Some(Outcome::Failed(r)),
        None if has_screen => Some(Outcome::Up(pid)),
        None if waited_ns > TAKE_SCREEN_NS => Some(Outcome::Slow(pid)),
        None => None,
    }
}

/// After start `attempt` (1-based) ended with `o`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Next {
    Up(u32),
    Slow(u32),
    /// Ended after it took the screen: stop, no restart.
    Ended(&'static str),
    /// Try again after this many ns.
    Retry(u64),
    /// `ATTEMPTS` failed starts: the text console (the last reason).
    GiveUp(&'static str),
}

pub(crate) fn next(attempt: u32, o: Outcome) -> Next {
    match o {
        Outcome::Up(p) => Next::Up(p),
        Outcome::Slow(p) => Next::Slow(p),
        Outcome::TookThenEnded(r) => Next::Ended(r),
        Outcome::Failed(r) if attempt >= ATTEMPTS => Next::GiveUp(r),
        Outcome::Failed(_) => Next::Retry(attempt as u64 * 1_000_000_000),
    }
}

fn thread() {
    run();
    RUNNING.store(false, Ordering::Release);
}

fn run() {
    let line = console::bootscreen::desktop_program();
    let argv: Vec<&str> = line.split_whitespace().collect();
    let Some(&program) = argv.first() else { return };
    klog::event!(Info, "console", "desktop.wait", program = program, args = argv.len() - 1);
    let t0 = time::now_ns();
    loop {
        match init_wait(console::bootscreen::init_state(), init_ready(), time::now_ns().saturating_sub(t0)) {
            InitWait::Ready => break,
            InitWait::Wait => sched::sleep_ns(POLL_NS),
            InitWait::NoInit => {
                klog::event!(Warn, "console", "desktop.no_init"; "init is off or gave up: no desktop, the text console stays");
                console::bootscreen::fallback("no-init", "\nOhne init startet keine Arbeitsfläche: Textkonsole.\n");
                return;
            }
            InitWait::Timeout => {
                klog::event!(Error, "console", "desktop.given_up", attempts = 0, reason = "no_init", waited_s = WAIT_INIT_NS / 1_000_000_000);
                console::bootscreen::fallback("no-init", "\nInit wurde nicht bereit: keine Arbeitsfläche, Textkonsole.\n");
                return;
            }
        }
    }
    for attempt in 1.. {
        klog::event!(Info, "console", "desktop.start", program = program, attempt = attempt);
        let t = time::now_ns();
        let o = start_once(&argv);
        if let Outcome::Failed(reason) = o {
            FAILED.fetch_add(1, Ordering::AcqRel);
            klog::event!(Warn, "console", "desktop.failed", program = program, attempt = attempt, reason = reason);
        }
        match next(attempt, o) {
            Next::Up(pid) => {
                FAILED.store(0, Ordering::Release);
                klog::event!(Info, "console", "desktop.up", program = program, pid = pid, after_ms = (time::now_ns() - t) / 1_000_000);
                return;
            }
            Next::Slow(pid) => {
                klog::event!(Warn, "console", "desktop.slow", program = program, pid = pid, waited_s = TAKE_SCREEN_NS / 1_000_000_000;
                    "the desktop runs but has not taken the screen: text console");
                console::bootscreen::fallback("desktop-slow", &format!("\n{program} läuft (pid {pid}), hat den Bildschirm aber nicht übernommen.\n"));
                return;
            }
            Next::Ended(reason) => {
                klog::event!(Warn, "console", "desktop.ended", program = program, attempt = attempt, reason = reason;
                    "the desktop took the screen and ended: not restarted, the console takes the screen back");
                let why = console::bootsteps::why(reason);
                console::bootscreen::fallback("desktop-ended", &format!("\n{program} hat den Bildschirm übernommen und ist dann {why}.\n"));
                return;
            }
            Next::Retry(ns) => sched::sleep_ns(ns),
            Next::GiveUp(last) => {
                klog::event!(Error, "console", "desktop.given_up", program = program, attempts = ATTEMPTS, reason = last);
                let why = console::bootsteps::why(last);
                console::bootscreen::fallback(
                    "desktop-failed",
                    &format!("\nDie Arbeitsfläche ({program}) ist {ATTEMPTS}-mal nicht gestartet: {why}.\nTextkonsole; `bitscomp` in der Shell startet sie von Hand.\n"),
                );
                return;
            }
        }
    }
}

/// init runs and has made user 1000's runtime directory (bitscomp's sockets go there).
fn init_ready() -> bool {
    console::jobs::procs().alive(1) && vfs::root_ns().lookup(RUNTIME_DIR).is_ok()
}

/// One start of `argv` (argv[0] the program's path), watched until `poll` decides.
fn start_once(argv: &[&str]) -> Outcome {
    match spawn_once(argv) {
        Ok(p) => watch(&p),
        Err(reason) => Outcome::Failed(reason),
    }
}

fn spawn_once(argv: &[&str]) -> Result<alloc::sync::Arc<proc::Process>, &'static str> {
    let program = argv[0];
    let subject = policy::Subject { program: crate::CONSOLE_PROGRAM, pid: 0, uid: 0 };
    if policy::check(&subject, &policy::Access::Path { path: program, write: false, create: false }).is_err() {
        return Err("refused");
    }
    let ns = vfs::root_ns();
    let lang = LANGS.iter().find(|(_, f)| ns.lookup(f).is_ok()).map(|(l, _)| *l);
    let mut envp: Vec<&str> = alloc::vec![SHELL_PATH, "HOME=/root", "TERM=linux", "XDG_RUNTIME_DIR=/run/user/1000"];
    envp.extend(lang);
    let mut o = proc::SpawnOptions::new(argv, &envp, crate::CONSOLE_PROGRAM);
    // As init's shell: root, which bitscomp gives up for user 1000 once its devices are open.
    o.uid = 0;
    // Not the console's reader: init's shell keeps the keyboard (stdout/stderr stay on the console).
    o.stdin = ns.open("/dev/null").ok();
    let p = proc::spawn_with(program, &o).map_err(|e| match e {
        proc::SpawnError::NotFound => "not_found",
        proc::SpawnError::NotExecutable(_) => "not_executable",
        proc::SpawnError::NoMemory => "no_memory",
        _ => "spawn",
    })?;
    PID.store(p.pid, Ordering::Release);
    Ok(p)
}

fn watch(p: &proc::Process) -> Outcome {
    let t = time::now_ns();
    loop {
        let ended = p.wait(Some(POLL_NS)).map(|_| match p.status() {
            Some(proc::ExitStatus::Signaled(_)) => "crashed",
            Some(proc::ExitStatus::Exited(0)) => "ended",
            _ => "exit_code",
        });
        let has = console::vt::kd_owner() == p.pid;
        if let Some(o) = poll(p.pid, ended, has, console::vt::took_screen(p.pid), time::now_ns().saturating_sub(t)) {
            return o;
        }
    }
}

fn desktop_cmd(c: &mut Ctx) -> Result<(), String> {
    match c.args.first().copied() {
        None | Some("status") => {
            let s = console::bootscreen::steps();
            let d = s.get(console::bootsteps::Key::Desktop);
            c.println(&format!(
                "desktop: {} ({}), program {}, last pid {}, failed starts {}, {}",
                if console::bootscreen::desktop_on() { "on" } else { "off" },
                d.state.name(),
                console::bootscreen::desktop_program(),
                PID.load(Ordering::Acquire),
                FAILED.load(Ordering::Acquire),
                if RUNNING.load(Ordering::Acquire) { "starting" } else { "idle" }
            ));
            Ok(())
        }
        Some("start") => {
            let pid = PID.load(Ordering::Acquire);
            if pid != 0 && console::vt::kd_owner() == pid && console::jobs::procs().alive(pid) {
                return Err(format!("desktop: runs already (pid {pid}) and has the screen"));
            }
            if spawn_start() {
                c.println("desktop: starting");
                Ok(())
            } else {
                Err(String::from("desktop: the start thread could not start"))
            }
        }
        Some(_) => Err(String::from("usage: desktop [status|start]")),
    }
}
console::command!(name: "desktop", args: "[status|start]", help: "the desktop the console starts after boot (CONFIG desktop=)", physical_only: false, run: desktop_cmd);
