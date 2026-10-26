//! `/dev/console`, `/dev/tty` and `/dev/tty0` (FS-2) on the kernel console. The console itself (screen,
//! keyboard, line discipline) belongs to IN (`crates/console`, IN-3), which sits above `vfs`; it plugs in here
//! through `ConsoleBackend` and `set_console`.
//!
//! Until IN's VT does, a node uses the `File` a `devnode!` of the same path registered (today `console`'s
//! G0 `ConsoleDev`: writes to the screen, serial and the capture buffer, reads at end of file); `devfs` hands it
//! over as the node's fallback, so program output looks the same at every merge order. Without either, writes go
//! to the kernel log (visible on screen and in the mirror) and reads are end of file.
//!
//! Linux differences, on purpose: `/dev/tty` is the console for every program (Linux: the controlling
//! terminal, ENXIO without one; BitsOS has one terminal and no sessions yet). Blocking reads wait in bounded
//! slices and end with EINTR once the program is killed (B4: `abort` below).

use crate::node::File;
use crate::poll::{Events, PollQueue};
use crate::Errno;
use alloc::sync::Arc;
use alloc::vec::Vec;
use ksync::{OnceRef, WaitQueue};
use uapi::errno::*;

/// What the console offers to the device nodes (implemented by IN's VT).
pub trait ConsoleBackend: Sync {
    /// Output of a program, as written (the backend draws it and keeps it for `console_output`). Should not
    /// wait for anything but the screen; a full backend may return a short count.
    fn write(&self, buf: &[u8]) -> Result<usize, Errno>;
    /// Input after the line discipline. Waits until a line (canonical) or a byte (raw) is there; EAGAIN when
    /// `nonblock` and nothing is there; EINTR when `abort()` says the reader is being ended (to be asked between
    /// wait slices of at most 100 ms) or on Ctrl+C; Ok(0) at end of file (Ctrl+D on an empty line).
    fn read(&self, buf: &mut [u8], nonblock: bool, abort: &dyn Fn() -> bool) -> Result<usize, Errno>;
    /// Whether a read would return now (poll's IN).
    fn has_input(&self) -> bool;
    /// Terminal ioctls (TIOCGWINSZ, TCGETS, ...; IN-4's termios). The answer is copied to `arg` by the caller.
    fn ioctl(&self, _cmd: u64, _arg: u64) -> Result<Vec<u8>, Errno> {
        Err(ENOTTY)
    }
    /// Woken when input arrives (FS-4's poll/epoll).
    fn wait_queue(&self) -> Option<&WaitQueue> {
        None
    }
    /// IN-4: `read` knowing the node (job control: every node but `/dev/console`, Linux's `redirected_tty_write`).
    fn read_on(&self, _which: Which, buf: &mut [u8], nonblock: bool, abort: &dyn Fn() -> bool) -> Result<usize, Errno> {
        self.read(buf, nonblock, abort)
    }
    /// IN-4: `write` knowing the node and O_NONBLOCK (TOSTOP's job control, output stopped by ^S).
    fn write_on(&self, _which: Which, buf: &[u8], _nonblock: bool, _abort: &dyn Fn() -> bool) -> Result<usize, Errno> {
        self.write(buf)
    }
    /// IN-4: the terminal ioctls with the caller's memory (`File::ioctl_user`).
    fn ioctl_user(&self, _cmd: u64, _arg: u64, _mem: &dyn crate::sys::UserMem) -> Option<Result<i64, Errno>> {
        None
    }
    /// FS-4: the same queue as a `PollQueue` (woken when input arrives, a line ends or the terminal hangs up),
    /// so poll/select/epoll on the terminal wake at once. None: they look again every few milliseconds.
    fn poll_queue(&self) -> Option<&PollQueue> {
        None
    }
    /// IN-7 CR1: `/dev/tty` of a caller whose controlling terminal is not the console (a pty): that terminal's
    /// own open file (`tty_open_current_tty`). None: the console as before.
    fn open_ctty(&self, _flags: u64) -> Option<Result<Arc<dyn File>, Errno>> {
        None
    }
}

static CONSOLE: OnceRef<dyn ConsoleBackend> = OnceRef::new();

/// The console registers itself once (IN: in `console::init`). A second call is refused (logged).
pub fn set_console(b: &'static dyn ConsoleBackend) -> bool {
    let ok = CONSOLE.set(b);
    if ok {
        klog::event!(Info, "vfs", "tty.backend"; "/dev/console, /dev/tty and /dev/tty0 are on the kernel console");
    } else {
        klog::event!(Warn, "vfs", "tty.backend_refused"; "a console backend is set already; this one is not used");
    }
    ok
}

pub fn console() -> Option<&'static dyn ConsoleBackend> {
    CONSOLE.get()
}

/// Which node (they differ only in number and permissions).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Which {
    /// `/dev/console` 5:1.
    Console,
    /// `/dev/tty` 5:0.
    Tty,
    /// `/dev/tty0` 4:0 (the current virtual terminal).
    Tty0,
}

impl Which {
    pub fn path(self) -> &'static str {
        match self {
            Which::Console => "/dev/console",
            Which::Tty => "/dev/tty",
            Which::Tty0 => "/dev/tty0",
        }
    }
}

pub struct TtyDev {
    pub which: Which,
    /// A `devnode!` of the same path (IN's G0 console device) used while no backend is set.
    fallback: Option<Arc<dyn File>>,
}

impl TtyDev {
    pub fn new(which: Which, fallback: Option<Arc<dyn File>>) -> Self {
        TtyDev { which, fallback }
    }
    pub fn has_fallback(&self) -> bool {
        self.fallback.is_some()
    }
}

/// Output without any console: one log line per written line (bounded, no allocation per byte).
fn log_output(buf: &[u8]) {
    for line in buf.split(|&b| b == b'\n').filter(|l| !l.is_empty()) {
        for piece in line.chunks(240) {
            klog::event!(Info, "tty", "out", text = &*alloc::string::String::from_utf8_lossy(piece));
        }
    }
}

impl File for TtyDev {
    fn read_at(&self, _pos: u64, buf: &mut [u8]) -> Result<usize, Errno> {
        self.read_stream(buf, false)
    }
    fn write_at(&self, _pos: u64, buf: &[u8]) -> Result<usize, Errno> {
        self.write_stream(buf, false)
    }
    fn read_stream(&self, buf: &mut [u8], nonblock: bool) -> Result<usize, Errno> {
        if let Some(c) = console() {
            return c.read_on(self.which, buf, nonblock, &crate::dentry::interrupted);
        }
        match &self.fallback {
            Some(f) => f.read_stream(buf, nonblock),
            None => Ok(0),
        }
    }
    fn write_stream(&self, buf: &[u8], nonblock: bool) -> Result<usize, Errno> {
        if let Some(c) = console() {
            return c.write_on(self.which, buf, nonblock, &crate::dentry::interrupted);
        }
        match &self.fallback {
            Some(f) => f.write_stream(buf, nonblock),
            None => {
                log_output(buf);
                Ok(buf.len())
            }
        }
    }
    fn mode(&self) -> u32 {
        match self.which {
            Which::Console | Which::Tty0 => uapi::S_IFCHR | 0o620,
            Which::Tty => uapi::S_IFCHR | 0o666,
        }
    }
    fn rdev(&self) -> u64 {
        match self.which {
            Which::Console => crate::abi::encode_dev(5, 1),
            Which::Tty => crate::abi::encode_dev(5, 0),
            Which::Tty0 => crate::abi::encode_dev(4, 0),
        }
    }
    fn ioctl(&self, cmd: u64, arg: u64) -> Result<Vec<u8>, Errno> {
        if let Some(c) = console() {
            return c.ioctl(cmd, arg);
        }
        match &self.fallback {
            Some(f) => f.ioctl(cmd, arg),
            None => Err(ENOTTY),
        }
    }
    fn ioctl_user(&self, cmd: u64, arg: u64, mem: &dyn crate::sys::UserMem) -> Option<Result<i64, Errno>> {
        console()?.ioctl_user(cmd, arg, mem)
    }
    fn open_node(&self, flags: u64, _who: crate::inode::Cred) -> Option<Result<Arc<dyn File>, Errno>> {
        if self.which != Which::Tty {
            return None;
        }
        console()?.open_ctty(flags)
    }
    fn poll_head(&self) -> Option<&PollQueue> {
        if let Some(c) = console() {
            return c.poll_queue();
        }
        self.fallback.as_ref().and_then(|f| f.poll_head())
    }
    fn poll_wait(&self, add: &mut dyn FnMut(&PollQueue)) {
        if let Some(c) = console() {
            if let Some(q) = c.poll_queue() {
                add(q);
            }
            return;
        }
        if let Some(f) = &self.fallback {
            f.poll_wait(add);
        }
    }
    fn events(&self) -> Events {
        if let Some(c) = console() {
            return Events(Events::OUT | if c.has_input() { Events::IN } else { 0 });
        }
        match &self.fallback {
            Some(f) => f.events(),
            // Reads end at once (end of file): readable, as a hung-up terminal.
            None => Events(Events::OUT | Events::IN),
        }
    }
}
