//! `/dev/bitsos/gpu-messung`: the measurement run asked for by a program, `/bin/gpu-messung` at busybox's shell.
//! A write of `start` asks; the run (its question first) goes on a thread of its own, as the monitor's command
//! runs on the monitor's thread, so the question and the run see the same kernel as from `bits>`. A read waits
//! until the start ended and hands out one line: the exit code the program gives (0: the run went; 1: nothing
//! was done) and the plain German line it prints. The screen meanwhile is the kernel's (`vt::write`): the program
//! prints nothing before that line.
//!
//! The node does not decide whether the card is touched: that is Georg's J at a real keyboard (`super::start`).
//! It is root's (0600, checked at open), and a process MCP's run started is refused at open, as the monitor
//! refuses a line with an injected key: MCP cannot even put the question up and take the console's keys.

use super::{End, Hold, Machine, Rx9070, started};
use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};
use uapi::errno::*;
use vfs::{Cred, Errno, File};

pub const PATH: &str = "/dev/bitsos/gpu-messung";

/// Opens refused (not root, or MCP's run): the ktest's witness.
pub static OPEN_REFUSED: AtomicU64 = AtomicU64::new(0);

/// How often a read looks whether the start ended.
const SLICE_NS: u64 = 50_000_000;

/// The answer's line for an ended start.
pub fn answer(end: &End) -> String {
    format!("{} {}\n", if end.ran { 0 } else { 1 }, end.line)
}

enum Got {
    Idle,
    Waiting,
    /// The answer, and how much of it was read.
    Done(String, usize),
}

/// One open file: at most one start.
pub struct Request {
    make: fn() -> Box<dyn Machine>,
    got: Arc<ksync::Mutex<Got>>,
}

impl Request {
    /// A request on `make`'s machine (the node's is the card; the ktests give their own).
    pub fn new(make: fn() -> Box<dyn Machine>) -> Request {
        Request { make, got: Arc::new(ksync::Mutex::named("gx.messung.request", Got::Idle)) }
    }

    fn ask(&self, buf: &[u8]) -> Result<usize, Errno> {
        if core::str::from_utf8(buf).map(str::trim) != Ok("start") {
            return Err(EINVAL);
        }
        let mut g = self.got.lock();
        if !matches!(*g, Got::Idle) {
            return Err(EBUSY);
        }
        klog::event!(Info, "gx", "messung.request", via = "shell"; "/bin/gpu-messung asks for the measurement run");
        let Some(hold) = Hold::take() else {
            klog::event!(Info, "gx", "messung.busy", via = "shell"; "a second start while one stands: refused");
            *g = Got::Done(answer(&End { ran: false, line: String::from(super::BUSY) }), 0);
            return Ok(buf.len());
        };
        *g = Got::Waiting;
        drop(g);
        let (make, got) = (self.make, self.got.clone());
        let spawned = sched::spawn("gpu-messung", sched::Class::Normal, move || {
            let end = started(hold, &mut *make(), "shell");
            *got.lock() = Got::Done(answer(&end), 0);
        });
        if let Err(e) = spawned {
            // The closure, and with it the hold, is dropped: nothing stands.
            let line = format!("Die Messung ließ sich nicht starten (kein Faden: {e:?}). Es wurde nichts getan.");
            *self.got.lock() = Got::Done(answer(&End { ran: false, line }), 0);
        }
        Ok(buf.len())
    }

    fn read(&self, buf: &mut [u8], nonblock: bool) -> Result<usize, Errno> {
        loop {
            {
                let mut g = self.got.lock();
                match &mut *g {
                    Got::Idle => return Err(EINVAL),
                    Got::Waiting => {}
                    Got::Done(text, at) => {
                        let rest = &text.as_bytes()[*at..];
                        let n = rest.len().min(buf.len());
                        buf[..n].copy_from_slice(&rest[..n]);
                        *at += n;
                        return Ok(n);
                    }
                }
            }
            if nonblock {
                return Err(EAGAIN);
            }
            if vfs::dentry::interrupted() {
                return Err(EINTR);
            }
            sched::sleep_ns(SLICE_NS);
        }
    }
}

impl File for Request {
    fn read_at(&self, _pos: u64, buf: &mut [u8]) -> Result<usize, Errno> {
        self.read(buf, false)
    }
    fn read_stream(&self, buf: &mut [u8], nonblock: bool) -> Result<usize, Errno> {
        self.read(buf, nonblock)
    }
    fn write_at(&self, _pos: u64, buf: &[u8]) -> Result<usize, Errno> {
        self.ask(buf)
    }
    fn mode(&self) -> u32 {
        uapi::S_IFCHR | 0o600
    }
    fn rdev(&self) -> u64 {
        rdev_number()
    }
}

fn rdev_number() -> u64 {
    vfs::abi::encode_dev(10, 233)
}

fn card() -> Box<dyn Machine> {
    Box::new(Rx9070::default())
}

struct Node;

impl File for Node {
    fn read_at(&self, _pos: u64, _buf: &mut [u8]) -> Result<usize, Errno> {
        Err(EINVAL)
    }
    fn write_at(&self, _pos: u64, _buf: &[u8]) -> Result<usize, Errno> {
        Err(EINVAL)
    }
    fn mode(&self) -> u32 {
        uapi::S_IFCHR | 0o600
    }
    fn rdev(&self) -> u64 {
        rdev_number()
    }
    fn open_node(&self, _flags: u64, who: Cred) -> Option<Result<Arc<dyn File>, Errno>> {
        // MCP_PROGRAM is inherited over fork and exec: a shell MCP's run started is refused too.
        let mcp = proc::current().is_some_and(|p| p.program == proc::MCP_PROGRAM);
        if who.uid != 0 || mcp {
            OPEN_REFUSED.fetch_add(1, Ordering::Relaxed);
            klog::event!(Warn, "gx", "messung.open_refused", uid = who.uid, mcp = mcp; "the measurement run is asked for at the machine, never over MCP");
            return Some(Err(if mcp { EPERM } else { EACCES }));
        }
        Some(Ok(Arc::new(Request::new(card))))
    }
}

fn make() -> Arc<dyn File> {
    Arc::new(Node)
}
vfs::devnode!("/dev/bitsos/gpu-messung", make);
