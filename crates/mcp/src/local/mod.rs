//! MC-L: the local tools socket `/run/bitsos/tools.sock`: a second, narrow door to the MCP tools for a program
//! on this machine (the assistant, V2-PLAN §8a.3). JSON-RPC 2.0, one message per line both ways, the methods
//! of the network side (`initialize`, `ping`, `tools/list`, `tools/call`; `mcp_wire::rpc` builds the answers).
//!
//! - Who: the kernel decides at accept (`door`): SO_PEERCRED and the peer process's executable. Anyone else
//!   gets one refusal line and the connection is closed; every refusal is an event (`local.refused`).
//! - What: only tools marked `local` (`ToolEntry::local`; a service's tool when a service of uid 0 marked it).
//!   `tools/list` shows nothing else, and a call of anything else is "Unknown tool", as if it did not exist.
//!   `output` and `stop` are not local, so a call cannot be left running behind its caller: at its deadline
//!   the caller gets an error, the tool is asked to stop and its late answer is dropped.
//! - The network side is untouched: this door has its own threads (accept, watch, one reader per connection),
//!   its own workers and call table (`Shared::headless`), and takes none of the lifeline's connection places
//!   (B4). A client that sends garbage, never ends a line or never reads costs its own connection only: lines
//!   are capped (`LINE_MAX`), answers must be taken within `WRITE_STALL_NS`, bad messages in a row close it.
//! - A call waits for its tool at most `CALL_MAX_NS`, and not at all once the program hung up. A tool that did
//!   not end at its deadline is not called again from here until it did (no thread piles up behind it).
//! - Every call is an event (`local.call`: pid, uid, tool, outcome; a tool of class change or delete_overwrite
//!   `local.change`, a call site of its own so a flood of read calls cannot push it out of the log). A stranger
//!   at the door is an Error event (`local.refused`), at most one a second, so it shows at the log level
//!   "errors only" too; ordinary calls show from level info on, and always in the counters `mcp.local.*`.

pub mod door;
mod ktests;
mod serve;

use crate::server::{Env, Shared, SpawnFailed, ThreadKind};
use crate::svc::link::{Link, Listen};
use crate::{ServerConfig, ToolEntry};
use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
pub use door::{Door, Pass};
use ksync::{SpinLock, WaitQueue};

pub use crate::svc::{By, Peer};

/// Where local programs ask (V2-PLAN §8a.3).
pub const SOCKET_PATH: &str = "/run/bitsos/tools.sock";
/// Longest request line (a call's arguments; the network side's bodies are far below this too). A longer one
/// ends the connection: the stream cannot be trusted past it.
pub const LINE_MAX: usize = 256 * 1024;
/// Bad messages in a row after which a connection is closed (it is not speaking the protocol).
pub const MAX_ERRORS: usize = 16;
/// Connections one process may hold at once: a bound on a client that leaks them, each pins a reader thread.
/// The number of processes is not bounded here.
pub const PER_PEER: usize = 4;
/// An answer waits this long for the client to take more of it, then the connection is closed.
const WRITE_STALL_NS: u64 = 5_000_000_000;
/// How often the watchdog asks the door about every connection.
const WATCH_NS: u64 = 1_000_000_000;
const RETRY_NS: u64 = 1_000_000;
/// The longest a call waits for its tool, whatever `timeout_ms` asks: the reader waits with it and the
/// connection is one of the process's `PER_PEER`. The network side's long calls rest on `output` and `stop`,
/// which this door does not have.
pub const CALL_MAX_NS: u64 = 60_000_000_000;
/// Strangers are told apart in the log at most this often at level Error (the rest at Warn, counted).
const REFUSAL_SAID_NS: u64 = 1_000_000_000;
/// A failing `accept` is tried again after this long at first, doubling up to `ACCEPT_RETRY_MAX_NS`.
const ACCEPT_RETRY_NS: u64 = 10_000_000;
const ACCEPT_RETRY_MAX_NS: u64 = 1_000_000_000;

klog::counter!(LOCAL_CONNECTS, "mcp.local.connects");
klog::counter!(LOCAL_REFUSED, "mcp.local.refused");
klog::counter!(LOCAL_CALLS, "mcp.local.calls");

/// One admitted connection.
pub(crate) struct Conn {
    id: u64,
    peer: Peer,
    link: Box<dyn Link>,
    pass: Box<dyn Pass>,
    /// Closed by the watchdog or `stop`; the reader ends at its next look.
    gone: AtomicBool,
}

/// The local door: its listener, its connections and its own call machinery.
pub struct Local {
    env: Arc<dyn Env>,
    sh: Arc<Shared>,
    door: Arc<dyn Door>,
    listener: SpinLock<Option<Arc<dyn Listen>>>,
    conns: SpinLock<Vec<Arc<Conn>>>,
    stop: AtomicBool,
    threads: AtomicUsize,
    next_conn: AtomicU64,
    wq: WaitQueue,
    write_stall_ns: AtomicU64,
    call_max_ns: AtomicU64,
    /// Calls whose caller stopped waiting while the tool still runs (`serve::run`).
    stuck: SpinLock<Vec<Arc<crate::calls::Call>>>,
    /// When a stranger's refusal was last an Error event, and how many were not since.
    refusal_said_ns: AtomicU64,
    refusals_unsaid: AtomicU64,
}

impl Local {
    /// A door over `statics` and the services' tools; nothing listens yet.
    pub fn new(env: Arc<dyn Env>, statics: &'static [ToolEntry], services: Option<Arc<crate::svc::Registry>>, door: Arc<dyn Door>) -> Arc<Local> {
        // No listener and no token are ever used: only the workers, deadlines and the call table.
        let sh = Shared::headless(ServerConfig::new(""), env.clone(), statics, services);
        Arc::new(Local {
            env,
            sh,
            door,
            listener: SpinLock::named("mcp.local.listener", None),
            conns: SpinLock::named("mcp.local.conns", Vec::new()),
            stop: AtomicBool::new(false),
            threads: AtomicUsize::new(0),
            next_conn: AtomicU64::new(1),
            wq: WaitQueue::new(),
            write_stall_ns: AtomicU64::new(WRITE_STALL_NS),
            call_max_ns: AtomicU64::new(CALL_MAX_NS),
            stuck: SpinLock::named("mcp.local.stuck", Vec::new()),
            refusal_said_ns: AtomicU64::new(0),
            refusals_unsaid: AtomicU64::new(0),
        })
    }

    /// How long an answer waits for a client that does not read (tests shorten it).
    pub fn set_write_stall_ns(&self, ns: u64) {
        self.write_stall_ns.store(ns, Ordering::Release);
    }

    /// The longest a call waits for its tool (tests shorten it).
    pub fn set_call_max_ns(&self, ns: u64) {
        self.call_max_ns.store(ns, Ordering::Release);
    }

    /// Takes connections from `l` in a thread of its own until `stop`.
    pub fn listen(self: &Arc<Self>, l: Arc<dyn Listen>) -> Result<(), SpawnFailed> {
        *self.listener.lock() = Some(l.clone());
        self.threads.fetch_add(2, Ordering::AcqRel);
        let (a, w) = (self.clone(), self.clone());
        let l2 = l.clone();
        let ok = self.env.spawn("mcp-local-accept", ThreadKind::Worker, Box::new(move || a.accept_loop(l2))).is_ok();
        // Without the watchdog a connection whose process ended could stay (a child may still hold the socket).
        let ok2 = ok && self.env.spawn("mcp-local-watch", ThreadKind::Worker, Box::new(move || w.watch_loop())).is_ok();
        if !ok2 {
            self.threads.fetch_sub(if ok { 1 } else { 2 }, Ordering::AcqRel);
            klog::event!(Error, "mcp", "local.listen_failed", reason = "spawn");
            self.stop();
            return Err(SpawnFailed);
        }
        Ok(())
    }

    fn stopping(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }

    fn accept_loop(self: Arc<Self>, l: Arc<dyn Listen>) {
        let mut failures: u32 = 0;
        while !self.stopping() {
            match l.accept() {
                Ok(link) => {
                    failures = 0;
                    if self.stopping() {
                        link.close();
                        break;
                    }
                    self.attach(link);
                }
                Err(e) => {
                    if self.stopping() {
                        break;
                    }
                    // The socket no longer listens: no retry brings it back (host stand-ins give -errno).
                    if [uapi::errno::EINVAL, uapi::errno::EBADF].contains(&e.abs()) {
                        klog::event!(Error, "mcp", "local.listener_dead", errno = e; "the local tools socket takes no more connections until the next start");
                        break;
                    }
                    // Out of memory or the like: slower each time, and said the first time and then rarely.
                    if failures == 0 || failures % 64 == 0 {
                        klog::event!(Warn, "mcp", "local.accept_failed", errno = e, failures = failures + 1);
                    }
                    let wait = ACCEPT_RETRY_NS.saturating_mul(1u64 << failures.min(7)).min(ACCEPT_RETRY_MAX_NS);
                    failures = failures.saturating_add(1);
                    self.env.wait(&self.wq, &mut || self.stopping(), wait);
                }
            }
        }
        self.threads.fetch_sub(1, Ordering::AcqRel);
    }

    /// Says no in one line, without waiting for the peer (a new socket's buffer is empty), and hangs up.
    /// `stranger`: the door did not admit it (else an admitted program over its bounds).
    fn refuse(&self, link: &dyn Link, why: &str, stranger: bool) {
        let peer = link.peer();
        LOCAL_REFUSED.inc();
        let now = self.env.now_ns();
        let last = self.refusal_said_ns.load(Ordering::Acquire);
        // Level Error, so it is in the log at "errors only" too; at most one a second, so a program that
        // knocks in a loop cannot fill that log. `more`: refusals since the last one said here.
        if stranger && (last == 0 || now.saturating_sub(last) >= REFUSAL_SAID_NS) && self.refusal_said_ns.compare_exchange(last, now.max(1), Ordering::AcqRel, Ordering::Acquire).is_ok() {
            let more = self.refusals_unsaid.swap(0, Ordering::AcqRel);
            klog::event!(Error, "mcp", "local.refused", pid = peer.pid, uid = peer.uid, reason = why, more = more; "a program that is not admitted asked at the local tools socket");
        } else {
            if stranger {
                self.refusals_unsaid.fetch_add(1, Ordering::AcqRel);
            }
            klog::event!(Warn, "mcp", "local.refused", pid = peer.pid, uid = peer.uid, reason = why);
        }
        let _ = link.try_write(serve::refusal_line(why).as_bytes());
        link.close();
    }

    /// Judges one new connection and serves it in a reader thread of its own (the listener calls it).
    fn attach(self: &Arc<Self>, link: Box<dyn Link>) {
        let peer = link.peer();
        let pass = match self.door.admit(peer, link.by()) {
            Ok(p) => p,
            Err(why) => return self.refuse(&*link, &why, true),
        };
        if self.conns.lock().iter().filter(|c| c.peer.pid == peer.pid).count() >= PER_PEER {
            return self.refuse(&*link, "too many connections of this process", false);
        }
        let conn = Arc::new(Conn { id: self.next_conn.fetch_add(1, Ordering::AcqRel), peer, link, pass, gone: AtomicBool::new(false) });
        self.conns.lock().push(conn.clone());
        self.threads.fetch_add(1, Ordering::AcqRel);
        LOCAL_CONNECTS.inc();
        klog::event!(Info, "mcp", "local.connect", conn = conn.id, pid = peer.pid, uid = peer.uid);
        let (me, c2) = (self.clone(), conn.clone());
        if self.env.spawn("mcp-local", ThreadKind::Worker, Box::new(move || me.serve(c2))).is_err() {
            self.threads.fetch_sub(1, Ordering::AcqRel);
            self.forget(&conn);
            conn.link.close();
            klog::event!(Error, "mcp", "local.spawn_failed", conn = conn.id; "no thread for the connection");
        }
    }

    fn forget(&self, conn: &Arc<Conn>) {
        self.conns.lock().retain(|c| !Arc::ptr_eq(c, conn));
    }

    /// Every `WATCH_NS`: closes connections whose peer is no longer the admitted program (it ended, its pid
    /// went to another process, it became another program), also when nobody sends on them.
    fn watch_loop(self: Arc<Self>) {
        while !self.stopping() {
            let conns: Vec<Arc<Conn>> = self.conns.lock().clone();
            for c in conns {
                if let Err(why) = c.pass.still()
                    && !c.gone.swap(true, Ordering::AcqRel)
                {
                    klog::event!(Info, "mcp", "local.peer_gone", conn = c.id, pid = c.peer.pid, reason = why.as_str());
                    c.link.close();
                }
            }
            self.env.wait(&self.wq, &mut || self.stopping(), WATCH_NS);
        }
        self.threads.fetch_sub(1, Ordering::AcqRel);
    }

    /// Ends the listener (the socket's name goes), every connection and the idle workers; running calls are
    /// asked to stop.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
        // Taken in its own statement: closing removes the socket file (VFS locks), never under this spin lock.
        let l = self.listener.lock().take();
        if let Some(l) = l {
            l.close();
        }
        let conns: Vec<Arc<Conn>> = self.conns.lock().clone();
        for c in conns {
            c.gone.store(true, Ordering::Release);
            c.link.close();
        }
        self.sh.stop.store(true, Ordering::Release);
        for c in self.sh.calls.lock().iter() {
            c.ask_stop();
        }
        self.sh.pool.wake_all();
        self.wq.wake_all();
    }

    /// Waits (bounded) until the listener, the watchdog, every reader and every idle worker ended.
    pub fn wait_stopped(&self, timeout_ns: u64) -> bool {
        let end = self.env.now_ns().saturating_add(timeout_ns);
        while self.threads.load(Ordering::Acquire) > 0 || self.sh.pool.waiting_workers() > 0 {
            if self.env.now_ns() >= end {
                return false;
            }
            self.env.sleep_ns(10 * RETRY_NS);
        }
        true
    }

    /// Admitted connections open right now.
    pub fn connections(&self) -> usize {
        self.conns.lock().len()
    }
    /// Worker threads inside a tool right now (a call whose caller already got its deadline error included).
    pub fn tools_running(&self) -> usize {
        self.sh.pool.in_tool()
    }
}

impl Drop for Local {
    fn drop(&mut self) {
        let l = self.listener.lock().take();
        if let Some(l) = l {
            l.close();
        }
    }
}

static KERNEL_LOCAL: SpinLock<Option<Arc<Local>>> = SpinLock::named("mcp.local.kernel", None);

/// A door on the kernel's AF_UNIX socket `path` (made as the registry's: mode 0666, the folder if missing, a
/// dead socket of the same name replaced).
pub fn start_at(path: &str, statics: &'static [ToolEntry], services: Option<Arc<crate::svc::Registry>>, door: Arc<dyn Door>) -> Result<Arc<Local>, i64> {
    let local = Local::new(Arc::new(crate::KernelEnv), statics, services, door);
    let l = crate::svc::link::bind_kernel(path)?;
    local.listen(Arc::new(l)).map_err(|_| uapi::errno::ENOMEM)?;
    Ok(local)
}

/// The kernel's local door on `SOCKET_PATH`, for the boot's `door::PROGRAM`. None when the socket could not
/// be made: the tools are then reachable over the network side only, as before MC-L.
pub fn start_kernel(statics: &'static [ToolEntry], services: Option<Arc<crate::svc::Registry>>) -> Option<Arc<Local>> {
    let door = Arc::new(door::ProgramDoor::kernel());
    match start_at(SOCKET_PATH, statics, services, door) {
        Ok(local) => {
            klog::event!(Info, "mcp", "local.listen", path = SOCKET_PATH, program = door::PROGRAM);
            *KERNEL_LOCAL.lock() = Some(local.clone());
            Some(local)
        }
        Err(e) => {
            klog::event!(Error, "mcp", "local.listen_failed", path = SOCKET_PATH, errno = e; "no local tools socket");
            None
        }
    }
}

/// The kernel's local door, once `start_kernel` made it.
pub fn kernel_local() -> Option<Arc<Local>> {
    KERNEL_LOCAL.lock().clone()
}
