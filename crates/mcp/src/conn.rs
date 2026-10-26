//! One MCP connection: requests one after the other (keep-alive, pipelining, any split into reads), each
//! answered in order. Every wait has a deadline (B4): 60 s between requests, 10 s for a request from its first
//! byte (408), 10 s + 1 s per 64 KB to take an answer, a tool call's deadline (+ a short grace) for its worker.
//! One log event per request (V1's line).

use crate::server::{Shared, Slot};
use crate::calls::{self, AfterAnswer};
use crate::server::ThreadKind;
use crate::catalog::Tool;
use crate::{Json, ToolContext, ToolResult, http, rpc, worker};
use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;
use net::{Ipv4, NetError, TcpStream};

/// Reads wait at most this long at a time, so eviction and stop are seen promptly.
const SLICE_NS: u64 = 100_000_000;
const READ_CHUNK: usize = 16 * 1024;
/// A tool that watches its own deadline gets this long past it to say so before the connection gives up.
const TOOL_GRACE_NS: u64 = 250_000_000;

/// A stream that is closed when dropped unused (a connection thread that never started).
pub(crate) struct Owned(Option<Box<dyn TcpStream>>);

impl Owned {
    pub fn new(s: Box<dyn TcpStream>) -> Self {
        Owned(Some(s))
    }
}

impl Drop for Owned {
    fn drop(&mut self) {
        if let Some(s) = self.0.take() {
            s.close();
        }
    }
}

pub(crate) fn ip(a: Ipv4) -> String {
    let [a, b, c, d] = a.0;
    format!("{a}.{b}.{c}.{d}")
}

enum Next {
    KeepAlive,
    Close,
}

enum Got {
    Bytes,
    /// The peer closed, the connection broke, or the server stops.
    Gone,
    TimedOut,
}

struct Conn {
    sh: Arc<Shared>,
    slot: Arc<Slot>,
    s: Box<dyn TcpStream>,
    peer: String,
    buf: Vec<u8>,
    chunk: Vec<u8>,
    bytes_in: u64,
    bytes_out: u64,
    why: &'static str,
    /// What the last tool asked to do after its answer.
    after: Option<AfterAnswer>,
}

klog::counter!(REQUESTS, "mcp.requests");
// Requests that passed the token and Origin checks and whose answer went out (RB-6 CR1): the boot confirmation
// proves "MCP has answered" with this one, so a peer without the token cannot confirm a new generation.
klog::counter!(ANSWERED, "mcp.answered");

pub(crate) fn serve(sh: Arc<Shared>, slot: Arc<Slot>, mut s: Owned) {
    let Some(stream) = s.0.take() else { return };
    let peer = slot.peer;
    klog::event!(Debug, "mcp", "conn.open", peer = ip(peer.ip), port = peer.port);
    // From here `Conn`'s Drop closes and counts out, also when a handler or the parser panics (host tests
    // unwind; in the kernel a panic stops everything anyway).
    let mut c = Conn { sh, slot, s: stream, peer: ip(peer.ip), buf: Vec::new(), chunk: vec![0; READ_CHUNK], bytes_in: 0, bytes_out: 0, why: "panic", after: None };
    c.why = c.run();
}

impl Drop for Conn {
    fn drop(&mut self) {
        self.s.close();
        klog::event!(Debug, "mcp", "conn.close", peer = self.peer.as_str(), port = self.slot.peer.port, reason = self.why);
        self.sh.remove(&self.slot);
        self.sh.threads.fetch_sub(1, Ordering::AcqRel);
    }
}

impl Conn {
    fn stopping(&self) -> bool {
        self.sh.stop.load(Ordering::Acquire)
    }

    /// Reads what comes within `left_ns` (at most one slice).
    fn read_some(&mut self, left_ns: u64) -> Got {
        match self.s.read(&mut self.chunk, Some(left_ns.clamp(1, SLICE_NS))) {
            Ok(0) => Got::Gone,
            Ok(n) => {
                self.bytes_in += n as u64;
                self.buf.extend_from_slice(&self.chunk[..n]);
                Got::Bytes
            }
            Err(NetError::TimedOut | NetError::WouldBlock) => Got::TimedOut,
            Err(_) => Got::Gone,
        }
    }

    fn run(&mut self) -> &'static str {
        let mut idle_since = self.slot.accepted_ns;
        loop {
            let skip = http::empty_lines(&self.buf);
            self.buf.drain(..skip);
            if self.buf.is_empty() {
                if self.stopping() {
                    return "stop";
                }
                if self.slot.evicted() {
                    return "evicted";
                }
                let idle = self.sh.now().saturating_sub(idle_since);
                if idle >= self.sh.cfg.idle_ns {
                    return "idle";
                }
                match self.read_some(self.sh.cfg.idle_ns - idle) {
                    Got::Gone => return "peer",
                    Got::Bytes | Got::TimedOut => continue,
                }
            }
            if !self.slot.head(self.sh.now()) {
                return "evicted";
            }
            match self.request() {
                Next::Close if self.slot.evicted() => return "evicted",
                Next::Close => return "request",
                Next::KeepAlive => {
                    idle_since = self.sh.now();
                    if http::empty_lines(&self.buf) == self.buf.len() && !self.slot.idle(idle_since) {
                        return "evicted";
                    }
                }
            }
        }
    }

    /// Reads until `enough(buf)` or the request's deadline.
    fn fill(&mut self, deadline: u64, enough: impl Fn(&[u8]) -> bool) -> Got {
        while !enough(&self.buf) {
            let now = self.sh.now();
            if now >= deadline {
                return Got::TimedOut;
            }
            if self.stopping() || self.slot.evicted() {
                return Got::Gone;
            }
            if let Got::Gone = self.read_some(deadline - now) {
                return Got::Gone;
            }
        }
        Got::Bytes
    }

    fn request(&mut self) -> Next {
        let t0 = self.sh.now();
        let deadline = t0.saturating_add(self.sh.cfg.slow_ns);
        // Scan only what is new (a request may come one byte at a time).
        let scanned = core::cell::Cell::new(0usize);
        let head_done = |b: &[u8]| {
            let from = scanned.replace(b.len()).saturating_sub(2);
            http::head_end(b, from).is_some() || b.len() >= http::HEAD_MAX
        };
        match self.fill(deadline, head_done) {
            Got::Gone => return Next::Close,
            Got::TimedOut => return self.timed_out(t0),
            Got::Bytes => {}
        }
        let end = match http::head_end(&self.buf, 0) {
            Some(e) if e <= http::HEAD_MAX => e,
            _ => return self.refuse(t0, 431, http::what(&self.buf)),
        };
        let what = http::what(&self.buf[..end]);
        let head = http::parse_head(&self.buf[..end], self.sh.cfg.token.as_bytes());
        if head.status != 0 {
            return self.refuse(t0, head.status, what);
        }
        // Token and all accepted: from here the request is served to its end and its place is not given away.
        if !self.slot.busy() {
            return Next::Close;
        }
        let total = end + head.length;
        if head.expect && self.buf.len() < total && !self.send(b"HTTP/1.1 100 Continue\r\n\r\n", 100) {
            return Next::Close;
        }
        match self.fill(deadline, |b| b.len() >= total) {
            Got::Gone => return Next::Close,
            Got::TimedOut => return self.timed_out(t0),
            Got::Bytes => {}
        }
        let body: Vec<u8> = self.buf.drain(..total).skip(end).collect();
        let sh = self.sh.clone();
        let a = rpc::handle_with(&body, sh.now(), &sh.catalog(), &mut |tool, ctx, args| self.run_tool(tool, ctx, args));
        let after = self.after.take();
        let sent = self.answer(a.status, a.body.as_bytes(), head.close || after.is_some());
        if sent {
            ANSWERED.inc();
        }
        self.log(t0, &what, a.status, a.body.len(), &a.method, &a.tool);
        if let Some(f) = after {
            self.run_after(f, &a.tool, sent);
            return Next::Close;
        }
        if sent && !head.close { Next::KeepAlive } else { Next::Close }
    }

    /// Answer first, then restart: the action runs in its own thread once the grace has passed, by which time
    /// this connection is closed (FIN after the answer). A lost answer does not cancel it: the client asked,
    /// the request was valid, and it will see the connection end.
    fn run_after(&mut self, f: AfterAnswer, tool: &str, sent: bool) {
        let grace = self.sh.cfg.after_grace_ns;
        klog::event!(Info, "mcp", "tool.after_answer", tool = tool, peer = self.peer.as_str(), answer_sent = sent, grace_ms = grace / 1_000_000);
        let env = self.sh.env.clone();
        // Shared with the thread so a failed spawn leaves the action here.
        let slot = Arc::new(ksync::SpinLock::named("mcp.after", Some(f)));
        let theirs = slot.clone();
        let run = Box::new(move || {
            env.sleep_ns(grace);
            let f = theirs.lock().take();
            if let Some(f) = f {
                f();
            }
        });
        if self.sh.env.spawn("mcp-after", ThreadKind::Worker, run).is_err() {
            klog::event!(Warn, "mcp", "tool.after_inline", tool = tool; "no thread for the action after the answer; running it in the connection's thread");
            self.s.close();
            self.sh.env.sleep_ns(grace);
            let f = slot.lock().take();
            if let Some(f) = f {
                f();
            }
        }
    }

    /// Runs a tool in a worker and waits for it until its deadline (+ grace), the server stopping, or the
    /// answer. `output` and `stop` are answered right here (they must work while every worker is stuck). Err is
    /// the text of an `isError` result.
    fn run_tool(&mut self, tool: &Tool, ctx: ToolContext, args: &Json) -> Result<ToolResult, String> {
        if let Some(f) = calls::server_tool(tool.name()) {
            return Ok(f(&self.sh, args));
        }
        if !tool.is_service() {
            return self.run_in_worker(tool, ctx, args);
        }
        // MC-6: a call to a service waits apart from the lifeline's places (a hung service fills at most these).
        if !self.sh.enter_service() {
            klog::event!(Warn, "mcp", "svc.busy", tool = tool.name(), waiting = self.sh.cfg.conns);
            return Err(format!(
                "ERROR: {} calls to user-space services are waiting already (mcp_conns); try again when one ended. The kernel's own tools answer meanwhile",
                self.sh.cfg.conns
            ));
        }
        self.slot.in_service.store(true, Ordering::Release);
        let r = self.run_in_worker(tool, ctx, args);
        self.slot.in_service.store(false, Ordering::Release);
        self.sh.leave_service();
        r
    }

    fn run_in_worker(&mut self, tool: &Tool, ctx: ToolContext, args: &Json) -> Result<ToolResult, String> {
        let deadline = ctx.deadline_ns;
        let Ok(call) = worker::submit(&self.sh, tool, ctx, args) else {
            return Err(String::from("ERROR: no thread for the tool (out of memory)"));
        };
        let end = deadline.saturating_add(TOOL_GRACE_NS);
        loop {
            let now = self.sh.now();
            let stopping = self.stopping();
            if !call.done() && (now >= end || stopping) && call.abandon() {
                let why = if stopping { "the server stops" } else { "its deadline passed" };
                klog::event!(Warn, "mcp", "tool.still_running", tool = tool.name(), call = call.id, peer = self.peer.as_str(), reason = why);
                let mut text = format!(
                    "Still running: {} (call {id}) did not return before {why}. output {{\"call\":{id}}} shows its progress and, once it ends, its result; stop {{\"call\":{id}}} asks it to stop",
                    tool.name(),
                    id = call.id
                );
                // What the tool reported so far goes with this answer (V1's run: the output so far, then
                // "Still running"); `output` then shows only what comes after it.
                let (out, lost) = call.take_output();
                if lost > 0 {
                    text.push_str(&format!("\n[... {lost} bytes of output lost]"));
                }
                if !out.is_empty() {
                    text.push_str("\n--- progress so far ---\n");
                    text.push_str(out.trim_end_matches('\n'));
                }
                return Err(text);
            }
            if call.done() {
                calls::forget(&self.sh, &call);
                // `abandon` failed above or was never tried, so the call is ours: `output`/`stop` never take it.
                let (result, after) = call.take_own();
                self.after = after;
                return Ok(result.unwrap_or_else(|| Err(crate::ToolError::Failed(String::from("no result")))));
            }
            self.sh.env.wait(&call.wq, &mut || call.done(), end.saturating_sub(now).clamp(1, SLICE_NS));
        }
    }

    fn timed_out(&mut self, t0: u64) -> Next {
        // The buffer starts with this request's line (earlier requests were drained), so `what` is its line.
        let (what, got) = (http::what(&self.buf), self.buf.len());
        self.answer(408, b"", true);
        REQUESTS.inc();
        let ms = self.sh.now().saturating_sub(t0) / 1_000_000;
        klog::event!(Info, "mcp", "http.request", peer = self.peer.as_str(), what = what.as_str(), status = 408u16, bytes = 0usize, ms = ms, rpc = "-", tool = "-", got = got; "request not complete in time");
        Next::Close
    }

    /// Answers without a body and closes: a peer without the token gets one answer per connection, and a
    /// refused request's body is never read as the next request.
    fn refuse(&mut self, t0: u64, status: u16, what: String) -> Next {
        self.answer(status, b"", true);
        self.log(t0, &what, status, 0, "", "");
        Next::Close
    }

    fn answer(&mut self, status: u16, body: &[u8], close: bool) -> bool {
        let head = http::answer_head(status, body.len(), close);
        let mut out = Vec::with_capacity(head.len() + body.len());
        out.extend_from_slice(head.as_bytes());
        out.extend_from_slice(body);
        self.send(&out, status)
    }

    fn send(&mut self, out: &[u8], status: u16) -> bool {
        // V1: the answer must be taken within the request time, plus a second per 64 KB.
        let limit = self.sh.cfg.slow_ns.saturating_add((out.len() as u64 >> 16) * 1_000_000_000);
        match self.s.write_all(out, Some(limit)) {
            Ok(()) => {
                self.bytes_out += out.len() as u64;
                true
            }
            Err(e) => {
                klog::event!(Warn, "mcp", "http.send_failed", peer = self.peer.as_str(), status = status, bytes = out.len(), error = format!("{e:?}"));
                false
            }
        }
    }

    fn log(&self, t0: u64, what: &str, status: u16, bytes: usize, method: &str, tool: &str) {
        REQUESTS.inc();
        let ms = self.sh.now().saturating_sub(t0) / 1_000_000;
        let dash = |s: &str| if s.is_empty() { String::from("-") } else { String::from(s) };
        klog::event!(Info, "mcp", "http.request", peer = self.peer.as_str(), what = what, status = status, bytes = bytes, ms = ms, rpc = dash(method), tool = dash(tool));
    }
}
