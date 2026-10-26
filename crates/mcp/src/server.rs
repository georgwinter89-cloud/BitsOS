//! The MCP server (V2-PLAN §6, spec/net.md §5.1): an acceptor thread and one thread per connection, all of
//! class KernelRt (B4), every wait bounded; tool handlers run in worker threads (`worker.rs`, class Normal)
//! because a tool may block. It knows the network only through `net::KSocket`, threads, waiting and time only
//! through `Env`, so the same code runs in the kernel (`kernel::KernelEnv`, loopback or smoltcp) and in host
//! tests (`host::StdEnv`, `net::host::StdSockets`).

use crate::calls::Call;
use crate::worker::Pool;
use crate::{ToolEntry, conn};
use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use ksync::{SpinLock, WaitQueue};
use net::{Ipv4, KSocket, NetError, SockAddr, TcpListener, TcpStream};

/// What a thread of the server does; it decides the scheduling class (B4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadKind {
    /// Acceptor and connections: class KernelRt, every wait bounded.
    Server,
    /// Tool handlers, which may block: class Normal.
    Worker,
}

/// Threads, waiting and time for the server.
pub trait Env: Send + Sync {
    fn now_ns(&self) -> u64;
    fn sleep_ns(&self, ns: u64);
    /// Runs `f` in a new thread of the class `kind` stands for.
    fn spawn(&self, name: &str, kind: ThreadKind, f: Box<dyn FnOnce() + Send + 'static>) -> Result<(), SpawnFailed>;
    /// Sleeps until `cond()` is true (woken through `wq`) or `timeout_ns` passed; returns `cond()`.
    fn wait(&self, wq: &WaitQueue, cond: &mut dyn FnMut() -> bool, timeout_ns: u64) -> bool;
}

/// No thread could be started (no memory for its stack).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpawnFailed;

#[derive(Clone)]
pub struct ServerConfig {
    pub addr: SockAddr,
    /// `CONFIG.TXT token=`; never logged (R8i).
    pub token: String,
    /// Connections served at once; one more peer knocking makes the idlest give way. A bound against hostile
    /// peers, not a table size (R1): set by `mcp_conns=`, at least 3 (Georg's K3b decision).
    pub conns: usize,
    /// A connection without a request this long is closed (V1: 60 s).
    pub idle_ns: u64,
    /// A request must be complete this long after its first byte (408), and an answer taken as fast
    /// (+1 s per 64 KB) (V1: 10 s).
    pub slow_ns: u64,
    /// A new connection, or one that began a request without finishing its head, keeps its place this long
    /// before it may give way (V1's `HTTPD_FIRST_MS`).
    pub first_ns: u64,
    /// A newcomer waits this long for a place when no connection may give way, then is closed (`conn.full`).
    /// Short, because the acceptor takes no one else meanwhile.
    pub full_ns: u64,
    /// A tool's action after its answer (a restart) starts this long after the answer was handed to the
    /// socket and the connection closed, so the answer has left the machine first (`chk-restart.mjs`).
    pub after_grace_ns: u64,
}

impl core::fmt::Debug for ServerConfig {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // Never the token (R8i), not even in a debug print.
        f.debug_struct("ServerConfig")
            .field("addr", &self.addr)
            .field("token", &"***")
            .field("conns", &self.conns)
            .field("idle_ns", &self.idle_ns)
            .field("slow_ns", &self.slow_ns)
            .field("first_ns", &self.first_ns)
            .field("full_ns", &self.full_ns)
            .field("after_grace_ns", &self.after_grace_ns)
            .finish()
    }
}

pub const DEFAULT_PORT: u16 = 8765;
pub const DEFAULT_CONNS: usize = 8;
pub const MIN_CONNS: usize = 3;
const MS: u64 = 1_000_000;

impl ServerConfig {
    pub fn new(token: &str) -> Self {
        ServerConfig {
            addr: SockAddr { ip: Ipv4::ANY, port: DEFAULT_PORT },
            token: String::from(token),
            conns: DEFAULT_CONNS,
            idle_ns: 60_000 * MS,
            slow_ns: 10_000 * MS,
            first_ns: 1_000 * MS,
            full_ns: 2_000 * MS,
            after_grace_ns: 500 * MS,
        }
    }

    /// From CONFIG.TXT: `token=` (required), `mcp_port=` (default 8765), `mcp_conns=` (default 8, at least 3).
    pub fn from_config(get: &dyn Fn(&str) -> Option<String>) -> Result<Self, StartError> {
        let token = get("token").ok_or(StartError::NoToken)?;
        let mut c = ServerConfig::new(&token);
        if let Some(p) = get("mcp_port") {
            c.addr.port = p.parse::<u16>().ok().filter(|&p| p != 0).ok_or(StartError::BadPort)?;
        }
        if let Some(n) = get("mcp_conns") {
            match n.trim().parse::<usize>() {
                Ok(n) => c.conns = n,
                // B6: a setting that is not applied is said so.
                Err(_) => klog::event!(Warn, "mcp", "config.bad_value", key = "mcp_conns", value = n.chars().take(32).collect::<String>(), used = c.conns; "not a number; the default is used"),
            }
        }
        Ok(c)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartError {
    /// No `token=` in CONFIG.TXT.
    NoToken,
    /// The token must be 8 to 127 characters from '!' to '~' (V1).
    BadToken,
    BadPort,
    /// No kernel socket stack installed yet.
    NoStack,
    Listen(NetError),
    Spawn,
}

pub fn token_ok(t: &str) -> bool {
    (8..=127).contains(&t.len()) && t.bytes().all(|b| (b'!'..=b'~').contains(&b))
}

// Slot states. A connection may give way (be evicted) while it is idle between requests, while it is new and
// silent, and while a request's head is still coming (the token is not checked yet: a peer without the token
// must not hold a place). Once the head is accepted it is busy until answered.
pub(crate) const NEW: u8 = 0;
pub(crate) const IDLE: u8 = 1;
pub(crate) const HEAD: u8 = 2;
pub(crate) const BUSY: u8 = 3;
pub(crate) const EVICTED: u8 = 4;

/// What the acceptor knows about a connection.
pub(crate) struct Slot {
    pub peer: SockAddr,
    pub accepted_ns: u64,
    /// Since when it is in its state (idle since, head begun at).
    pub last_ns: AtomicU64,
    pub state: AtomicU8,
    /// MC-6: waiting for a user-space service's answer. Such a connection does not hold one of the lifeline's
    /// places (`admit` counts it apart), so a hung service never fills the server.
    pub in_service: AtomicBool,
}

impl Slot {
    /// Moves to `to` unless evicted first; false when the acceptor evicted the connection.
    fn enter(&self, to: u8, now: Option<u64>) -> bool {
        let mut s = self.state.load(Ordering::Acquire);
        loop {
            if s == EVICTED {
                return false;
            }
            if s == to {
                return true;
            }
            if let Some(n) = now {
                self.last_ns.store(n, Ordering::Release);
            }
            match self.state.compare_exchange(s, to, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return true,
                Err(now) => s = now,
            }
        }
    }
    /// The first bytes of a request came in.
    pub fn head(&self, now: u64) -> bool {
        self.enter(HEAD, Some(now))
    }
    /// The head is accepted (token and all): the request is served to its end.
    pub fn busy(&self) -> bool {
        self.enter(BUSY, None)
    }
    pub fn idle(&self, now: u64) -> bool {
        self.enter(IDLE, Some(now))
    }
    pub fn evicted(&self) -> bool {
        self.state.load(Ordering::Acquire) == EVICTED
    }
}

pub(crate) struct Shared {
    pub cfg: ServerConfig,
    pub env: Arc<dyn Env>,
    pub tools: &'static [ToolEntry],
    pub stop: AtomicBool,
    pub threads: AtomicUsize,
    pub slots: SpinLock<Vec<Arc<Slot>>>,
    pub local: SockAddr,
    pub pool: Pool,
    /// Calls in a worker, and finished ones whose caller got "Still running" (until read).
    pub calls: SpinLock<Vec<Arc<Call>>>,
    pub next_call: AtomicU64,
    /// MC-6: the tools user-space services registered (None: built-in tools only).
    pub services: Option<Arc<crate::svc::Registry>>,
    /// MC-6: connections waiting for a service's answer now (at most `cfg.conns`, apart from the lifeline's).
    pub svc_waiting: AtomicUsize,
}

impl Shared {
    pub fn now(&self) -> u64 {
        self.env.now_ns()
    }
    pub fn catalog(&self) -> crate::catalog::Catalog<'_> {
        crate::catalog::Catalog { statics: self.tools, services: self.services.as_deref() }
    }
    /// Takes one of the `cfg.conns` places for connections waiting on a service; false when all are taken.
    pub fn enter_service(&self) -> bool {
        self.svc_waiting.try_update(Ordering::AcqRel, Ordering::Acquire, |n| (n < self.cfg.conns).then_some(n + 1)).is_ok()
    }
    pub fn leave_service(&self) {
        self.svc_waiting.fetch_sub(1, Ordering::AcqRel);
    }
    pub fn remove(&self, slot: &Arc<Slot>) {
        self.slots.lock().retain(|s| !Arc::ptr_eq(s, slot));
    }
}

/// A running server. Dropping the handle leaves the server running; `stop` ends it.
#[derive(Clone)]
pub struct ServerHandle {
    sh: Arc<Shared>,
}

impl ServerHandle {
    pub fn local(&self) -> SockAddr {
        self.sh.local
    }
    /// Connections open right now (evicted ones that are still closing excluded).
    pub fn connections(&self) -> usize {
        self.sh.slots.lock().iter().filter(|s| !s.evicted()).count()
    }
    /// Asks every thread to end: the acceptor within 0.2 s, connections within 0.1 s (one waiting for a tool
    /// answers "Still running" first), idle workers within 0.1 s. Every running call is asked to stop; a worker
    /// inside a tool ends when the tool returns.
    pub fn stop(&self) {
        self.sh.stop.store(true, Ordering::Release);
        // A program a call runs is killed at once (no SIGINT grace): the server stops for a Reload or reboot.
        for c in self.sh.calls.lock().iter() {
            c.ask_stop_with(0);
        }
        self.sh.pool.wake_all();
    }
    /// Waits (bounded) until the acceptor, every connection and every worker not inside a tool ended; false if
    /// some still run after `timeout_ns`. `tools_running` tells about workers still inside a tool.
    pub fn wait_stopped(&self, timeout_ns: u64) -> bool {
        let end = self.sh.now().saturating_add(timeout_ns);
        while self.sh.threads.load(Ordering::Acquire) > 0 || self.sh.pool.waiting_workers() > 0 {
            if self.sh.now() >= end {
                return false;
            }
            self.sh.env.sleep_ns(10 * MS);
        }
        true
    }
    /// Worker threads inside a tool right now (including tools whose caller already got "Still running").
    pub fn tools_running(&self) -> usize {
        self.sh.pool.in_tool()
    }
    /// Worker threads alive (inside a tool or waiting for one).
    pub fn workers(&self) -> usize {
        self.sh.pool.alive()
    }
    /// Calls the server keeps: running ones and finished ones whose result was not read yet.
    pub fn calls(&self) -> usize {
        self.sh.calls.lock().len()
    }
}

klog::counter!(EVICTIONS, "mcp.evictions");
klog::counter!(REFUSED_PEERS, "mcp.refused_peers");

/// Starts the server: listens on `cfg.addr` and serves the tools in `tools` (normally `TOOLS.entries()`).
pub fn start(sock: &'static dyn KSocket, env: Arc<dyn Env>, cfg: ServerConfig, tools: &'static [ToolEntry]) -> Result<ServerHandle, StartError> {
    start_with(sock, env, cfg, tools, None)
}

/// `start` that also serves the tools user-space services register in `services` (MC-6). The registry lives on
/// its own: stopping the server does not stop it.
pub fn start_with(sock: &'static dyn KSocket, env: Arc<dyn Env>, mut cfg: ServerConfig, tools: &'static [ToolEntry], services: Option<Arc<crate::svc::Registry>>) -> Result<ServerHandle, StartError> {
    if !token_ok(&cfg.token) {
        klog::event!(Error, "mcp", "server.start_failed", reason = "token"; "the token must be 8 to 127 characters from ! to ~");
        return Err(StartError::BadToken);
    }
    if cfg.addr.port == 0 {
        return Err(StartError::BadPort);
    }
    if cfg.conns < MIN_CONNS {
        klog::event!(Info, "mcp", "config.raised", key = "mcp_conns", asked = cfg.conns, used = MIN_CONNS; "at least 3 connections are served");
        cfg.conns = MIN_CONNS;
    }
    // NW-3b (desk-granted): private networks only from the first packet on (the stack sends RST to a public
    // SYN before any state); the peer check in the accept loop stays as a second line.
    let listener = sock.tcp_listen_private(cfg.addr, cfg.conns).map_err(|e| {
        klog::event!(Error, "mcp", "server.start_failed", reason = "listen", port = cfg.addr.port, error = alloc::format!("{e:?}"));
        StartError::Listen(e)
    })?;
    let local = listener.local();
    let (port, conns) = (cfg.addr.port, cfg.conns);
    let sh = Arc::new(Shared { cfg, env, tools, stop: AtomicBool::new(false), threads: AtomicUsize::new(1), slots: SpinLock::named("mcp.slots", Vec::new()), local, pool: Pool::new(), calls: SpinLock::named("mcp.calls", Vec::new()), next_call: AtomicU64::new(1), services, svc_waiting: AtomicUsize::new(0) });
    let acc = sh.clone();
    if sh.env.spawn("mcp-accept", ThreadKind::Server, Box::new(move || accept_loop(acc, listener))).is_err() {
        sh.threads.store(0, Ordering::Release);
        klog::event!(Error, "mcp", "server.start_failed", reason = "spawn");
        return Err(StartError::Spawn);
    }
    klog::event!(Info, "mcp", "server.start", port = port, conns = conns, tools = tools.len());
    Ok(ServerHandle { sh })
}

const ACCEPT_SLICE_NS: u64 = 200 * MS;

fn accept_loop(sh: Arc<Shared>, listener: Box<dyn TcpListener>) {
    while !sh.stop.load(Ordering::Acquire) {
        match listener.accept(Some(ACCEPT_SLICE_NS)) {
            Ok(s) => admit(&sh, s),
            Err(NetError::TimedOut | NetError::WouldBlock) => {}
            Err(e) => {
                klog::event!(Warn, "mcp", "accept.error", error = alloc::format!("{e:?}"); "retrying in 50 ms");
                sh.env.sleep_ns(50 * MS);
            }
        }
    }
    drop(listener);
    klog::event!(Info, "mcp", "server.stop", port = sh.local.port);
    sh.threads.fetch_sub(1, Ordering::AcqRel);
}

/// The connection that gives way to a knocking peer: the one idle longest; a new silent one or one whose head
/// is still coming only after `first_ns` in that state (a client opening two at once sends its first request
/// right after the handshake), and a half head only when nothing is idle.
fn idlest(sh: &Shared, now: u64) -> Option<(Arc<Slot>, u8)> {
    sh.slots
        .lock()
        .iter()
        .filter_map(|s| {
            let st = s.state.load(Ordering::Acquire);
            let since = s.last_ns.load(Ordering::Acquire);
            let rank = match st {
                IDLE => 0,
                NEW if now.saturating_sub(s.accepted_ns) >= sh.cfg.first_ns => 0,
                HEAD if now.saturating_sub(since) >= sh.cfg.first_ns => 1,
                _ => return None,
            };
            Some(((rank, since), s.clone(), st))
        })
        .min_by_key(|(key, _, _)| *key)
        .map(|(_, s, st)| (s, st))
}

fn state_name(st: u8) -> &'static str {
    match st {
        NEW => "new",
        IDLE => "idle",
        HEAD => "head",
        BUSY => "busy",
        _ => "evicted",
    }
}

fn admit(sh: &Arc<Shared>, s: Box<dyn TcpStream>) {
    let peer = s.peer();
    if !peer.ip.is_private() {
        // Private networks only: no MCP from the internet, even through a forwarded port. A reset, as V1 sent
        // (NW-3's `shutdown`); the smoltcp stack also refuses such SYNs itself once MCP listens privately.
        REFUSED_PEERS.inc();
        klog::event!(Warn, "mcp", "conn.refused", peer = conn::ip(peer.ip), reason = "not a private network");
        s.shutdown();
        return;
    }
    let since = sh.now();
    loop {
        let now = sh.now();
        let open = sh.slots.lock().iter().filter(|s| !s.evicted() && !s.in_service.load(Ordering::Acquire)).count();
        if open < sh.cfg.conns {
            break;
        }
        if let Some((v, from)) = idlest(sh, now) {
            if v.state.compare_exchange(from, EVICTED, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                EVICTIONS.inc();
                let in_state_ms = now.saturating_sub(v.last_ns.load(Ordering::Acquire)) / MS;
                klog::event!(Info, "mcp", "conn.evicted", peer = conn::ip(v.peer.ip), port = v.peer.port, state = state_name(from), ms = in_state_ms, for_peer = conn::ip(peer.ip));
            }
            continue;
        }
        if now.saturating_sub(since) >= sh.cfg.full_ns || sh.stop.load(Ordering::Acquire) {
            klog::event!(Warn, "mcp", "conn.full", peer = conn::ip(peer.ip), conns = open, waited_ms = now.saturating_sub(since) / MS; "no connection could give way; closed the new one");
            s.close();
            return;
        }
        sh.env.sleep_ns(20 * MS);
    }
    let now = sh.now();
    let slot = Arc::new(Slot { peer, accepted_ns: now, last_ns: AtomicU64::new(now), state: AtomicU8::new(NEW), in_service: AtomicBool::new(false) });
    sh.slots.lock().push(slot.clone());
    sh.threads.fetch_add(1, Ordering::AcqRel);
    let (csh, cslot) = (sh.clone(), slot.clone());
    // The guard closes the stream if the closure is dropped without running (a failed spawn), so the peer
    // learns at once instead of waiting for its own timeout.
    let s = conn::Owned::new(s);
    if sh.env.spawn("mcp-conn", ThreadKind::Server, Box::new(move || conn::serve(csh, cslot, s))).is_err() {
        sh.remove(&slot);
        sh.threads.fetch_sub(1, Ordering::AcqRel);
        klog::event!(Error, "mcp", "conn.spawn_failed", peer = conn::ip(peer.ip); "no thread for the connection; closed it");
    }
}
