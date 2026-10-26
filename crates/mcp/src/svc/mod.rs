//! MC-6: tools registered by user-space services (V2-PLAN §6 "Tools from user-space services", B5). A service
//! connects to `/run/bitsos/mcp.sock`, registers tools with a JSON schema, owner (its name) and safety class each
//! (`proto`), and answers the calls the kernel forwards; its tools vanish when its connection ends (exit, crash,
//! kill: the kernel sees EOF). Claude keeps one endpoint, and a hung service never blocks the lifeline:
//!
//! - every forwarded call ends at its deadline: the service gets `notifications/cancelled`, the caller an error,
//!   the worker is free again (no "Still running" for service calls; a longer `timeout_ms` gives more time);
//! - a connection waiting on a service does not hold one of the lifeline's places (`server::admit`), and at most
//!   `mcp_conns` such calls wait at once (`Shared::enter_service`);
//! - `tools/list` copies the registry under a short lock and never waits for a service; all services' tools
//!   together take at most `LIST_BUDGET` of the 4 MB answer;
//! - the registry's threads (one listener, one reader per service: class Normal) never run in the TCP server's
//!   threads, and a service's bytes are never trusted (line cap, schema checks, names checked against the
//!   kernel's own tools and every other service's);
//! - a connection pins a thread only while it behaves: one that has not registered within `register_ns` (10 s)
//!   is closed, at most max(8, 2 x registered services) unregistered ones wait at once, a service whose process
//!   ended is dropped even when a child it forked still holds the socket (`set_pid_alive`), and a service that
//!   sends `MAX_ERRORS` bad messages in a row is closed;
//! - trust follows the peer's uid until PM-1 decides who may register (B3): only uid 0 (services init starts)
//!   gets its declared safety listed as `readOnlyHint`/`destructiveHint`, only uid 0 may take names starting
//!   with `settings` (settingsd's), and services of other uids together get half of `LIST_BUDGET`.

pub mod link;
pub mod proto;
mod ktests;

use crate::server::{Env, SpawnFailed, ThreadKind};
use crate::{Json, Safety, ToolContext, ToolEntry, ToolError, ToolResult};
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use ksync::{SpinLock, WaitQueue};
pub use link::{Link, Listen, Peer};

/// Where services register (V2-PLAN §6).
pub const SOCKET_PATH: &str = "/run/bitsos/mcp.sock";
/// A forwarded call looks at its stop flag at least this often.
const SLICE_NS: u64 = 50_000_000;
/// A cancel notification may wait this long for the service's socket.
const CANCEL_NS: u64 = 100_000_000;
/// A full socket is tried again after this long.
const WRITE_RETRY_NS: u64 = 1_000_000;
/// Read size per `Link::read` once a service registered; before, `FIRST_CHUNK` (a connection that never
/// registers costs little).
const CHUNK: usize = 64 * 1024;
const FIRST_CHUNK: usize = 4 * 1024;
/// A connection that has not registered after this long is closed (default; `set_register_deadline_ns`).
const REGISTER_NS: u64 = 10_000_000_000;
/// How often the watchdog looks at the connections.
const WATCH_NS: u64 = 1_000_000_000;
/// Unregistered connections wait at once: at least this many, else twice the registered services.
const MIN_UNREGISTERED: usize = 8;
/// Bad messages in a row after which a service is closed (it is not speaking the protocol).
const MAX_ERRORS: usize = 16;
/// Writing a call's request to the service may take this long at most (less when the deadline is sooner):
/// a service that does not read its socket fails the call, it does not hold it to a 24 h deadline.
const REQUEST_WRITE_NS: u64 = 5_000_000_000;
/// What a registered service itself costs of `LIST_BUDGET` (bytes): the number of services stays bounded by
/// the same budget as their tools, each of which pins a reader thread.
const SERVICE_COST: usize = 4 * 1024;
/// Transient read errors (EINTR, EAGAIN) in a row a reader retries before it gives up on the connection.
const READ_RETRIES: usize = 100;

klog::counter!(SVC_CALLS, "mcp.svc.calls");
klog::counter!(SVC_TIMEOUTS, "mcp.svc.timeouts");
klog::counter!(SVC_CONNECTS, "mcp.svc.connects");
klog::counter!(SVC_REFUSED, "mcp.svc.refused");

/// Whether process `pid` still runs (proc installs it, `set_pid_alive`; mcp cannot reach the process table).
static PID_ALIVE: SpinLock<Option<fn(u32) -> bool>> = SpinLock::named("mcp.svc.pid_alive", None);

/// Installs the process check the watchdog uses to drop a service whose process ended while another process
/// (a forked child) still holds its socket: Linux gives EOF only when the last holder closes it
/// (linux-6.6/net/unix/af_unix.c unix_release_sock).
pub fn set_pid_alive(f: fn(u32) -> bool) {
    *PID_ALIVE.lock() = Some(f);
}

/// Pid 0 (a kernel thread) and no installed check (host tests) count as alive.
fn pid_alive(pid: u32) -> bool {
    let f = *PID_ALIVE.lock();
    pid == 0 || f.is_none_or(|f| f(pid))
}

/// A tool a service registered.
pub struct SvcTool {
    pub name: String,
    pub description: String,
    /// JSON text of the argument schema (checked and re-serialised by `proto`).
    pub schema: String,
    pub safety: Safety,
    /// The service's name (B5 owner).
    pub owner: String,
    pub pid: u32,
    /// The service's uid (SO_PEERCRED): only uid 0's declared safety is listed as MCP hints.
    pub uid: u32,
    listed: usize,
    service: Weak<Service>,
}

/// One call waiting for its service's answer.
struct Pending {
    st: SpinLock<PendState>,
    wq: WaitQueue,
}

struct PendState {
    result: Option<ToolResult>,
    /// Progress text the service sent (`notifications/progress`) not yet handed to the call's `output`.
    progress: String,
}

impl Pending {
    fn ready(&self) -> bool {
        let s = self.st.lock();
        s.result.is_some() || !s.progress.is_empty()
    }
}

/// One connected service.
pub(crate) struct Service {
    conn: u64,
    /// When it connected (`Env::now_ns`): the registration deadline counts from here.
    since_ns: u64,
    registered: AtomicBool,
    /// Bad messages in a row.
    errors: AtomicUsize,
    name: SpinLock<Option<String>>,
    peer: Peer,
    link: Box<dyn Link>,
    alive: AtomicBool,
    pending: SpinLock<BTreeMap<u64, Arc<Pending>>>,
    next_id: AtomicU64,
    /// One writer at a time: a line is never interleaved with another.
    writing: AtomicBool,
    wq: WaitQueue,
    env: Arc<dyn Env>,
}

impl Service {
    fn alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }
    fn name(&self) -> String {
        self.name.lock().clone().unwrap_or_else(|| format!("pid {}", self.peer.pid))
    }

    /// Writes one whole line by `deadline_ns`, giving up early when `stop()` says so. A line cut in the middle
    /// breaks the stream: the service is closed then (its tools vanish), so it never reads half a message.
    fn send(&self, line: &str, deadline_ns: u64, stop: &dyn Fn() -> bool) -> Result<(), &'static str> {
        loop {
            if !self.alive() {
                return Err("the service ended");
            }
            if stop() {
                return Err("stopped");
            }
            if self.writing.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                break;
            }
            let now = self.env.now_ns();
            if now >= deadline_ns {
                return Err("the service's socket stayed busy");
            }
            self.env.wait(&self.wq, &mut || !self.writing.load(Ordering::Acquire) || !self.alive(), (deadline_ns - now).min(SLICE_NS));
        }
        let r = self.write_all(line.as_bytes(), deadline_ns, stop);
        self.writing.store(false, Ordering::Release);
        self.wq.wake_all();
        r
    }

    fn write_all(&self, mut data: &[u8], deadline_ns: u64, stop: &dyn Fn() -> bool) -> Result<(), &'static str> {
        let total = data.len();
        while !data.is_empty() {
            match self.link.try_write(data) {
                Ok(0) => {
                    if !self.alive() {
                        return Err("the service ended");
                    }
                    let stopped = stop();
                    if stopped || self.env.now_ns() >= deadline_ns {
                        if data.len() < total {
                            klog::event!(Warn, "mcp", "svc.write_stuck", service = self.name(), pid = self.peer.pid, sent = total - data.len(), bytes = total; "the service stopped reading in the middle of a message; closed it");
                            self.link.close();
                        }
                        return Err(if stopped { "stopped" } else { "the service does not read its socket" });
                    }
                    self.env.sleep_ns(WRITE_RETRY_NS);
                }
                Ok(n) => data = &data[n.min(data.len())..],
                Err(_) => {
                    self.link.close();
                    return Err("the service's socket is broken");
                }
            }
        }
        Ok(())
    }
}

struct Inner {
    services: Vec<Arc<Service>>,
    tools: BTreeMap<String, Arc<SvcTool>>,
    /// Bytes the services' tools (and `SERVICE_COST` per registered service) take of `LIST_BUDGET`.
    listed: usize,
    /// The part of `listed` that services of uids other than 0 take (at most half the budget).
    listed_user: usize,
}

/// The tools of user-space services, and the socket they register through.
pub struct Registry {
    env: Arc<dyn Env>,
    statics: &'static [ToolEntry],
    inner: SpinLock<Inner>,
    listener: SpinLock<Option<Arc<dyn Listen>>>,
    stop: AtomicBool,
    threads: AtomicUsize,
    next_conn: AtomicU64,
    /// The watchdog sleeps here (`stop` wakes it).
    wq: WaitQueue,
    register_ns: AtomicU64,
    /// Refusing connections right now (one event per refusing spell, not one per connection).
    refusing: AtomicBool,
}

impl Registry {
    /// An empty registry; `statics` are the kernel's own tools (their names stay theirs).
    pub fn new(env: Arc<dyn Env>, statics: &'static [ToolEntry]) -> Arc<Registry> {
        Arc::new(Registry {
            env,
            statics,
            inner: SpinLock::named("mcp.svc", Inner { services: Vec::new(), tools: BTreeMap::new(), listed: 0, listed_user: 0 }),
            listener: SpinLock::named("mcp.svc.listener", None),
            stop: AtomicBool::new(false),
            threads: AtomicUsize::new(0),
            next_conn: AtomicU64::new(1),
            wq: WaitQueue::new(),
            register_ns: AtomicU64::new(REGISTER_NS),
            refusing: AtomicBool::new(false),
        })
    }

    /// How long a connection may take to register before it is closed (tests shorten it).
    pub fn set_register_deadline_ns(&self, ns: u64) {
        self.register_ns.store(ns, Ordering::Release);
    }

    /// Takes services from `l` in a thread of its own until `stop`.
    pub fn listen(self: &Arc<Self>, l: Arc<dyn Listen>) -> Result<(), SpawnFailed> {
        *self.listener.lock() = Some(l.clone());
        self.threads.fetch_add(1, Ordering::AcqRel);
        let reg = self.clone();
        let l2 = l.clone();
        if self.env.spawn("mcp-svc-accept", ThreadKind::Worker, Box::new(move || reg.accept_loop(l2))).is_err() {
            self.threads.fetch_sub(1, Ordering::AcqRel);
            self.listener.lock().take();
            l.close();
            klog::event!(Error, "mcp", "svc.listen_failed", reason = "spawn");
            return Err(SpawnFailed);
        }
        self.threads.fetch_add(1, Ordering::AcqRel);
        let reg = self.clone();
        if self.env.spawn("mcp-svc-watch", ThreadKind::Worker, Box::new(move || reg.watch_loop())).is_err() {
            // Without the watchdog a connection that never registers would pin its thread: no services then.
            self.threads.fetch_sub(1, Ordering::AcqRel);
            klog::event!(Error, "mcp", "svc.listen_failed", reason = "spawn watchdog");
            self.stop();
            return Err(SpawnFailed);
        }
        Ok(())
    }

    /// Every `WATCH_NS`: closes connections past their registration deadline and drops services whose
    /// process ended (a forked child may still hold the socket, so no EOF comes).
    fn watch_loop(self: Arc<Self>) {
        while !self.stopping() {
            let now = self.env.now_ns();
            let deadline = self.register_ns.load(Ordering::Acquire);
            let services: Vec<Arc<Service>> = self.inner.lock().services.clone();
            for s in services {
                if !s.alive() {
                    continue;
                }
                if !s.registered.load(Ordering::Acquire) {
                    if now.saturating_sub(s.since_ns) >= deadline {
                        klog::event!(Info, "mcp", "svc.register_timeout", conn = s.conn, pid = s.peer.pid, uid = s.peer.uid, ms = deadline / 1_000_000; "closed a connection that did not register");
                        self.drop_service(&s, "no register within the deadline");
                        s.link.close();
                    }
                } else if !pid_alive(s.peer.pid) {
                    self.drop_service(&s, "its process ended (another process still holds the socket)");
                    s.link.close();
                }
            }
            self.env.wait(&self.wq, &mut || self.stopping(), WATCH_NS);
        }
        self.threads.fetch_sub(1, Ordering::AcqRel);
    }

    /// Room for one more unregistered connection: at most max(`MIN_UNREGISTERED`, 2 x registered services)
    /// wait at once (R1: relative, not a fixed count).
    fn room_for_one_more(&self) -> bool {
        let inner = self.inner.lock();
        let reg = inner.services.iter().filter(|s| s.registered.load(Ordering::Acquire)).count();
        let waiting = inner.services.len() - reg;
        waiting < MIN_UNREGISTERED.max(2 * reg)
    }

    fn accept_loop(self: Arc<Self>, l: Arc<dyn Listen>) {
        while !self.stopping() {
            match l.accept() {
                Ok(link) => {
                    if self.stopping() {
                        link.close();
                        break;
                    }
                    if !self.room_for_one_more() {
                        SVC_REFUSED.inc();
                        if !self.refusing.swap(true, Ordering::AcqRel) {
                            klog::event!(Warn, "mcp", "svc.refused", pid = link.peer().pid, uid = link.peer().uid; "too many connections that have not registered; refusing new ones until they register or time out");
                        }
                        link.close();
                        continue;
                    }
                    self.refusing.store(false, Ordering::Release);
                    let _ = self.attach(link);
                }
                Err(e) => {
                    if self.stopping() {
                        break;
                    }
                    klog::event!(Warn, "mcp", "svc.accept_failed", errno = e);
                    self.env.sleep_ns(10 * WRITE_RETRY_NS);
                }
            }
        }
        self.threads.fetch_sub(1, Ordering::AcqRel);
    }

    /// Serves one service's connection in a reader thread of its own (the listener calls it; tests may too).
    pub fn attach(self: &Arc<Self>, link: Box<dyn Link>) -> Result<(), SpawnFailed> {
        let peer = link.peer();
        let conn = self.next_conn.fetch_add(1, Ordering::AcqRel);
        let svc = Arc::new(Service {
            conn,
            since_ns: self.env.now_ns(),
            registered: AtomicBool::new(false),
            errors: AtomicUsize::new(0),
            name: SpinLock::named("mcp.svc.name", None),
            peer,
            link,
            alive: AtomicBool::new(true),
            pending: SpinLock::named("mcp.svc.pending", BTreeMap::new()),
            next_id: AtomicU64::new(1),
            writing: AtomicBool::new(false),
            wq: WaitQueue::new(),
            env: self.env.clone(),
        });
        self.inner.lock().services.push(svc.clone());
        self.threads.fetch_add(1, Ordering::AcqRel);
        SVC_CONNECTS.inc();
        klog::event!(Info, "mcp", "svc.connect", conn = conn, pid = peer.pid, uid = peer.uid);
        let (reg, s2) = (self.clone(), svc.clone());
        if self.env.spawn("mcp-svc", ThreadKind::Worker, Box::new(move || reg.serve(s2))).is_err() {
            self.threads.fetch_sub(1, Ordering::AcqRel);
            self.drop_service(&svc, "no thread for the service");
            svc.link.close();
            return Err(SpawnFailed);
        }
        Ok(())
    }

    fn stopping(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }

    /// Ends the listener and every service connection (their tools vanish, calls waiting on them fail).
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
        // Taken in its own statement: closing removes the socket file (VFS locks), never under this spin lock.
        let l = self.listener.lock().take();
        if let Some(l) = l {
            l.close();
        }
        let services: Vec<Arc<Service>> = self.inner.lock().services.clone();
        for s in services {
            s.link.close();
        }
        self.wq.wake_all();
    }

    /// Waits (bounded) until the listener and every reader ended.
    pub fn wait_stopped(&self, timeout_ns: u64) -> bool {
        let end = self.env.now_ns().saturating_add(timeout_ns);
        while self.threads.load(Ordering::Acquire) > 0 {
            if self.env.now_ns() >= end {
                return false;
            }
            self.env.sleep_ns(10 * WRITE_RETRY_NS);
        }
        true
    }

    pub fn find(&self, name: &str) -> Option<Arc<SvcTool>> {
        self.inner.lock().tools.get(name).cloned()
    }

    /// Every registered tool (sorted by name).
    pub fn snapshot(&self) -> Vec<Arc<SvcTool>> {
        self.inner.lock().tools.values().cloned().collect()
    }

    pub fn tool_count(&self) -> usize {
        self.inner.lock().tools.len()
    }

    /// Connected services: (name or "pid n" before `register`, pid, tools).
    pub fn services(&self) -> Vec<(String, u32, usize)> {
        let inner = self.inner.lock();
        inner
            .services
            .iter()
            .map(|s| {
                let n = inner.tools.values().filter(|t| Weak::as_ptr(&t.service) == Arc::as_ptr(s)).count();
                (s.name(), s.peer.pid, n)
            })
            .collect()
    }

    /// Ends a service once (the reader, the watchdog and a forward may all find it dead).
    fn drop_service(&self, svc: &Arc<Service>, reason: &str) {
        if !svc.alive.swap(false, Ordering::AcqRel) {
            return;
        }
        let (tools, listed) = {
            let mut inner = self.inner.lock();
            inner.services.retain(|s| !Arc::ptr_eq(s, svc));
            let before = inner.tools.len();
            let mut freed = if svc.registered.load(Ordering::Acquire) { SERVICE_COST } else { 0 };
            inner.tools.retain(|_, t| {
                let mine = Weak::as_ptr(&t.service) == Arc::as_ptr(svc);
                if mine {
                    freed += t.listed;
                }
                !mine
            });
            inner.listed -= freed;
            if svc.peer.uid != 0 {
                inner.listed_user -= freed;
            }
            (before - inner.tools.len(), inner.listed)
        };
        let pending: Vec<Arc<Pending>> = svc.pending.lock().values().cloned().collect();
        for p in &pending {
            p.wq.wake_all();
        }
        svc.wq.wake_all();
        klog::event!(Info, "mcp", "svc.ended", service = svc.name(), pid = svc.peer.pid, conn = svc.conn, tools = tools, waiting_calls = pending.len(), listed_bytes = listed, reason = reason);
    }

    /// The reader: one line at a time until EOF, an error, an over-long line or `stop`.
    fn serve(self: Arc<Self>, svc: Arc<Service>) {
        let mut buf: Vec<u8> = Vec::new();
        let mut chunk = alloc::vec![0u8; FIRST_CHUNK];
        let mut scanned = 0;
        let mut retries = 0;
        let reason = loop {
            if self.stopping() {
                break "the registry stops";
            }
            if !svc.alive() {
                break "dropped";
            }
            if chunk.len() < CHUNK && svc.registered.load(Ordering::Acquire) {
                chunk.resize(CHUNK, 0);
            }
            match svc.link.read(&mut chunk) {
                Ok(0) => break "the service closed its connection",
                // A wakeup is no reason to drop the service's tools; ECONNRESET, EPIPE and the rest are.
                Err(e) if (e == uapi::errno::EINTR || e == uapi::errno::EAGAIN) && retries < READ_RETRIES => {
                    retries += 1;
                    self.env.sleep_ns(WRITE_RETRY_NS);
                    continue;
                }
                Err(_) => break "read error",
                Ok(n) => {
                    retries = 0;
                    if buf.try_reserve(n).is_err() {
                        break "no memory for the service's message";
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
            }
            // Every whole line in the buffer, then one drain (not one per line: short lines stay O(n)).
            let mut start = 0;
            let mut too_many = false;
            while let Some(i) = buf[scanned..].iter().position(|&b| b == b'\n') {
                let end = scanned + i;
                if self.handle_line(&svc, &buf[start..end]) {
                    svc.errors.store(0, Ordering::Release);
                } else if svc.errors.fetch_add(1, Ordering::AcqRel) + 1 >= MAX_ERRORS {
                    too_many = true;
                    break;
                }
                start = end + 1;
                scanned = start;
            }
            if too_many {
                klog::event!(Warn, "mcp", "svc.too_many_errors", service = svc.name(), pid = svc.peer.pid, errors = MAX_ERRORS; "closed a service that does not speak the protocol");
                break "too many bad messages in a row";
            }
            buf.drain(..start);
            scanned = buf.len();
            if buf.len() > proto::MAX_LINE {
                klog::event!(Warn, "mcp", "svc.line_too_long", service = svc.name(), pid = svc.peer.pid, bytes = buf.len(), limit = proto::MAX_LINE);
                break "a message longer than the line limit";
            }
        };
        self.drop_service(&svc, reason);
        svc.link.close();
        self.threads.fetch_sub(1, Ordering::AcqRel);
    }

    fn reply(&self, svc: &Service, line: &str) {
        // Answers to the service's own requests: short, so a short deadline (the service reads its socket).
        let _ = svc.send(line, self.env.now_ns().saturating_add(1_000_000_000), &|| self.stopping());
    }

    /// Handles one message; false for a bad one (`MAX_ERRORS` in a row close the service). Events for bad
    /// messages are Debug after the first of a row, so a flood does not fill the log ring.
    fn handle_line(&self, svc: &Arc<Service>, raw: &[u8]) -> bool {
        let raw = raw.strip_suffix(b"\r").unwrap_or(raw);
        if raw.iter().all(|b| b.is_ascii_whitespace()) {
            return true;
        }
        let first = svc.errors.load(Ordering::Acquire) == 0;
        let parsed = core::str::from_utf8(raw).ok().and_then(|t| Json::parse_with_members(t).ok().map(|(j, m)| (t, j, m)));
        let Some((text, msg, members)) = parsed else {
            if first {
                klog::event!(Info, "mcp", "svc.bad_message", service = svc.name(), pid = svc.peer.pid, bytes = raw.len(), reason = "not JSON");
            }
            self.reply(svc, &proto::err_line("null", -32700, "Parse error"));
            return false;
        };
        let id = members.iter().rev().find(|m| m.key == "id").map(|m| &text[m.start..m.end]);
        if let Some(method) = msg.get("method") {
            let Some(method) = method.as_str() else {
                self.reply(svc, &proto::err_line(id.unwrap_or("null"), -32600, "method must be a string"));
                return false;
            };
            let empty = Json::Obj(Vec::new());
            let params = msg.get("params").unwrap_or(&empty);
            let Some(id) = id else {
                self.notification(svc, method, params);
                return true;
            };
            if !crate::rpc::id_ok(id) {
                self.reply(svc, &proto::err_line("null", -32600, "id must be a string or an integer"));
                return false;
            }
            let (line, ok) = match method {
                "register" => match self.register(svc, params) {
                    Ok(names) => (proto::ok_line(id, &proto::names_result("registered", &names)), true),
                    Err(why) => {
                        if first {
                            klog::event!(Info, "mcp", "svc.register_refused", service = svc.name(), pid = svc.peer.pid, uid = svc.peer.uid, reason = why.as_str());
                        }
                        (proto::err_line(id, -32602, &why), false)
                    }
                },
                "unregister" => match self.unregister(svc, params) {
                    Ok(names) => (proto::ok_line(id, &proto::names_result("unregistered", &names)), true),
                    Err(why) => (proto::err_line(id, -32602, &why), false),
                },
                "ping" => (proto::ok_line(id, "{}"), true),
                other => (proto::err_line(id, -32601, &format!("Method not found: {}", proto::shown(other))), false),
            };
            self.reply(svc, &line);
            return ok;
        }
        // An answer to a forwarded call.
        let call = msg.get("id").and_then(Json::as_u64);
        let p = call.and_then(|c| svc.pending.lock().get(&c).cloned());
        match p {
            Some(p) if msg.get("result").is_some() || msg.get("error").is_some() => {
                let r = proto::result_of(&msg, &svc.name());
                p.st.lock().result = Some(r);
                p.wq.wake_all();
                true
            }
            Some(_) => {
                self.reply(svc, &proto::err_line(id.unwrap_or("null"), -32600, "an answer needs result or error"));
                false
            }
            // Cancelled already (deadline, stop) or never asked: nothing waits for it.
            None => {
                klog::event!(Debug, "mcp", "svc.late_answer", service = svc.name(), pid = svc.peer.pid, call = call.unwrap_or(0));
                true
            }
        }
    }

    fn notification(&self, svc: &Service, method: &str, params: &Json) {
        if method != "notifications/progress" {
            return;
        }
        let token = params.get("progressToken").and_then(Json::as_u64);
        let Some(p) = token.and_then(|t| svc.pending.lock().get(&t).cloned()) else { return };
        let Some(text) = params.get("message").and_then(Json::as_str) else { return };
        {
            let mut st = p.st.lock();
            if st.progress.len() + text.len() + 1 > crate::OUTPUT_KEEP {
                // Unread progress gives way to newer text (the call's own `output` keeps the newest too).
                st.progress.clear();
            }
            st.progress.push_str(text);
            st.progress.push('\n');
        }
        p.wq.wake_all();
    }

    fn register(&self, svc: &Arc<Service>, params: &Json) -> Result<Vec<String>, String> {
        let asked = match params.get("service") {
            None | Some(Json::Null) => None,
            Some(Json::Str(s)) => Some(s.as_str()),
            Some(_) => return Err(String::from("service must be a string")),
        };
        let root = svc.peer.uid == 0;
        if let Some(n) = asked {
            proto::service_name_ok(n)?;
            if self.statics.iter().any(|t| t.owner.eq_ignore_ascii_case(n)) {
                return Err(format!("service name {n} is a kernel owner's name"));
            }
            if !root && proto::root_only(n) {
                return Err(format!("service name {n}: names starting with {} are for services of uid 0 (until PM decides who may register)", proto::ROOT_PREFIX));
            }
        }
        let tools = proto::parse_tools(params)?;
        if !root && let Some(t) = tools.iter().find(|t| proto::root_only(&t.name)) {
            return Err(format!("{}: names starting with {} are for services of uid 0 (until PM decides who may register)", t.name, proto::ROOT_PREFIX));
        }
        // Names are set only under `inner`; one service's name lock at a time (never two of the same class).
        let mut inner = self.inner.lock();
        let have = svc.name.lock().clone();
        let owner = match (have.as_deref(), asked) {
            (Some(have), Some(n)) if have != n => return Err(format!("this connection registered as {have}, not {n}")),
            (Some(have), _) => have.to_owned(),
            (None, Some(n)) => {
                if let Some(o) = inner.services.iter().find(|s| !Arc::ptr_eq(s, svc) && s.name.lock().as_deref() == Some(n)) {
                    return Err(format!("service name {n} is in use (pid {})", o.peer.pid));
                }
                n.to_owned()
            }
            (None, None) => return Err(String::from("the first register needs service (the service's name)")),
        };
        let mut added = if svc.registered.load(Ordering::Acquire) { 0 } else { SERVICE_COST };
        for t in &tools {
            if self.statics.iter().any(|s| s.name == t.name) {
                return Err(format!("{} is a kernel tool", t.name));
            }
            if let Some(o) = inner.tools.get(&t.name) {
                return Err(format!("{} is registered already by {} (pid {})", t.name, o.owner, o.pid));
            }
            added += t.listed;
        }
        if inner.listed + added > proto::LIST_BUDGET {
            return Err(format!("the services' tools would take {} bytes of tools/list (at most {})", inner.listed + added, proto::LIST_BUDGET));
        }
        if !root && inner.listed_user + added > proto::LIST_BUDGET / 2 {
            return Err(format!("services of uids other than 0 would take {} bytes of tools/list (at most {})", inner.listed_user + added, proto::LIST_BUDGET / 2));
        }
        let weak = Arc::downgrade(svc);
        let mut names = Vec::with_capacity(tools.len());
        for t in tools {
            names.push(t.name.clone());
            let st = SvcTool { name: t.name, description: t.description, schema: t.schema, safety: t.safety, owner: owner.clone(), pid: svc.peer.pid, uid: svc.peer.uid, listed: t.listed, service: weak.clone() };
            inner.tools.insert(st.name.clone(), Arc::new(st));
        }
        inner.listed += added;
        if !root {
            inner.listed_user += added;
        }
        let total = inner.tools.len();
        *svc.name.lock() = Some(owner.clone());
        svc.registered.store(true, Ordering::Release);
        drop(inner);
        klog::event!(Info, "mcp", "svc.register", service = owner, pid = svc.peer.pid, tools = names.len(), total = total);
        Ok(names)
    }

    fn unregister(&self, svc: &Arc<Service>, params: &Json) -> Result<Vec<String>, String> {
        let list = params.get("tools").and_then(Json::as_array).ok_or("tools must be an array of names")?;
        let mut inner = self.inner.lock();
        let mine = |t: &SvcTool| Weak::as_ptr(&t.service) == Arc::as_ptr(svc);
        let mut names = Vec::new();
        for v in list {
            let n = v.as_str().ok_or("tools must be an array of names")?;
            match inner.tools.get(n) {
                Some(t) if mine(t) => names.push(String::from(n)),
                _ => return Err(format!("{} is not a tool of this service", proto::shown(n))),
            }
        }
        for n in &names {
            if let Some(t) = inner.tools.remove(n) {
                inner.listed -= t.listed;
                if t.uid != 0 {
                    inner.listed_user -= t.listed;
                }
            }
        }
        drop(inner);
        klog::event!(Info, "mcp", "svc.unregister", service = svc.name(), pid = svc.peer.pid, tools = names.len());
        Ok(names)
    }
}

impl Drop for Registry {
    fn drop(&mut self) {
        let l = self.listener.lock().take();
        if let Some(l) = l {
            l.close();
        }
    }
}

/// Forwards a call to the tool's service and waits for its answer until the call's deadline, `stop`, or the
/// service's end. Runs in a worker thread (class Normal); every wait is bounded (B4).
pub(crate) fn forward(t: &SvcTool, ctx: &ToolContext, args: &Json) -> ToolResult {
    let gone = || ToolError::Failed(format!("the service {} ended; its tool {} is gone (tools/list shows what is there now)", t.owner, t.name));
    let Some(svc) = t.service.upgrade().filter(|s| s.alive()) else { return Err(gone()) };
    let env = svc.env.clone();
    let id = svc.next_id.fetch_add(1, Ordering::AcqRel);
    let p = Arc::new(Pending { st: SpinLock::named("mcp.svc.call", PendState { result: None, progress: String::new() }), wq: WaitQueue::new() });
    svc.pending.lock().insert(id, p.clone());
    SVC_CALLS.inc();
    let t0 = env.now_ns();
    let timeout_ms = ctx.deadline_ns.saturating_sub(t0) / 1_000_000;
    let r = (|| {
        let write_by = ctx.deadline_ns.min(t0.saturating_add(REQUEST_WRITE_NS));
        if let Err(why) = svc.send(&proto::call_line(id, &t.name, args, timeout_ms), write_by, &|| ctx.stop_requested()) {
            klog::event!(Warn, "mcp", "svc.send_failed", service = t.owner.as_str(), tool = t.name.as_str(), reason = why);
            // Closing a half-written line ends the reader at once: report why, not only that it is gone.
            return Err(if why == "the service ended" { gone() } else { ToolError::Failed(format!("{}: {why}", t.owner)) });
        }
        loop {
            let (result, progress) = {
                let mut st = p.st.lock();
                (st.result.take(), core::mem::take(&mut st.progress))
            };
            if !progress.is_empty() {
                ctx.output(&progress);
            }
            if let Some(r) = result {
                return r;
            }
            if !svc.alive() {
                return Err(ToolError::Failed(format!("the service {} ended before it answered {} (its tools are gone)", t.owner, t.name)));
            }
            let now = env.now_ns();
            if ctx.stop_requested() {
                cancel(&svc, id, "stop");
                return Err(ToolError::Failed(format!("stopped; {} was told to cancel {}", t.owner, t.name)));
            }
            if now >= ctx.deadline_ns {
                SVC_TIMEOUTS.inc();
                klog::event!(Warn, "mcp", "svc.timeout", service = t.owner.as_str(), tool = t.name.as_str(), pid = t.pid, ms = timeout_ms);
                if !pid_alive(t.pid) {
                    // Its process ended but a child holds the socket, so no EOF comes: closing our end ends the
                    // reader at once (it reads 0), which drops the service; the watchdog would do it too.
                    klog::event!(Info, "mcp", "svc.peer_gone", service = t.owner.as_str(), pid = t.pid);
                    svc.link.close();
                    return Err(gone());
                }
                cancel(&svc, id, "timeout");
                return Err(ToolError::Failed(format!(
                    "{} did not answer {} within {timeout_ms} ms; the call was cancelled at the service (a larger timeout_ms gives it more time)",
                    t.owner, t.name
                )));
            }
            env.wait(&p.wq, &mut || p.ready() || !svc.alive(), (ctx.deadline_ns - now).min(SLICE_NS));
        }
    })();
    svc.pending.lock().remove(&id);
    r
}

/// Tells the service a call is off (MCP `notifications/cancelled`); best effort, briefly.
fn cancel(svc: &Service, id: u64, reason: &str) {
    svc.pending.lock().remove(&id);
    let ok = svc.send(&proto::cancel_line(id, reason), svc.env.now_ns().saturating_add(CANCEL_NS), &|| false).is_ok();
    klog::event!(Info, "mcp", "svc.cancel", service = svc.name(), call = id, reason = reason, sent = ok);
}

static KERNEL_REGISTRY: SpinLock<Option<Arc<Registry>>> = SpinLock::named("mcp.svc.kernel", None);

/// The kernel's registry on `SOCKET_PATH` (`start_kernel`); None when the socket could not be made (the
/// server then serves its own tools only).
pub fn start_kernel(statics: &'static [ToolEntry]) -> Option<Arc<Registry>> {
    let reg = Registry::new(Arc::new(crate::KernelEnv), statics);
    let l = match link::bind_kernel(SOCKET_PATH) {
        Ok(l) => l,
        Err(e) => {
            klog::event!(Error, "mcp", "svc.listen_failed", path = SOCKET_PATH, errno = e; "no tools from user-space services");
            return None;
        }
    };
    reg.listen(Arc::new(l)).ok()?;
    klog::event!(Info, "mcp", "svc.listen", path = SOCKET_PATH);
    *KERNEL_REGISTRY.lock() = Some(reg.clone());
    Some(reg)
}

/// The kernel's registry, once `start_kernel` made it.
pub fn kernel_registry() -> Option<Arc<Registry>> {
    KERNEL_REGISTRY.lock().clone()
}
