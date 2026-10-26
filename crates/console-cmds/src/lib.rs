//! The console's parts that need processes (owner IN), above `proc` (which sits above `mcp`, which uses
//! `console`): the monitor commands `ps` and `run`, the console's read-abort check (a killed program's blocked
//! read ends), and IN-4's process hook `console::jobs::Procs` (the terminal's caller, its process group and
//! session, signals for a process group, the caller's memory for ioctl arguments).
//!
//! `run <path> [args]` starts a static Linux program with stdin/stdout/stderr on `/dev/console` and makes the
//! console its controlling terminal with the program's group in the foreground (what a shell does with setsid +
//! TIOCSCTTY + tcsetpgrp), so Ctrl+C, Ctrl+\ and Ctrl+Z reach it as SIGINT, SIGQUIT and SIGTSTP from the line
//! discipline (until AB-6 a signal with a handler stays pending; default actions end the program at once).
//! Afterwards the terminal settings it changed are restored and the console takes screen and keyboard back.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

mod ktests;
mod ktests_in6;
mod ktests_pty;

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use console::jobs::{Caller, Ctl};
use console::monitor::Ctx;
use proc::{ExitStatus, Process};

/// The program id of things started from the console (policy B3: the person at the keyboard).
pub const CONSOLE_PROGRAM: policy::ProgramId = policy::ProgramId(2);
const SLICE_NS: u64 = 100_000_000;
/// Ctrl+C presses (VINTR with ISIG) after which `run` stops waiting for SIGINT to work and ends the program with
/// SIGKILL: the first is SIGINT as on Linux, the third means the program is not listening (a handler that stays
/// pending until AB-6, a blocked or ignored SIGINT). The console is Georg's only way in on bare metal.
const KILL_AFTER_INTERRUPTS: u64 = 3;
const SIGKILL: u32 = 9;

fn ps(c: &mut Ctx) -> Result<(), String> {
    let mut list = proc::processes();
    list.sort_by_key(|p| p.pid);
    c.println(&format!("{:>6} {:>6} {:>6} {:<10} {}", "PID", "PPID", "PGID", "STATE", "COMMAND"));
    for p in &list {
        let state = match p.status() {
            None => String::from("running"),
            Some(ExitStatus::Exited(n)) => format!("exited {n}"),
            Some(ExitStatus::Signaled(s)) => format!("killed {s}"),
        };
        c.println(&format!("{:>6} {:>6} {:>6} {:<10} {}", p.pid, p.ppid(), p.pgid(), state, p.argv.join(" ")));
    }
    if list.is_empty() {
        c.println("(no processes)");
    }
    let threads = sched::threads();
    c.println(&format!("kernel threads: {}", threads.len()));
    Ok(())
}
console::command!(name: "ps", args: "", help: "processes (and the number of kernel threads)", physical_only: false, run: ps);

/// SN-5/SN-8b CR (INT-W8): where the kernel's mixer plays, and HDMI sound off. `hdmi` writes display registers
/// (`hda::hdmi::hdmi_on`); a refusal (a gate, no display engine) is printed and the target stays.
fn snd_cmd(c: &mut Ctx) -> Result<(), String> {
    let args: Vec<&str> = c.args.clone();
    match args.as_slice() {
        ["output", v] => {
            snd::set_output(v)?;
            c.println(&format!("snd output {v}"));
            Ok(())
        }
        ["hdmi", "off"] => {
            // SN-10 CR: the same make-before-break, target reset and serialisation as the file and the MCP tool.
            let line = snd::output::apply(&snd::output::Request::HdmiOff, "console").map_err(|r| r.text)?;
            c.println(&line);
            Ok(())
        }
        _ => Err(String::from("usage: snd output auto|jack|hdmi|usb, or snd hdmi off")),
    }
}
console::command!(name: "snd", args: "output auto|jack|hdmi|usb | hdmi off", help: "where sound plays; HDMI sound off", physical_only: false, run: snd_cmd);

fn run(c: &mut Ctx) -> Result<(), String> {
    let Some(&path) = c.args.first() else { return Err(String::from("usage: run <path> [args]")) };
    let argv: Vec<&str> = c.args.clone();
    let subject = policy::Subject { program: CONSOLE_PROGRAM, pid: 0, uid: 0 };
    if let Err(d) = policy::check(&subject, &policy::Access::Path { path, write: false, create: false }) {
        return Err(format!("refused: {}", d.reason));
    }
    let envp = ["HOME=/", "PATH=/bin", "TERM=linux", "LANG=de_DE.UTF-8"];
    let mut o = proc::SpawnOptions::new(&argv, &envp, CONSOLE_PROGRAM);
    o.held = true;
    let tty = console::tty();
    let saved = tty.termios();
    let p = proc::spawn_with(path, &o).map_err(|e| format!("{path}: {e}"))?;
    // The program leads its own session and group (a child of the kernel): the console becomes its terminal.
    let sid = sid_of(&p);
    tty.with_ctl(&PROCS, |ctl, _| ctl.assign(sid, p.pgid()));
    p.release();
    klog::event!(Info, "console", "run.start", pid = p.pid, path = path, args = argv.len(), injected = c.injected);
    let seen = tty.interrupts();
    let mut killed = false;
    let code = loop {
        if let Some(code) = p.wait(Some(SLICE_NS)) {
            break code;
        }
        let presses = tty.interrupts().saturating_sub(seen);
        if !killed && presses >= KILL_AFTER_INTERRUPTS {
            killed = true;
            klog::event!(Warn, "console", "run.killed", pid = p.pid, path = path, interrupts = presses;
                "Ctrl+C pressed again and again while the program kept running: ended with SIGKILL");
            p.kill(SIGKILL);
        }
    };
    tty.with_ctl(&PROCS, |ctl, _| {
        if ctl.session == Some(sid) {
            *ctl = Ctl::default();
        }
    });
    if tty.termios() != saved {
        tty.set_termios(saved, false);
    }
    console::vt::reclaim();
    if matches!(p.status(), Some(ExitStatus::Signaled(_))) {
        // After the ^C echo (no newline of its own, the shell's job on Linux).
        c.print("\n");
    }
    if let Some(m) = p.end_message() {
        c.println(&m);
    }
    if killed {
        c.println(&format!("[{path}: did not end on Ctrl+C, ended with SIGKILL after {KILL_AFTER_INTERRUPTS} presses]"));
    }
    klog::event!(Info, "console", "run.end", pid = p.pid, code = code);
    if code != 0 {
        c.println(&format!("[{path}: exit code {code}]"));
    }
    Ok(())
}
console::command!(name: "run", args: "<path> [args]", help: "starts a program on the console and waits for it (Ctrl+C sends SIGINT, the third Ctrl+C SIGKILL)", physical_only: false, run: run);

/// A process's session (AB-4's `Group::sid`; IN-4 CR3, INT-A2a).
pub fn sid_of(p: &Process) -> u32 {
    p.sid()
}

fn caller(p: &Arc<Process>) -> Caller {
    Caller { pid: p.pid, pgid: p.pgid(), sid: sid_of(p), uid: p.cred().euid, subject: p.subject() }
}

/// IN-4's process hook for the console terminal (`console::jobs::Procs`).
pub struct ProcHook;
pub static PROCS: ProcHook = ProcHook;

impl console::jobs::Procs for ProcHook {
    fn current(&self) -> Option<Caller> {
        proc::current().map(|p| caller(&p))
    }
    /// `kill_pgrp(pgrp, sig, 1)`: every live member, through AB-6's `proc::kill_pgrp` (IN-4 CR4d, INT-A2a). The
    /// terminal is the user's own keyboard: no B3 question.
    fn signal_pgrp(&self, pgrp: u32, sig: u32) -> usize {
        proc::kill_pgrp(pgrp, sig)
    }
    fn session_of_pgrp(&self, pgrp: u32) -> Option<u32> {
        let all = proc::processes();
        all.iter()
            .find(|p| p.pgid() == pgrp && !p.has_exited())
            .or_else(|| all.iter().find(|p| p.pid == pgrp && !p.has_exited()))
            .map(|p| sid_of(p))
    }
    fn alive(&self, pid: u32) -> bool {
        proc::find(pid).is_some_and(|p| !p.has_exited())
    }
    fn ignores(&self, sig: u32) -> bool {
        let Some(p) = proc::current() else { return false };
        if !(1..=64).contains(&sig) {
            return false;
        }
        p.signals.with(|s| s.blocked & (1u64 << (sig - 1)) != 0 || s.action(sig).handler == uapi::ab::sigabi::SIG_IGN)
    }
    fn read_user(&self, addr: u64, buf: &mut [u8]) -> bool {
        proc::current().is_some_and(|p| p.mm.copy_from_user(buf, addr).is_ok())
    }
    fn write_user(&self, addr: u64, data: &[u8]) -> bool {
        proc::current().is_some_and(|p| p.mm.copy_to_user(addr, data).is_ok())
    }
    /// One process (IN-7: a pty hang-up's SIGHUP and SIGCONT for the session leader), SI_KERNEL.
    fn signal_proc(&self, pid: u32, sig: u32) -> bool {
        proc::find(pid).is_some_and(|p| !p.has_exited() && !matches!(p.send_signal(sig), proc::signal::Sent::Dropped))
    }
    fn pgrp_stopped(&self, pgrp: u32) -> bool {
        proc::job::pgrp_has_stopped(pgrp)
    }
    /// `will_become_orphaned_pgrp` (linux-6.6/kernel/exit.c:326): members whose parent is init (pid 1), the
    /// kernel (0) or gone (Linux re-parents those to init) do not count.
    fn pgrp_orphaned(&self, pgrp: u32) -> bool {
        let all = proc::processes();
        !all.iter().filter(|p| p.pgid() == pgrp && !p.has_exited()).any(|p| {
            let pp = p.ppid();
            if pp <= 1 {
                return false;
            }
            match all.iter().find(|q| q.pid == pp && !q.has_exited()) {
                Some(parent) => parent.pgid() != pgrp && sid_of(parent) == sid_of(p),
                None => false,
            }
        })
    }
}

/// A blocked console read ends when its process was told to end (kill, a fatal signal).
fn read_abort() -> bool {
    proc::current().is_some_and(|p| p.has_exited())
}

fn init() -> Result<(), &'static str> {
    console::set_read_abort(read_abort);
    console::jobs::set_procs(&PROCS);
    Ok(())
}
kreg::initcall!(kreg::Level::Service, "console-cmds", init);
