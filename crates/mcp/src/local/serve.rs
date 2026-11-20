//! One admitted connection of the local tools socket: lines in, answers out, calls through the door's own
//! workers.

use super::{Conn, LINE_MAX, LOCAL_CALLS, Local, MAX_ERRORS, RETRY_NS};
use crate::calls;
use crate::catalog::{LocalCatalog, Tool};
use crate::{Json, Safety, ToolContext, ToolError, ToolResult};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::Cell;
use core::sync::atomic::Ordering;
use mcp_wire::rpc as wire;

const CHUNK: usize = 16 * 1024;
/// Transient read errors (EINTR, EAGAIN) in a row a reader retries before it gives up on the connection.
const READ_RETRIES: usize = 100;
/// A waiting call looks at `stop` at least this often.
const SLICE_NS: u64 = 100_000_000;
/// Past its deadline a call gets this long to hand its answer over (the network side's grace).
const GRACE_NS: u64 = 250_000_000;
/// Messages a connection may send at once, and how fast after that (20 a second): above it the reader waits,
/// nothing is dropped. With `PER_PEER` connections one process stays below what klog keeps per call site
/// (100 records a second), so its calls stay in the log; an assistant sends a few a minute.
const PACE_BURST: u64 = 20;
const PACE_NS: u64 = 50_000_000;

/// Arguments a local tool has on the network side but not at this door, because they reach what a program
/// of uid 1000 cannot read otherwise. `metrics` with since/until answers from the journal (bitsjournal keeps it
/// under /data/system/journal, mode 0700: every boot, weeks back); without them it answers from this boot's
/// rings. ktest `mc.local.list_call` holds this list against the real tool's schema.
const WITHHELD: &[(&str, &[&str])] = &[("metrics", &["since", "until"])];

const INSTRUCTIONS: &str = "BitsOS's local tools socket: the tools that tell about this machine and its settings \
(tools/list names each with its owner and safety class read, change or delete_overwrite). One JSON-RPC message per \
line. Calls take an optional timeout_ms (default 10000, at most 60000); a call that is not done by then answers \
with an error and is stopped. Every other BitsOS tool is reachable only over the network endpoint.";

const INFO: wire::Info<'static> = wire::Info { version: env!("CARGO_PKG_VERSION"), instructions: INSTRUCTIONS };

/// JSON-RPC's "invalid request" with BitsOS's reason, id null: the line for a peer that is not admitted.
pub(super) fn refusal_line(why: &str) -> String {
    let mut s = String::from("{\"jsonrpc\":\"2.0\",\"id\":null,\"error\":{\"code\":-32001,\"message\":");
    crate::json::escape_into(&format!("Refused: {why}"), &mut s);
    s.push_str("}}\n");
    s
}

/// An argument the tool's schema does not name. The network side lets such arguments through to the handler
/// (V1's rule); this door does not: a local tool was judged by the arguments it declares, and one that hands
/// its arguments on (`metrics` to the journal) must not carry anything else along.
fn undeclared(tool: &Tool, args: &Json) -> Option<String> {
    let Json::Obj(given) = args else { return None };
    let schema = Json::parse(tool.input_schema()).ok();
    let props = schema.as_ref().and_then(|s| s.get("properties"));
    given.iter().map(|(k, _)| k).find(|k| k.as_str() != "timeout_ms" && props.and_then(|p| p.get(k)).is_none()).map(|k| crate::svc::proto::shown(k))
}

/// An argument of `WITHHELD` the call gives.
fn withheld(tool: &Tool, args: &Json) -> Option<&'static str> {
    let Json::Obj(given) = args else { return None };
    let (_, names) = WITHHELD.iter().find(|(t, _)| !tool.is_service() && *t == tool.name())?;
    names.iter().copied().find(|n| given.iter().any(|(k, _)| k.as_str() == *n))
}

/// The local tools, remembering what the one request asked for (for the call's event).
struct Asked<'a> {
    tools: LocalCatalog<'a>,
    /// The tool a `tools/call` named is not offered here.
    unknown: Cell<bool>,
    /// The tool it named changes something (class change or delete_overwrite).
    change: Cell<bool>,
}

impl wire::ToolSet for Asked<'_> {
    type Tool = Tool;
    fn find(&self, name: &str) -> Option<Tool> {
        let t = self.tools.find(name);
        self.unknown.set(t.is_none());
        self.change.set(t.as_ref().is_some_and(|t| t.safety() != Safety::Read));
        t
    }
    fn sorted(&self) -> Vec<Tool> {
        self.tools.sorted()
    }
}

/// The kernel around a local call: B3 with the caller's own subject.
struct Host {
    subject: policy::Subject,
    refused: Cell<bool>,
}

impl wire::Host for Host {
    fn bad_schema(&self, tool: &str, owner: &str) {
        klog::event!(Error, "mcp", "tool.bad_schema", tool = tool, owner = owner);
    }
    fn answer_cut(&self, bytes: usize, limit: usize) {
        klog::event!(Info, "mcp", "local.answer_cut", bytes = bytes, limit = limit);
    }
    fn check(&self, tool: &str) -> Result<(), String> {
        policy::check(&self.subject, &policy::Access::McpAction { tool }).map_err(|d| {
            self.refused.set(true);
            d.reason
        })
    }
}

impl Local {
    /// The reader: one line at a time until EOF, an error, an over-long line, too many bad messages, a peer
    /// that is no longer the admitted program, or `stop`.
    pub(super) fn serve(self: Arc<Self>, conn: Arc<Conn>) {
        let mut buf: Vec<u8> = Vec::new();
        let mut chunk = alloc::vec![0u8; CHUNK];
        let (mut scanned, mut retries, mut errors) = (0, 0, 0);
        // When the next message may come without waiting (`pace`).
        let mut paced_ns = 0u64;
        let reason = 'conn: loop {
            if self.stopping() {
                break "the door stops";
            }
            if conn.gone.load(Ordering::Acquire) {
                break "closed";
            }
            match conn.link.read(&mut chunk) {
                Ok(0) => break "the program closed its connection",
                Err(e) if (e == uapi::errno::EINTR || e == uapi::errno::EAGAIN) && retries < READ_RETRIES => {
                    retries += 1;
                    self.env.sleep_ns(RETRY_NS);
                    continue;
                }
                Err(_) => break "read error",
                Ok(n) => {
                    retries = 0;
                    if buf.try_reserve(n).is_err() {
                        break "no memory for the message";
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
            }
            let mut start = 0;
            while let Some(i) = buf[scanned..].iter().position(|&b| b == b'\n') {
                let end = scanned + i;
                let line = &buf[start..end];
                start = end + 1;
                scanned = start;
                if line.len() > LINE_MAX {
                    break 'conn self.too_long(&conn, line.len());
                }
                // One read can bring many lines: the door's end must not wait for all of them.
                if self.stopping() || conn.gone.load(Ordering::Acquire) {
                    break 'conn "closed";
                }
                self.pace(&mut paced_ns);
                match self.handle_line(&conn, line) {
                    Ok(true) => errors = 0,
                    Ok(false) => {
                        errors += 1;
                        if errors >= MAX_ERRORS {
                            klog::event!(Warn, "mcp", "local.too_many_errors", conn = conn.id, pid = conn.peer.pid, errors = errors; "closed a connection that does not speak the protocol");
                            break 'conn "too many bad messages in a row";
                        }
                    }
                    Err(why) => break 'conn why,
                }
            }
            buf.drain(..start);
            scanned = buf.len();
            if buf.len() > LINE_MAX {
                break self.too_long(&conn, buf.len());
            }
        };
        klog::event!(Info, "mcp", "local.ended", conn = conn.id, pid = conn.peer.pid, reason = reason);
        self.forget(&conn);
        conn.link.close();
        self.threads.fetch_sub(1, Ordering::AcqRel);
    }

    /// Lets `PACE_BURST` messages through at once, then one every `PACE_NS`: the reader sleeps the rest off.
    fn pace(&self, next_ns: &mut u64) {
        let now = self.env.now_ns();
        *next_ns = (*next_ns).max(now).saturating_add(PACE_NS);
        let wait = (*next_ns - now).saturating_sub(PACE_BURST * PACE_NS);
        if wait > 0 {
            self.env.sleep_ns(wait);
        }
    }

    fn too_long(&self, conn: &Conn, bytes: usize) -> &'static str {
        klog::event!(Warn, "mcp", "local.line_too_long", conn = conn.id, pid = conn.peer.pid, bytes = bytes, limit = LINE_MAX);
        let _ = self.write_line(conn, &format!("{{\"jsonrpc\":\"2.0\",\"id\":null,\"error\":{{\"code\":-32600,\"message\":\"A line longer than {LINE_MAX} bytes; the connection is closed\"}}}}"));
        "a message longer than the line limit"
    }

    /// Writes one whole line; an error when the client does not take it (the connection is then useless: a
    /// line cut in the middle cannot be read).
    fn write_line(&self, conn: &Conn, line: &str) -> Result<(), &'static str> {
        let stall = self.write_stall_ns.load(Ordering::Acquire);
        let parts: [&[u8]; 2] = [line.as_bytes(), b"\n"];
        let mut last = self.env.now_ns();
        for mut data in parts {
            while !data.is_empty() {
                if self.stopping() || conn.gone.load(Ordering::Acquire) {
                    return Err("closed");
                }
                match conn.link.try_write(data) {
                    Ok(0) => {
                        if self.env.now_ns().saturating_sub(last) >= stall {
                            klog::event!(Warn, "mcp", "local.write_stuck", conn = conn.id, pid = conn.peer.pid, ms = stall / 1_000_000; "the program does not read its answers; closed its connection");
                            return Err("the program does not read its answers");
                        }
                        self.env.sleep_ns(RETRY_NS);
                    }
                    Ok(n) => {
                        data = &data[n.min(data.len())..];
                        last = self.env.now_ns();
                    }
                    Err(_) => return Err("write error"),
                }
            }
        }
        Ok(())
    }

    /// Answers one message. Ok(false): a bad message; Err: the connection ends (the reason).
    fn handle_line(self: &Arc<Self>, conn: &Conn, raw: &[u8]) -> Result<bool, &'static str> {
        let raw = raw.strip_suffix(b"\r").unwrap_or(raw);
        if raw.iter().all(|b| b.is_ascii_whitespace()) {
            return Ok(true);
        }
        // The admitted image may have ended, or the pid may be another program by now: asked before every
        // request, never taken from the message.
        if let Err(why) = conn.pass.still() {
            super::LOCAL_REFUSED.inc();
            klog::event!(Warn, "mcp", "local.refused", pid = conn.peer.pid, uid = conn.peer.uid, conn = conn.id, reason = why.as_str(); "the peer is no longer the admitted program");
            let _ = conn.link.try_write(refusal_line(&why).as_bytes());
            return Err("the peer is no longer the admitted program");
        }
        let t0 = self.env.now_ns();
        let subject = conn.pass.subject();
        let asked = Asked { tools: LocalCatalog(self.sh.catalog()), unknown: Cell::new(false), change: Cell::new(false) };
        let host = Host { subject, refused: Cell::new(false) };
        let ran = Cell::new(false);
        let a = wire::handle_with(raw, t0, &asked, &host, &INFO, &mut |tool, deadline_ns, args| {
            if let Some(name) = undeclared(tool, args) {
                return Ok(Err(ToolError::BadArgument(format!("{name} is not an argument of {}", tool.name()))));
            }
            if let Some(name) = withheld(tool, args) {
                return Ok(Err(ToolError::BadArgument(format!("{name} is not an argument of {} at the local tools socket", tool.name()))));
            }
            ran.set(true);
            self.run(tool, ToolContext { caller: subject, deadline_ns }, args, conn, t0)
        });
        if a.method == "tools/call" {
            LOCAL_CALLS.inc();
            let ms = self.env.now_ns().saturating_sub(t0) / 1_000_000;
            let tool = if a.tool.is_empty() { "-" } else { a.tool.as_str() };
            let (pid, uid) = (conn.peer.pid, conn.peer.uid);
            if asked.unknown.get() {
                // Whether the name exists on the network side is said in the log only, never to the caller.
                let exists = self.sh.catalog().find(&a.tool).is_some();
                klog::event!(Warn, "mcp", "local.call", pid = pid, uid = uid, tool = tool, outcome = "refused", reason = if exists { "not a local tool" } else { "no such tool" });
            } else {
                let (outcome, reason) = match (host.refused.get(), ran.get(), a.tool_failed) {
                    (true, _, _) => ("refused", "policy"),
                    (false, false, _) => ("refused", "bad arguments"),
                    (false, true, true) => ("failed", "-"),
                    (false, true, false) => ("done", "-"),
                };
                if asked.change.get() {
                    // A call site of its own: klog limits records per site, and many read calls must not push
                    // a change out of the log.
                    klog::event!(Warn, "mcp", "local.change", pid = pid, uid = uid, tool = tool, outcome = outcome, reason = reason, ms = ms);
                } else if outcome == "refused" && reason == "policy" {
                    klog::event!(Warn, "mcp", "local.call", pid = pid, uid = uid, tool = tool, outcome = outcome, reason = reason);
                } else {
                    klog::event!(Info, "mcp", "local.call", pid = pid, uid = uid, tool = tool, outcome = outcome, reason = reason, ms = ms);
                }
            }
        }
        // A notification (or a client's own answer) gets no line.
        if !a.body.is_empty() {
            self.write_line(conn, &a.body)?;
        }
        Ok(a.status != 400)
    }

    /// Whether an earlier call of `tool` from this door is past its deadline and still inside the tool. Such a
    /// tool is not called again until it returned: every call would leave one more thread behind in it.
    fn stuck(&self, tool: &str) -> bool {
        // A copy first: `done` takes the call's own lock, never under this one.
        let all: Vec<Arc<calls::Call>> = self.stuck.lock().clone();
        let ended: Vec<u64> = all.iter().filter(|c| c.done()).map(|c| c.id).collect();
        if !ended.is_empty() {
            self.stuck.lock().retain(|c| !ended.contains(&c.id));
        }
        all.iter().any(|c| c.tool == tool && !ended.contains(&c.id))
    }

    /// Runs a tool in one of the door's workers until its deadline (at most `CALL_MAX_NS` from `t0`, when the
    /// request came), the program's hang-up or the door's end. There is no "Still running" here (`output` and
    /// `stop` are not local): then the tool is asked to stop and its answer is dropped. A service's tool ends
    /// itself at the deadline (`svc::forward` cancels it at the service).
    fn run(self: &Arc<Self>, tool: &Tool, mut ctx: ToolContext, args: &Json, conn: &Conn, t0: u64) -> Result<ToolResult, String> {
        let sh = &self.sh;
        ctx.deadline_ns = ctx.deadline_ns.min(t0.saturating_add(self.call_max_ns.load(Ordering::Acquire)));
        let deadline = ctx.deadline_ns;
        if self.stuck(tool.name()) {
            klog::event!(Warn, "mcp", "local.tool_stuck", tool = tool.name(), pid = conn.peer.pid; "an earlier call did not end at its deadline and still runs; not called again until it returns");
            return Err(format!("ERROR: {} is still busy with an earlier call that did not end at its deadline; ask again later", tool.name()));
        }
        let Ok(call) = crate::worker::submit(sh, tool, ctx, args) else {
            return Err(String::from("ERROR: no thread for the tool (out of memory)"));
        };
        let end = deadline.saturating_add(GRACE_NS);
        loop {
            if call.done() {
                calls::forget(sh, &call);
                let (result, after) = call.take_own();
                if after.is_some() {
                    // No local tool restarts anything; one that asks is not obeyed from this door.
                    klog::event!(Warn, "mcp", "local.after_dropped", tool = tool.name(); "a local call asked for an action after its answer; not done");
                }
                return Ok(result.unwrap_or_else(|| Err(ToolError::Failed(String::from("no result")))));
            }
            // The reader is here, not at the socket: nobody else notices that the program went away.
            if conn.link.hung_up() && !conn.gone.swap(true, Ordering::AcqRel) {
                klog::event!(Info, "mcp", "local.hung_up", conn = conn.id, pid = conn.peer.pid, tool = tool.name(); "the program closed its connection while its call ran");
                conn.link.close();
            }
            let now = sh.now();
            let closing = self.stopping() || conn.gone.load(Ordering::Acquire);
            if (now >= end || closing) && call.abandon() {
                call.ask_stop();
                // Nobody can ask for it later (`output` is not local): the table lets go of it now.
                calls::forget(sh, &call);
                self.stuck.lock().push(call.clone());
                if closing {
                    klog::event!(Info, "mcp", "local.call_dropped", tool = tool.name(), call = call.id, pid = conn.peer.pid; "the connection ended while the call ran; the tool was asked to stop");
                    return Err(String::from("ERROR: the connection ended"));
                }
                let ms = deadline.saturating_sub(t0) / 1_000_000;
                klog::event!(Warn, "mcp", "local.timeout", tool = tool.name(), call = call.id, pid = conn.peer.pid, ms = ms);
                return Err(format!("ERROR: {} did not answer within {ms} ms; it was asked to stop and its answer is dropped (timeout_ms gives it more time, up to {} ms)", tool.name(), self.call_max_ns.load(Ordering::Acquire) / 1_000_000));
            }
            sh.env.wait(&call.wq, &mut || call.done(), end.saturating_sub(now).clamp(1, SLICE_NS));
        }
    }
}
