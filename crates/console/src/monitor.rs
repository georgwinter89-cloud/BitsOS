//! The built-in monitor command line (A1, until busybox exists): a prompt on the console, one command per line
//! from the line discipline, commands from a registry so other crates add their own (`console::command!`; the
//! desk adds `hang` from RB-3, `console-cmds` adds `ps` and `run`).
//!
//! Commands marked `physical_only` (reboot, poweroff) refuse a line in which any key was injected by MCP
//! (`send_keys`), so Claude cannot restart or power off the machine through the console (R8c, R8f); the refusal is
//! an event (`console monitor.refused`).

use crate::screen::PALETTE;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, Ordering};

/// One monitor command. `run` gets the arguments after the name and prints through `Ctx::print`; an `Err` is
/// printed as `<name>: <message>`.
pub struct Command {
    pub name: &'static str,
    /// Argument synopsis for `help`, e.g. "[n]".
    pub args: &'static str,
    pub help: &'static str,
    /// Refuses lines that contain injected (MCP) keys.
    pub physical_only: bool,
    pub run: fn(&mut Ctx) -> Result<(), String>,
}

kreg::declare!(pub COMMANDS: Command = "console_cmd");

/// `console::command!(name: "uptime", args: "", help: "time since boot", physical_only: false, run: f);`
#[macro_export]
macro_rules! command {
    (name: $name:literal, args: $args:literal, help: $help:literal, physical_only: $p:expr, run: $f:path $(,)?) => {
        $crate::__kreg::register!("console_cmd", $crate::monitor::Command, $crate::monitor::Command {
            name: $name, args: $args, help: $help, physical_only: $p, run: $f,
        });
    };
}

/// What a running command sees.
pub struct Ctx<'a> {
    pub args: Vec<&'a str>,
    /// The line had injected keys.
    pub injected: bool,
    /// Everything printed (tests read it).
    pub out: String,
    /// Print to the screen too (false in tests).
    pub echo: bool,
}

impl Ctx<'_> {
    pub fn print(&mut self, s: &str) {
        self.out.push_str(s);
        if self.echo {
            crate::vt::write(s, crate::screen::FG_INDEX);
        }
    }
    pub fn println(&mut self, s: &str) {
        self.print(s);
        self.print("\n");
    }
}

/// Splits a command line into words; "double quotes" keep spaces, `\"` and `\\` escape inside them.
pub fn split(line: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let (mut quoted, mut have) = (false, false);
    let mut it = line.chars();
    while let Some(c) = it.next() {
        match c {
            '"' => {
                quoted = !quoted;
                have = true;
            }
            '\\' if quoted => match it.next() {
                Some(n) => cur.push(n),
                None => return Err(String::from("a backslash at the end")),
            },
            c if c.is_whitespace() && !quoted => {
                if have {
                    out.push(core::mem::take(&mut cur));
                    have = false;
                }
            }
            c => {
                cur.push(c);
                have = true;
            }
        }
    }
    if quoted {
        return Err(String::from("a quote is not closed"));
    }
    if have {
        out.push(cur);
    }
    Ok(out)
}

pub fn find(name: &str) -> Option<&'static Command> {
    COMMANDS.entries().iter().find(|c| c.name == name)
}

/// Runs one line. Returns whether it succeeded and what it printed.
pub fn execute(line: &str, injected: bool, echo: bool) -> (bool, String) {
    let words = match split(line) {
        Ok(w) => w,
        Err(e) => {
            let mut c = Ctx { args: Vec::new(), injected, out: String::new(), echo };
            c.println(&format!("cannot read the line: {e}"));
            return (false, c.out);
        }
    };
    let Some((name, rest)) = words.split_first() else { return (true, String::new()) };
    let mut ctx = Ctx { args: rest.iter().map(String::as_str).collect(), injected, out: String::new(), echo };
    let Some(cmd) = find(name) else {
        ctx.println(&format!("{name}: no such command (help lists them)"));
        return (false, ctx.out);
    };
    if cmd.physical_only && injected {
        klog::event!(Warn, "console", "monitor.refused", command = cmd.name; "typed by injected input (MCP send_keys): physical keyboard only (R8c)");
        ctx.println(&format!("{}: refused: this line was typed by injected input (MCP); type it on the keyboard", cmd.name));
        return (false, ctx.out);
    }
    klog::event!(Debug, "console", "monitor.command", command = cmd.name, args = rest.len(), injected = injected);
    match (cmd.run)(&mut ctx) {
        Ok(()) => (true, ctx.out),
        Err(e) => {
            ctx.println(&format!("{}: {e}", cmd.name));
            (false, ctx.out)
        }
    }
}

pub const PROMPT: &str = "bits> ";
pub const PROMPT_ATTR: u8 = 10;

/// A log line arriving while Georg types moves the open line (prompt and half-typed text) below itself, as
/// readline does: the VT's text model carries it (`Screen::write_above`), so no flag is needed here.
fn prompt() {
    crate::vt::write(PROMPT, PROMPT_ATTR);
}

/// While a session holds the terminal (a shell or a program that made the console its controlling terminal),
/// the monitor leaves the input alone, as a getty does; it comes back with a fresh prompt when the session ends.
/// A program that changed the terminal settings and ended leaves the settings to the monitor (`sane_if_orphaned`).
fn wait_for_terminal(t: &crate::tty::Tty) -> bool {
    let p = crate::jobs::procs();
    let mut waited = false;
    loop {
        t.sane_if_orphaned(p);
        if !t.owned() && t.termios().l(crate::termios::flag::ICANON) {
            return waited;
        }
        waited = true;
        sched::sleep_ns(200_000_000);
    }
}

static RUNNING: AtomicBool = AtomicBool::new(false);
static AT_PROMPT: AtomicBool = AtomicBool::new(false);

/// The monitor thread runs (it reads the console whenever no session holds it).
pub fn running() -> bool {
    RUNNING.load(Ordering::SeqCst)
}

/// The monitor shows its prompt and reads typed lines now.
pub fn at_prompt() -> bool {
    AT_PROMPT.load(Ordering::SeqCst)
}

fn monitor_thread() {
    let t = crate::tty();
    RUNNING.store(true, Ordering::SeqCst);
    loop {
        AT_PROMPT.store(false, Ordering::SeqCst);
        wait_for_terminal(t);
        prompt();
        AT_PROMPT.store(true, Ordering::SeqCst);
        let mut seen = t.interrupts();
        let line = loop {
            match t.read_line_or_interrupt(1_000_000_000, seen) {
                crate::tty::LineWait::Line(l) => break Some(l),
                // Ctrl+C at the prompt dropped the line and echoed ^C: a fresh prompt on the next line.
                crate::tty::LineWait::Interrupted => {
                    seen = t.interrupts();
                    crate::vt::write("\n", crate::screen::FG_INDEX);
                    prompt();
                }
                crate::tty::LineWait::Timeout => {
                    if t.owned() || !t.termios().l(crate::termios::flag::ICANON) {
                        break None;
                    }
                }
            }
        };
        AT_PROMPT.store(false, Ordering::SeqCst);
        let Some(line) = line else {
            // Someone else has the terminal now: wait until it is free again.
            crate::vt::write("\n", crate::screen::FG_INDEX);
            continue;
        };
        if line.eof {
            crate::vt::write("\n", crate::screen::FG_INDEX);
            continue;
        }
        execute(&line.text, line.injected, true);
    }
}

pub(crate) fn start() -> Result<(), &'static str> {
    match sched::spawn("monitor", sched::Class::Normal, monitor_thread) {
        Ok(_) => Ok(()),
        Err(e) => {
            klog::event!(Error, "console", "monitor.spawn_failed", error = format!("{e:?}"));
            Err("monitor thread")
        }
    }
}

// ---- built-in commands

fn help(c: &mut Ctx) -> Result<(), String> {
    let mut all: Vec<&Command> = COMMANDS.entries().iter().collect();
    all.sort_by_key(|c| c.name);
    for cmd in all {
        let head = if cmd.args.is_empty() { cmd.name.to_string() } else { format!("{} {}", cmd.name, cmd.args) };
        c.println(&format!("  {head:<22} {}", cmd.help));
    }
    c.println("Keys: Shift+PageUp/PageDown scroll back, Ctrl+C stops a program.");
    Ok(())
}
crate::command!(name: "help", args: "", help: "this list", physical_only: false, run: help);

fn echo(c: &mut Ctx) -> Result<(), String> {
    let s = c.args.join(" ");
    c.println(&s);
    Ok(())
}
crate::command!(name: "echo", args: "[text]", help: "prints the text", physical_only: false, run: echo);

fn clear(c: &mut Ctx) -> Result<(), String> {
    if c.echo {
        crate::vt::clear();
    }
    Ok(())
}
crate::command!(name: "clear", args: "", help: "empties the screen", physical_only: false, run: clear);

fn number(c: &Ctx, i: usize, default: u64, max: u64) -> Result<u64, String> {
    match c.args.get(i) {
        None => Ok(default),
        Some(s) => s.parse::<u64>().ok().filter(|&n| n <= max).ok_or_else(|| format!("{s:?} is not a number up to {max}")),
    }
}

/// Lines `log <n>` shows at most (a bound on one request; the ring itself is sized from memory).
pub const LOG_LINES_MAX: u64 = 100_000;

/// `log [n]`: the newest n records of the kernel's ring (every level, whatever the console shows), oldest first.
/// `log level [off|error|warn|info|debug|all]`: shows or sets which log lines the console prints by itself.
fn log(c: &mut Ctx) -> Result<(), String> {
    if c.args.first() == Some(&"level") {
        if let Some(v) = c.args.get(1) {
            let n = crate::logfilter::parse(v).ok_or_else(|| format!("{v:?}: want {}", crate::logfilter::CHOICES))?;
            crate::set_log_level(n, if c.injected { "monitor_injected" } else { "monitor" });
        }
        c.println(&level_text(crate::log_level()));
        return Ok(());
    }
    if c.args.len() > 1 {
        return Err(String::from("want log [n] or log level [level]"));
    }
    let n = number(c, 0, 20, LOG_LINES_MAX)? as usize;
    let page = klog::read_tail(n, &klog::Filter::ALL);
    // One write for the whole page: one redraw instead of one per line.
    let mut text = String::new();
    for r in &page.records {
        text.push_str(&r.render());
        text.push('\n');
    }
    if page.missed() > 0 {
        let _ = writeln!(text, "[{} records were overwritten]", page.missed());
    }
    if page.busy {
        text.push_str("[the log was busy; shown what could be read]\n");
    }
    c.print(&text);
    Ok(())
}
crate::command!(name: "log", args: "[n] | level [level]", help: "the newest n log lines (20); log level: which lines the console shows", physical_only: false, run: log);

/// What `log level` answers.
pub fn level_text(level: u8) -> String {
    use crate::logfilter::*;
    let what = match level {
        OFF => "none",
        ERROR => "errors only",
        WARN => "errors, warnings and important events (boot, link, DHCP, watchdog, init, reload)",
        INFO => "everything down to info",
        DEBUG => "everything down to debug (CONFIG loglevel= decides what the kernel logs)",
        _ => "everything the kernel logs",
    };
    format!("log level {}: the console shows {what}; `log [n]` shows the newest n lines of the whole log", name(level))
}

fn mib(b: u64) -> u64 {
    b / (1 << 20)
}

fn mem(c: &mut Ctx) -> Result<(), String> {
    match mm_core::frames() {
        Some(f) => {
            let s = f.stats();
            c.println(&format!("frames: total {} MiB, free {} MiB, used {} MiB", mib(s.total_bytes), mib(s.free_bytes), mib(s.total_bytes - s.free_bytes)));
        }
        None => c.println("frames: no frame allocator"),
    }
    let h = mm_core::HEAP.stats();
    c.println(&format!(
        "heap: {} KiB asked for, {} KiB in slabs, {} KiB in {} large blocks",
        h.requested_bytes / 1024,
        h.slab_bytes / 1024,
        h.large_bytes / 1024,
        h.large_blocks
    ));
    Ok(())
}
crate::command!(name: "mem", args: "", help: "memory: frames and kernel heap", physical_only: false, run: mem);

fn cpus(c: &mut Ctx) -> Result<(), String> {
    let n = hal::percpu::cpu_count();
    c.println(&format!("cpus online={} scheduled={} this={}", n, sched::online_cpus(), hal::percpu::this_cpu()));
    for cpu in 0..n {
        let mut l = format!("cpu {cpu}: heartbeat {}", sched::heartbeat(cpu).unwrap_or(0));
        if let Some(i) = sched::last_idle_entry(cpu) {
            let _ = write!(l, ", {}", if i.idle_now { "idle" } else { "busy" });
        }
        c.println(&l);
    }
    c.println(&format!("threads: {}", sched::threads().len()));
    Ok(())
}
crate::command!(name: "cpus", args: "", help: "processors and their heartbeat", physical_only: false, run: cpus);

#[cfg(target_os = "none")]
fn ip(a: net::Ipv4) -> String {
    format!("{}.{}.{}.{}", a.0[0], a.0[1], a.0[2], a.0[3])
}

#[cfg(not(target_os = "none"))]
fn netinfo(c: &mut Ctx) -> Result<(), String> {
    c.println("network: no kernel network stack in a host build");
    Ok(())
}

#[cfg(target_os = "none")]
fn netinfo(c: &mut Ctx) -> Result<(), String> {
    let Some(s) = net::stack::kernel() else {
        c.println("network: the stack is not running");
        return Ok(());
    };
    let list = s.interfaces();
    if list.is_empty() {
        c.println("network: no card");
    }
    for i in list {
        let m = i.mac;
        c.println(&format!(
            "{} {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} link {}{} address {} router {}{}",
            i.name,
            m[0], m[1], m[2], m[3], m[4], m[5],
            if i.info.link_up { "up" } else { "down" },
            if i.info.link_up && i.info.speed_mbps > 0 { format!(" {} Mbit/s", i.info.speed_mbps) } else { String::new() },
            i.info.addr.map(|(a, p)| format!("{}/{p}", ip(a))).unwrap_or_else(|| String::from("-")),
            i.info.router.map(ip).unwrap_or_else(|| String::from("-")),
            if i.default_route { " (default route)" } else { "" }
        ));
    }
    Ok(())
}
crate::command!(name: "net", args: "", help: "network cards and addresses", physical_only: false, run: netinfo);

fn font(c: &mut Ctx) -> Result<(), String> {
    if let Some(name) = c.args.first() {
        let f = crate::font::Face::parse(name).ok_or_else(|| format!("{name:?}: want vga or noto"))?;
        crate::vt::set_face(f);
    }
    match crate::vt::font() {
        Some(f) => c.println(&format!("font {} {}x{} pixels per cell, {}x{} cells", f.face.name(), f.w, f.h, crate::vt::size().0, crate::vt::size().1)),
        None => c.println("font: no display"),
    }
    Ok(())
}
crate::command!(name: "font", args: "[vga|noto]", help: "shows or switches the console font", physical_only: false, run: font);

fn scrollback(c: &mut Ctx) -> Result<(), String> {
    if !c.args.is_empty() {
        // Bounded by memory in `set_scrollback` (an eighth of the free memory), not by a count (R1).
        let n = number(c, 0, 0, usize::MAX as u64)?;
        crate::vt::set_scrollback(n as usize)?;
    }
    let (limit, used) = crate::vt::scrollback();
    c.println(&format!("scrollback: {limit} rows kept, {used} in use"));
    Ok(())
}
crate::command!(name: "scrollback", args: "[rows]", help: "rows kept for Shift+PageUp", physical_only: false, run: scrollback);

fn colors(c: &mut Ctx) -> Result<(), String> {
    for (i, p) in PALETTE.iter().enumerate() {
        c.print(&format!("\x1b[{}m{:2} #{:06x}\x1b[0m  ", if i < 8 { 30 + i } else { 90 + i - 8 }, i, p));
        if i % 8 == 7 {
            c.print("\n");
        }
    }
    Ok(())
}
crate::command!(name: "colors", args: "", help: "the console's 16 colours", physical_only: false, run: colors);

fn reboot(c: &mut Ctx) -> Result<(), String> {
    c.println("Restarting: drivers stop, then the machine resets (FADT reset register, 0xCF9, keyboard controller, triple fault).");
    crate::reset::reboot()
}
crate::command!(name: "reboot", args: "", help: "restarts the machine (keyboard only)", physical_only: true, run: reboot);

fn poweroff(c: &mut Ctx) -> Result<(), String> {
    c.println("Switching off: drivers stop, then ACPI S5.");
    Err(crate::reset::poweroff())
}
crate::command!(name: "poweroff", args: "", help: "switches the machine off (keyboard only)", physical_only: true, run: poweroff);
#[cfg(not(target_os = "none"))]
use net as _;
