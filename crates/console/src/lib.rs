//! Kernel console and input (owner: IN; contracts `C: Console`, `C: EvdevSink`, V2-PLAN §2.8, §9.1).
//!
//! IN-3: the kernel VT. A text model (`screen`: cells, cursor, scrollback, VT sequences) drawn into the display's
//! RAM frame (`render`) with a font sized from the mode (`font`: the VGA 8x16 font scaled, or Noto Sans Mono with
//! grey levels), the German keymap (`keymap`), key repeat by a timer (`repeat`), the mouse pointer over the text,
//! the line discipline programs read through `/dev/console` (`tty`), the built-in monitor command line with a
//! command registry (`monitor`, `command!`). The console is also a deferred klog sink: log lines appear once the early console handed over, filtered
//! by the console's own level (`logfilter`, IN-quiet: default warn = Warn and above plus a short list of important
//! Info events; CONFIG `console_log=`, monitor `log level`); a log line arriving while someone types goes above
//! the typed line (`Screen::write_above`).
//!
//! Locks: `console.vt` (the model, the font and the pointer) is taken before `display.frame` and holds nothing
//! else; it is never held while logging or waking, and the panic path (`write_emergency`) draws straight into
//! the frame without touching the model.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod evdev;
pub mod evdevfs;
pub mod evrepeat;
pub mod font;
pub mod jobs;
pub mod ktest_lease;
pub mod keymap;
pub mod latency;
pub mod ldisc;
pub mod logfilter;
pub mod monitor;
pub mod pointer_over;
pub mod ptm;
pub mod pts;
pub mod pty;
pub mod rawkeys;
pub mod render;
pub mod repeat;
pub mod reset;
pub mod screen;
pub mod termios;
pub mod tty;
pub mod ttyioctl;
pub mod vtioctl;
pub mod utf8;
pub mod vt;

mod ktests;
#[doc(hidden)]
pub mod ktests_in4;
mod ktests_pty;
mod ktests_in6;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_in4;
#[cfg(test)]
mod tests_in4b;
#[cfg(test)]
mod tests_in6;
#[cfg(test)]
mod tests_pty;
#[cfg(test)]
mod tests_quiet;
#[cfg(test)]
mod tests_vt;

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU8, Ordering};
use ksync::SpinLock;
use vfs::{Errno, File};

pub const FG: u32 = screen::PALETTE[screen::FG_INDEX as usize];
pub const BG: u32 = screen::PALETTE[screen::BG_INDEX as usize];
pub const FG_WARN: u32 = screen::PALETTE[3];
pub const FG_ERR: u32 = screen::PALETTE[1];
/// The monitor's prompt colour.
pub const FG_PROMPT: u32 = screen::PALETTE[10];

/// `C: Console`: a text output the kernel and programs write to.
pub trait Console: Send + Sync {
    fn write_str(&self, s: &str, color: u32);
    /// Columns and rows of the text grid (0, 0 before the first write found a display).
    fn size(&self) -> (u32, u32);
}

/// The kernel's console (the VT on the display's RAM frame).
pub struct TextConsole;

impl Console for TextConsole {
    fn write_str(&self, s: &str, color: u32) {
        vt::write(s, screen::index_of(color));
    }
    fn size(&self) -> (u32, u32) {
        vt::size()
    }
}

static CONSOLE: TextConsole = TextConsole;

pub fn console() -> &'static TextConsole {
    &CONSOLE
}

/// Columns, rows, the screen's lines and the cursor cell.
pub type TextGrid = (u32, u32, Vec<String>, (u32, u32));

/// The console's text grid for MCP `screen` text mode (change request 3 of MC-3): columns, rows, the live
/// screen's lines (trailing blanks trimmed) and the cursor cell. None before the console has a display.
pub fn text_grid() -> Option<TextGrid> {
    vt::text_grid()
}

/// The console's log level (`logfilter`: 0 off .. 5 all; default warn = Warn and above plus the important Info
/// events). Only the screen is filtered: ring, mirror and the MCP `log` tool keep everything.
static LOG_LEVEL: AtomicU8 = AtomicU8::new(logfilter::DEFAULT);

/// Sets the console's log level (`logfilter::OFF..=ALL`); `source` names who asked (event `console log.level`).
pub fn set_log_level(n: u8, source: &'static str) {
    let n = n.min(logfilter::ALL);
    let old = LOG_LEVEL.swap(n, Ordering::AcqRel);
    if old != n {
        klog::event!(Info, "console", "log.level", level = logfilter::name(n), was = logfilter::name(old), source = source);
    }
}
pub fn log_level() -> u8 {
    LOG_LEVEL.load(Ordering::Acquire)
}

struct LogSink;
impl klog::Sink for LogSink {
    fn write_line(&self, level: klog::Level, line: &str) {
        if logfilter::shows(log_level(), level, line) {
            vt::write_line(line, screen::index_of(level_color(level)));
        }
    }
    /// Panic/NMI path: draws only if neither the VT nor the frame is busy right now, never waits or allocates.
    fn write_emergency(&self, level: klog::Level, line: &str) {
        vt::write_emergency(line, screen::index_of(level_color(level)));
    }
}
static LOG_SINK: LogSink = LogSink;

fn level_color(level: klog::Level) -> u32 {
    match level {
        klog::Level::Error => FG_ERR,
        klog::Level::Warn => FG_WARN,
        _ => FG,
    }
}

/// Kernel init after the display exists. The log sink is attached by `configure` (right after this call), so the
/// boot lines from before the console are replayed at the level CONFIG chose; `vt::start` attaches it too if no
/// one called `configure`. The keyboard, the pointer and the monitor start at the Service init level (`vt::start`).
pub fn init() {
    // FS-2 change request 1 (INT-A1): FS's /dev/console, /dev/tty and /dev/tty0 reach the VT through this backend.
    vfs::tty::set_console(&CONSOLE_DEV);
    // IN-4: MCP's injected keyboard and pointer get their /dev/input/eventN like real devices.
    evdevfs::register_mcp_devices();
    // DP-2 CR 2a (INT-A2a, answers IN-4 CR6): display tells the VT when a program takes or gives back the scanout
    // (only from `display::sync_vt`, never under a display or fbdev lock).
    display::set_vt_hooks(&VT_HOOKS);
}

static VT_HOOKS: display::VtHooks = display::VtHooks { hand_over: vt::hand_over, take_back: vt::take_back };

static LOG_ATTACHED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Log lines appear on screen from now on (once). Deferred (RB-1 change request 5): klogd draws, so an emitter (IRQ
/// handler, KernelRt thread, a holder of the klog ring) never draws or scrolls the screen itself; `replay` puts the
/// boot lines from before the console on screen (filtered like every other line).
pub(crate) fn attach_log() {
    if !LOG_ATTACHED.swap(true, Ordering::AcqRel) {
        klog::add_deferred_sink("console", &LOG_SINK, true);
    }
}

/// CONFIG keys of the console (the kernel calls this right after `init`): `console_font=vga|noto`,
/// `console_scrollback=<rows>`, `console_log=off|error|warn|info|debug|all` (default warn; IN-3's name
/// `console_loglevel` is still read when `console_log` is not set). Attaches the log sink at the end.
pub fn configure(get: &dyn Fn(&str) -> Option<String>) {
    configure_keys(get);
    attach_log();
}

fn configure_keys(get: &dyn Fn(&str) -> Option<String>) {
    if let Some(v) = get("console_font") {
        match font::Face::parse(v.trim()) {
            Some(f) => vt::set_face(f),
            None => klog::event!(Warn, "console", "config.bad", key = "console_font", value = v.as_str(); "want vga or noto"),
        }
    }
    if let Some(v) = get("console_scrollback") {
        match v.trim().parse::<usize>() {
            Ok(n) => {
                if let Err(e) = vt::set_scrollback(n) {
                    klog::event!(Warn, "console", "config.bad", key = "console_scrollback", value = v.as_str(), reason = e.as_str());
                }
            }
            Err(_) => klog::event!(Warn, "console", "config.bad", key = "console_scrollback", value = v.as_str(); "want a number of rows"),
        }
    }
    let log = get("console_log").map(|v| ("console_log", v)).or_else(|| get("console_loglevel").map(|v| ("console_loglevel", v)));
    if let Some((key, v)) = log {
        match logfilter::parse(&v) {
            Some(n) => set_log_level(n, "config"),
            None => klog::event!(Warn, "console", "config.bad", key = key, value = v.as_str(); "want {}; keeping {}", logfilter::CHOICES, logfilter::name(log_level())),
        }
    }
}

/// Everything programs write to the console, also kept for tests and MCP (`console_output`). A ring: once full,
/// a write drops as many old bytes as it adds (no copy of the whole buffer under the lock).
static CAPTURE: SpinLock<VecDeque<u8>> = SpinLock::named("console.capture", VecDeque::new());
const CAPTURE_MAX: usize = 1 << 20;

/// What programs wrote to the console since boot (last MiB).
pub fn captured() -> Vec<u8> {
    CAPTURE.lock().iter().copied().collect()
}

/// Appends `buf` to a capture ring of at most `max` bytes (only the newest `max` bytes of `buf` are copied).
pub fn capture_into(ring: &mut VecDeque<u8>, buf: &[u8], max: usize) {
    let buf = &buf[buf.len().saturating_sub(max)..];
    let over = (ring.len() + buf.len()).saturating_sub(max);
    ring.drain(..over.min(ring.len()));
    ring.extend(buf.iter().copied());
}

/// The line discipline behind `/dev/console`, `/dev/tty` and the monitor.
pub fn tty() -> &'static tty::Tty {
    static TTY: tty::Tty = tty::Tty::new();
    &TTY
}

/// Asked by a blocked console read whether its reader is being ended (installed by the crate that runs programs,
/// `console-cmds`: "the current process was killed"). Without it only `abort` of the caller ends a blocked read.
static READ_ABORT: ksync::Once<fn() -> bool> = ksync::Once::new();

pub fn set_read_abort(f: fn() -> bool) {
    READ_ABORT.get_or_init(|| f);
}

/// The caller's program is being ended (the hook above; false without it).
pub(crate) fn program_ended() -> bool {
    READ_ABORT.get().is_some_and(|f| f())
}

// IN-7: `/dev/ptmx` (5:2, 0666) and `/dev/pts/ptmx` (mode 0 as CachyOS's `ptmxmode=000`; this registration also
// makes the `/dev/pts` directory the slave nodes are made in).
vfs::devnode!("/dev/ptmx", pty::make_ptmx);
vfs::devnode!("/dev/pts/ptmx", pty::make_pts_ptmx);

/// The console behind `/dev/console`, `/dev/tty`, `/dev/tty0` (FS-2's nodes, `vfs::tty::ConsoleBackend`): writes go
/// through the terminal's output processing to the screen (serial and the capture get the bytes as written),
/// reads come from the keyboard through the line discipline with the termios (IN-4). Also a plain `File` for
/// callers that want the console without a path.
pub struct ConsoleDev;
static CONSOLE_DEV: ConsoleDev = ConsoleDev;

/// How much program output one step of `write_on` processes.
pub(crate) const WRITE_PIECE: usize = 4096;

impl ConsoleDev {
    /// A program's write. `job`: the node does job control (all but `/dev/console`). In pieces of `WRITE_PIECE`
    /// bytes (the terminal's lock is held only for one piece's output processing, as Linux's write room); a write
    /// stopped part way (^S with O_NONBLOCK, the program killed) answers what went out.
    pub fn write_on(buf: &[u8], nonblock: bool, abort: &dyn Fn() -> bool, job: bool) -> Result<usize, Errno> {
        let abort = || abort() || READ_ABORT.get().is_some_and(|f| f());
        let mut done = 0;
        for piece in buf.chunks(WRITE_PIECE) {
            let screen = match tty().write(piece, nonblock, &abort, job && done == 0) {
                Ok(s) => s,
                Err(e) if done == 0 => return Err(e),
                Err(_) => break,
            };
            capture_into(&mut CAPTURE.lock(), piece, CAPTURE_MAX);
            hal::serial::write_bytes(piece);
            vt::write_program(&screen);
            // Linux's `respond_string`: a report goes into the terminal's input as if typed.
            let reply = vt::take_reply();
            if !reply.is_empty() {
                tty().input(&reply, false, &mut |e| vt::write_program(e));
            }
            done += piece.len();
        }
        Ok(done)
    }
    /// A program's read (`n_tty_read`).
    pub fn read_on(buf: &mut [u8], nonblock: bool, abort: &dyn Fn() -> bool, job: bool) -> Result<usize, Errno> {
        let abort = || abort() || READ_ABORT.get().is_some_and(|f| f());
        tty().read(buf, nonblock, &abort, job)
    }
    /// A terminal ioctl with the caller's memory (the integration patch's `ioctl_user`): Linux's return value.
    pub fn ioctl_user(cmd: u64, arg: u64, mem: &dyn vfs::sys::UserMem) -> Result<i64, Errno> {
        ttyioctl::ioctl(tty(), cmd, arg, mem, jobs::procs())
    }
    /// The same through the `File::ioctl` of today's VFS: the argument is read through the process hook, the
    /// answer collected for the syscall layer to copy (terminal ioctls all return 0).
    fn ioctl_vec(cmd: u64, arg: u64) -> Result<Vec<u8>, Errno> {
        let m = RecordMem::new(arg, TTY_RECORD_LIMIT);
        Self::ioctl_user(cmd, arg, &m)?;
        Ok(m.take())
    }
    fn poll_events() -> vfs::Events {
        let mut e = 0;
        if !tty().output_stopped() {
            e |= vfs::Events::OUT;
        }
        if tty().has_input() {
            e |= vfs::Events::IN;
        }
        vfs::Events(e)
    }
}

/// User memory for an ioctl that came through `File::ioctl(cmd, arg) -> bytes`: reads go to the calling process
/// (`jobs::Procs::read_user`). Writes inside the answer's own struct (`arg` up to `limit` bytes) are collected and
/// returned, since the syscall layer copies the returned bytes to `arg`; writes anywhere else (EVIOCGMASK's
/// buffer behind a pointer in its struct) go straight to the caller (`Procs::write_user`), so nothing lands at
/// the wrong address.
pub struct RecordMem {
    base: u64,
    limit: u64,
    out: core::cell::RefCell<Vec<u8>>,
}

/// The largest struct a terminal ioctl writes at its argument (`struct termios2`, 44 bytes, rounded up).
pub const TTY_RECORD_LIMIT: u64 = 64;

impl RecordMem {
    pub fn new(base: u64, limit: u64) -> RecordMem {
        RecordMem { base, limit, out: core::cell::RefCell::new(Vec::new()) }
    }
    /// For an `_IOC`-encoded command: its size field is the struct at `arg`.
    pub fn for_ioc(base: u64, cmd: u64) -> RecordMem {
        RecordMem::new(base, (cmd >> 16) & 0x3FFF)
    }
    pub fn take(&self) -> Vec<u8> {
        core::mem::take(&mut *self.out.borrow_mut())
    }
}

impl vfs::sys::UserMem for RecordMem {
    fn read(&self, addr: u64, buf: &mut [u8]) -> bool {
        jobs::procs().read_user(addr, buf)
    }
    fn write(&self, addr: u64, data: &[u8]) -> bool {
        // An end past the address space is a fault wherever it is (copy_to_user's access_ok).
        if addr.checked_add(data.len() as u64).is_none() {
            return false;
        }
        let inside = addr.checked_sub(self.base).and_then(|off| Some((off, off.checked_add(data.len() as u64)?)));
        let Some((off, end)) = inside.filter(|&(_, end)| end <= self.limit) else {
            return jobs::procs().write_user(addr, data);
        };
        let (off, end) = (off as usize, end as usize);
        let mut o = self.out.borrow_mut();
        if o.len() < end {
            o.resize(end, 0);
        }
        o[off..end].copy_from_slice(data);
        true
    }
    fn read_cstr(&self, _addr: u64, _max: usize) -> Result<Vec<u8>, Errno> {
        Err(uapi::errno::EFAULT)
    }
}

impl vfs::tty::ConsoleBackend for ConsoleDev {
    fn write(&self, buf: &[u8]) -> Result<usize, Errno> {
        // Without the node (today's backend interface): no job control, as on /dev/console.
        Self::write_on(buf, false, &vfs::dentry::interrupted, false)
    }
    fn read(&self, buf: &mut [u8], nonblock: bool, abort: &dyn Fn() -> bool) -> Result<usize, Errno> {
        Self::read_on(buf, nonblock, abort, false)
    }
    fn has_input(&self) -> bool {
        tty().has_input()
    }
    fn ioctl(&self, cmd: u64, arg: u64) -> Result<Vec<u8>, Errno> {
        Self::ioctl_vec(cmd, arg)
    }
    fn wait_queue(&self) -> Option<&ksync::WaitQueue> {
        Some(tty().wait_queue())
    }
    fn poll_queue(&self) -> Option<&vfs::poll::PollQueue> {
        Some(tty().poll_queue())
    }
    fn read_on(&self, which: vfs::tty::Which, buf: &mut [u8], nonblock: bool, abort: &dyn Fn() -> bool) -> Result<usize, Errno> {
        Self::read_on(buf, nonblock, abort, which != vfs::tty::Which::Console)
    }
    fn write_on(&self, which: vfs::tty::Which, buf: &[u8], nonblock: bool, abort: &dyn Fn() -> bool) -> Result<usize, Errno> {
        Self::write_on(buf, nonblock, abort, which != vfs::tty::Which::Console)
    }
    fn ioctl_user(&self, cmd: u64, arg: u64, mem: &dyn vfs::sys::UserMem) -> Option<Result<i64, Errno>> {
        Some(Self::ioctl_user(cmd, arg, mem))
    }
    /// IN-7: a session whose controlling terminal is a pty gets that pty for `/dev/tty`.
    fn open_ctty(&self, flags: u64) -> Option<Result<alloc::sync::Arc<dyn File>, Errno>> {
        pty::open_ctty(flags)
    }
}

impl File for ConsoleDev {
    fn read_at(&self, _pos: u64, buf: &mut [u8]) -> Result<usize, Errno> {
        Self::read_on(buf, false, &|| false, false)
    }
    fn write_at(&self, _pos: u64, buf: &[u8]) -> Result<usize, Errno> {
        Self::write_on(buf, false, &|| false, false)
    }
    fn mode(&self) -> u32 {
        uapi::S_IFCHR | 0o620
    }
    fn events(&self) -> vfs::Events {
        Self::poll_events()
    }
    fn ioctl(&self, cmd: u64, arg: u64) -> Result<Vec<u8>, Errno> {
        Self::ioctl_vec(cmd, arg)
    }
}

#[doc(hidden)]
pub use kreg as __kreg;
