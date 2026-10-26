//! Tool calls in flight (MC-2; spec/net.md §5.1 "`output` and `stop` are answered by the server itself, for any
//! running code"). Each `tools/call` that goes to a worker becomes a `Call`: its result, the progress text the
//! tool writes with `ToolContext::output`, its stop flag, and an action to run after the answer (the
//! answer-first-then-restart rule). A call whose caller got "Still running" stays in the server's table until
//! `output` or `stop` hands its result over, or `KEEP_NS` after it finished.
//!
//! Only an abandoned call (its caller got "Still running") is ever handed over by `output`/`stop`: a call whose
//! caller still waits belongs to that caller, so `output`/`stop` may look at it (explicit call number) or ask it to
//! stop, but never take its result, and without a number they pick only abandoned calls.
//!
//! A tool reaches its call through the `ToolContext` it was given: the worker registers the context's address
//! for the time the handler runs. So the G0 context type stays as it is (two public fields), and a context made
//! outside a server (a ktest calling a handler directly) simply has no call: no output kept, never stopped, no
//! action after an answer.

use crate::server::{Env, Shared};
use crate::{Json, ToolContext, ToolError, ToolOutput, ToolResult};
use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use ksync::{SpinLock, WaitQueue};

/// A finished call nobody picked up is dropped this long after it finished.
pub const KEEP_NS: u64 = 600_000_000_000;
/// Unread progress text kept per call; older text gives way (a bound against a chatty tool, not a count).
pub const OUTPUT_KEEP: usize = 1 << 20;
/// `stop` waits this long for the tool to notice before it answers. The wait sleeps in the connection's own
/// KernelRt thread (one thread per connection, so no other connection waits with it); kept short (B4) because a
/// tool that looks at its flag at all looks far more often than this (`ktest`: between two tests).
pub const STOP_WAIT_NS: u64 = 500_000_000;
/// MC-5b (spec/net.md §5.2: "stop = SIGINT, then SIGKILL after a grace"): how long a program named by `pid`
/// gets between SIGINT and SIGKILL when `stop` names no `grace_ms`. `stop {"call"}` and `stop {}` keep A1's
/// contract (MC-5a: SIGKILL at once, grace 0) unless `grace_ms` is given. `stop` waits the grace longer.
pub const DEFAULT_GRACE_MS: u64 = 1_000;
/// The longest grace `stop` takes: it waits in the connection's own thread (B4: bounded).
pub const MAX_GRACE_MS: u64 = 10_000;

/// Something to do once the answer is out (a restart, a Reload).
pub type AfterAnswer = Box<dyn FnOnce() + Send>;

struct State {
    result: Option<ToolResult>,
    done: bool,
    /// The caller got "Still running"; the result waits for `output`/`stop`.
    abandoned: bool,
    finished_ns: u64,
    out: String,
    lost: u64,
    after: Option<AfterAnswer>,
}

pub(crate) struct Call {
    pub id: u64,
    pub tool: String,
    pub started_ns: u64,
    pub deadline_ns: u64,
    stop: AtomicBool,
    /// MC-5b: the program this call runs (`ToolContext::set_job_pid`; 0 = none), for `output`/`stop` by pid.
    pid: AtomicU32,
    /// Grace between SIGINT and SIGKILL for that program, set by `stop` before it asks (0: SIGKILL at once).
    grace_ns: AtomicU64,
    state: SpinLock<State>,
    pub wq: WaitQueue,
    env: Arc<dyn Env>,
}

impl Call {
    pub fn new(id: u64, tool: String, deadline_ns: u64, env: Arc<dyn Env>) -> Call {
        let state = State { result: None, done: false, abandoned: false, finished_ns: 0, out: String::new(), lost: 0, after: None };
        Call { id, tool, started_ns: env.now_ns(), deadline_ns, stop: AtomicBool::new(false), pid: AtomicU32::new(0), grace_ns: AtomicU64::new(0), state: SpinLock::named("mcp.call", state), wq: WaitQueue::new(), env }
    }
    pub fn done(&self) -> bool {
        self.state.lock().done
    }
    pub fn abandoned(&self) -> bool {
        self.state.lock().abandoned
    }
    pub fn now(&self) -> u64 {
        self.env.now_ns()
    }
    pub fn ask_stop(&self) {
        self.stop.store(true, Ordering::Release);
    }
    /// `ask_stop` with the grace a program of this call gets between SIGINT and SIGKILL (0: SIGKILL at once,
    /// as when the server stops).
    pub fn ask_stop_with(&self, grace_ns: u64) {
        self.grace_ns.store(grace_ns, Ordering::Release);
        self.ask_stop();
    }
    /// The pid of the program this call runs (0: none).
    pub fn pid(&self) -> u32 {
        self.pid.load(Ordering::Acquire)
    }
    pub fn stop_asked(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }
    /// The handler returned. An action after the answer is kept only while the caller still waits: an answer
    /// that said "Still running" did not announce it, so nothing restarts behind the caller's back.
    pub fn finish(&self, r: ToolResult) {
        let dropped_after = {
            let mut st = self.state.lock();
            st.result = Some(r);
            st.done = true;
            st.finished_ns = self.env.now_ns();
            if st.abandoned { st.after.take() } else { None }
        };
        if dropped_after.is_some() {
            klog::event!(Warn, "mcp", "call.after_dropped", call = self.id, tool = self.tool.as_str(); "the caller got Still running, so the action after the answer is not done");
        }
        drop(dropped_after);
        self.wq.wake_all();
    }
    /// The caller stops waiting. False when the result came first (then take it).
    pub fn abandon(&self) -> bool {
        let mut st = self.state.lock();
        if st.done {
            return false;
        }
        st.abandoned = true;
        true
    }
    /// The waiting caller takes its finished call: result and action in one step. Nothing for an abandoned call
    /// (that one belongs to `output`/`stop`).
    pub fn take_own(&self) -> (Option<ToolResult>, Option<AfterAnswer>) {
        let mut st = self.state.lock();
        if st.abandoned {
            return (None, None);
        }
        (st.result.take(), st.after.take())
    }
    /// `output`/`stop` take an abandoned call's result; `None` for a call its caller still waits for.
    fn take_abandoned(&self) -> Option<Option<ToolResult>> {
        let mut st = self.state.lock();
        st.abandoned.then(|| st.result.take())
    }
    fn push_output(&self, text: &str) {
        let mut st = self.state.lock();
        st.out.push_str(text);
        if st.out.len() > OUTPUT_KEEP {
            let mut cut = st.out.len() - OUTPUT_KEEP;
            while !st.out.is_char_boundary(cut) {
                cut += 1;
            }
            st.out.drain(..cut);
            st.lost += cut as u64;
        }
    }
    /// Unread progress text and how many bytes of it were lost; taken (the next read shows only newer text).
    pub fn take_output(&self) -> (String, u64) {
        let mut st = self.state.lock();
        (core::mem::take(&mut st.out), core::mem::take(&mut st.lost))
    }
    fn set_after(&self, f: AfterAnswer) -> bool {
        let mut st = self.state.lock();
        if st.done || st.abandoned {
            return false;
        }
        st.after = Some(f);
        true
    }
    fn finished_ns(&self) -> Option<u64> {
        let st = self.state.lock();
        st.done.then_some(st.finished_ns)
    }
}

// ---------------------------------------------------------------------------------------------------------
// From a ToolContext to its call.

/// (address of the ToolContext a handler runs with, its call). Short spin sections only.
static ACTIVE: SpinLock<Vec<(usize, Arc<Call>)>> = SpinLock::named("mcp.active", Vec::new());

fn key(ctx: &ToolContext) -> usize {
    ctx as *const ToolContext as usize
}

/// Links `ctx` to `call` while the handler runs; the guard unlinks it (also when the handler panics).
pub(crate) struct Active(usize);

impl Active {
    pub fn link(ctx: &ToolContext, call: &Arc<Call>) -> Active {
        let k = key(ctx);
        ACTIVE.lock().push((k, call.clone()));
        Active(k)
    }
}

impl Drop for Active {
    fn drop(&mut self) {
        ACTIVE.lock().retain(|(k, _)| *k != self.0);
    }
}

fn call_of(ctx: &ToolContext) -> Option<Arc<Call>> {
    let k = key(ctx);
    ACTIVE.lock().iter().find(|(a, _)| *a == k).map(|(_, c)| c.clone())
}

impl ToolContext {
    /// A context for calling a handler directly (tests, a tool calling another): no call behind it.
    pub fn detached(deadline_ns: u64) -> ToolContext {
        ToolContext { caller: policy::Subject::KERNEL, deadline_ns }
    }
    /// The server's clock (the kernel's monotonic clock; a detached context uses `time::now_ns`).
    pub fn now_ns(&self) -> u64 {
        call_of(self).map(|c| c.now()).unwrap_or_else(time::now_ns)
    }
    pub fn remaining_ns(&self) -> u64 {
        self.deadline_ns.saturating_sub(self.now_ns())
    }
    /// The deadline passed: the caller has (or is about to get) "Still running". A tool may go on (its result
    /// then waits for `output`), but one that loops should look at `stop_requested` too.
    pub fn expired(&self) -> bool {
        self.remaining_ns() == 0
    }
    /// `stop` was called for this call, or the server stops. A tool that loops ends at the next look.
    pub fn stop_requested(&self) -> bool {
        call_of(self).is_some_and(|c| c.stop_asked())
    }
    /// Progress text for `output` (shown before the result; the newest `OUTPUT_KEEP` bytes are kept unread).
    pub fn output(&self, text: &str) {
        if let Some(c) = call_of(self) {
            c.push_output(text);
        }
    }
    /// MC-5b: this call runs program `pid` (a job): `output {"pid"}` and `stop {"pid"}` find the call by it.
    pub fn set_job_pid(&self, pid: u32) {
        if let Some(c) = call_of(self) {
            c.pid.store(pid, Ordering::Release);
        }
    }
    /// How long the program gets between SIGINT and SIGKILL once `stop_requested` (`stop`'s `grace_ns`; 0 when
    /// the server stops).
    pub fn stop_grace_ns(&self) -> u64 {
        call_of(self).map_or(0, |c| c.grace_ns.load(Ordering::Acquire))
    }
    /// The call's number (shown in "Still running" answers; `output`/`stop` take it).
    pub fn call_id(&self) -> Option<u64> {
        call_of(self).map(|c| c.id)
    }
    /// Runs `f` once this call's answer has been sent and the connection closed (answer first, then restart:
    /// `reboot`, `reload`). False when there is no answer to wait for (a detached context, a call whose caller
    /// already got "Still running"): then the tool must not do it at all.
    pub fn after_answer(&self, f: AfterAnswer) -> bool {
        call_of(self).is_some_and(|c| c.set_after(f))
    }
}

// ---------------------------------------------------------------------------------------------------------
// The server's table, and the two tools the server answers itself.

/// Removes finished calls nobody picked up within `KEEP_NS`.
pub(crate) fn prune(sh: &Shared) {
    let now = sh.now();
    let mut gone = Vec::new();
    sh.calls.lock().retain(|c| match c.finished_ns() {
        Some(t) if now.saturating_sub(t) >= KEEP_NS => {
            gone.push((c.id, c.tool.clone()));
            false
        }
        _ => true,
    });
    for (id, tool) in gone {
        klog::event!(Info, "mcp", "call.expired", call = id, tool = tool; "finished long ago and never read; result dropped");
    }
}

pub(crate) fn forget(sh: &Shared, call: &Arc<Call>) {
    sh.calls.lock().retain(|c| !Arc::ptr_eq(c, call));
}

/// `output` and `stop` run in the connection's thread, never in a worker: they must answer while every worker
/// is stuck inside a tool.
pub(crate) fn server_tool(name: &str) -> Option<fn(&Shared, &Json) -> ToolResult> {
    match name {
        "output" => Some(output),
        "stop" => Some(stop),
        _ => None,
    }
}

/// MC-5b: `output`/`stop` for a process that no call of this server runs (a console job, a program another
/// program started): proc installs them (`set_process_tools`; mcp cannot reach the process table).
#[derive(Clone, Copy)]
pub struct ProcessTools {
    /// What `output {"pid"}` says about such a process (its state; its output is not captured).
    pub output: fn(pid: u32) -> ToolResult,
    /// `stop {"pid"|"pgid"}`: SIGINT, then SIGKILL after `grace_ns` (bounded wait); `confirm` for PID 1.
    pub stop: fn(target: StopTarget, grace_ns: u64, confirm: bool) -> ToolResult,
}

/// What `stop` ends when no call runs it: one process, or a process group (a console job, as Ctrl+C).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopTarget {
    Pid(u32),
    Pgid(u32),
}

static PROCESS_TOOLS: SpinLock<Option<ProcessTools>> = SpinLock::named("mcp.process_tools", None);

/// Installs the process back end of `output`/`stop` by pid (proc, at Service init).
pub fn set_process_tools(t: ProcessTools) {
    *PROCESS_TOOLS.lock() = Some(t);
}

fn process_tools() -> Result<ProcessTools, ToolError> {
    (*PROCESS_TOOLS.lock()).ok_or_else(|| ToolError::Failed(String::from("no call of this server runs it, and no process back end is installed")))
}

enum Pick {
    Call(Arc<Call>),
    /// No call of this server runs it.
    Process(StopTarget),
    Nothing,
}

/// A positive integer argument that fits `max` (None when absent).
fn number(args: &Json, key: &str, max: u64) -> Result<Option<u64>, ToolError> {
    match args.get(key) {
        None | Some(Json::Null) => Ok(None),
        Some(v) => v.as_u64().filter(|n| (1..=max).contains(n)).map(Some).ok_or_else(|| ToolError::BadArgument(String::from(key))),
    }
}

/// The call `args` names (`call`, or `pid`: the newest call running that program), or (neither) the newest
/// abandoned one: a call whose caller still waits is never a default, so one client's `output {}` or `stop {}`
/// cannot take or stop another client's call by accident. A `pid` no call runs (or a `pgid`, `stop` only) is a
/// process for `ProcessTools`.
fn pick(sh: &Shared, args: &Json, running_only: bool, pgid_too: bool) -> Result<Pick, ToolError> {
    let call = number(args, "call", u64::MAX)?;
    let pid = number(args, "pid", u32::MAX as u64)?.map(|n| n as u32);
    let pgid = if pgid_too { number(args, "pgid", u32::MAX as u64)?.map(|n| n as u32) } else { None };
    if [call.is_some(), pid.is_some(), pgid.is_some()].iter().filter(|x| **x).count() > 1 {
        return Err(ToolError::BadArgument(String::from(if pgid_too { "call, pid or pgid: give one of them" } else { "call or pid: give one of them" })));
    }
    let calls = sh.calls.lock();
    if let Some(g) = pgid {
        return Ok(Pick::Process(StopTarget::Pgid(g)));
    }
    if let Some(p) = pid {
        return Ok(match calls.iter().filter(|c| c.pid() == p).max_by_key(|c| c.id) {
            Some(c) => Pick::Call(c.clone()),
            None => Pick::Process(StopTarget::Pid(p)),
        });
    }
    match call {
        None => Ok(calls.iter().filter(|c| c.abandoned() && (!running_only || !c.done())).max_by_key(|c| c.id).cloned().map_or(Pick::Nothing, Pick::Call)),
        Some(id) => calls.iter().find(|c| c.id == id).cloned().map(Pick::Call).ok_or_else(|| {
            ToolError::Failed(format!("no call {id} here (finished calls are kept until read, at most {} min)", KEEP_NS / 60_000_000_000))
        }),
    }
}

fn secs(ns: u64) -> String {
    format!("{}.{} s", ns / 1_000_000_000, ns % 1_000_000_000 / 100_000_000)
}

/// "call 7 ktest: running for 3.2 s" plus the unread progress text.
fn report(c: &Call, state: &str) -> String {
    let now = c.now();
    let mut s = format!("call {} {}: {state} after {}", c.id, c.tool, secs(now.saturating_sub(c.started_ns)));
    if !c.done() && now > c.deadline_ns {
        s.push_str(&format!(" (deadline passed {} ago)", secs(now - c.deadline_ns)));
    }
    if c.stop_asked() && !c.done() {
        s.push_str(" (stop asked)");
    }
    let (out, lost) = c.take_output();
    if lost > 0 {
        s.push_str(&format!("\n[... {lost} bytes of output lost]"));
    }
    if !out.is_empty() {
        s.push('\n');
        s.push_str(out.trim_end_matches('\n'));
    }
    s
}

/// The finished call's answer: its report first, then the tool's own content. Taken out of the table. A call
/// whose caller still waits keeps its result for that caller (decided under the call's lock).
fn hand_over(sh: &Shared, c: &Arc<Call>, state: &str) -> ToolResult {
    let Some(result) = c.take_abandoned() else {
        return Ok(alloc::vec![ToolOutput::Text(report(c, &format!("{state}; {WAITED}")))]);
    };
    forget(sh, c);
    let head = report(c, state);
    match result {
        Some(Ok(mut out)) => {
            out.insert(0, ToolOutput::Text(head));
            Ok(out)
        }
        Some(Err(ToolError::BadArgument(a))) => Err(ToolError::Failed(format!("{head}\nresult: bad argument {a}"))),
        Some(Err(ToolError::Refused(r))) => Err(ToolError::Refused(format!("{head}\nresult: {r}"))),
        Some(Err(ToolError::Failed(f))) => Err(ToolError::Failed(format!("{head}\nresult: {f}"))),
        None => Err(ToolError::Failed(format!("{head}\nresult: already handed over"))),
    }
}

const WAITED: &str = "its caller is still waiting and gets the result";

/// `output {"call": n}`: new progress text and the state of a call (default: the newest abandoned one), and its
/// result once it finished (then the call is gone from the table). A call whose caller still waits is only shown.
fn output(sh: &Shared, args: &Json) -> ToolResult {
    prune(sh);
    let c = match pick(sh, args, false, false)? {
        Pick::Call(c) => c,
        Pick::Process(StopTarget::Pid(p)) | Pick::Process(StopTarget::Pgid(p)) => return (process_tools()?.output)(p),
        Pick::Nothing => return Ok(alloc::vec![ToolOutput::Text(String::from("no tool call is running or waiting to be read"))]),
    };
    if c.done() {
        return hand_over(sh, &c, "finished");
    }
    let state = if c.abandoned() { "running" } else { "running; its caller is still waiting" };
    Ok(alloc::vec![ToolOutput::Text(report(&c, state))])
}

/// `stop {"call": n}`: asks a running call (default: the newest running abandoned one) to stop, waits up to
/// `STOP_WAIT_NS`, and answers with its result if it ended (a call whose caller still waits: that caller gets
/// it). Kernel tools cannot be killed; one that never looks at its stop flag runs on, and the answer says so.
fn stop(sh: &Shared, args: &Json) -> ToolResult {
    prune(sh);
    // A pid or pgid gets the spec's grace (SIGINT first); only the call form keeps A1's SIGKILL at once.
    let by_pid = ["pid", "pgid"].iter().any(|k| !matches!(args.get(k), None | Some(Json::Null)));
    let grace_ms = match args.get("grace_ms") {
        None | Some(Json::Null) if by_pid => DEFAULT_GRACE_MS,
        None | Some(Json::Null) => 0,
        Some(v) => v.as_u64().filter(|g| *g <= MAX_GRACE_MS).ok_or_else(|| ToolError::BadArgument(format!("grace_ms (0..={MAX_GRACE_MS})")))?,
    };
    let grace_ns = grace_ms * 1_000_000;
    let c = match pick(sh, args, true, true)? {
        Pick::Call(c) => c,
        Pick::Process(t) => {
            let confirm = args.get("confirm").and_then(Json::as_bool).unwrap_or(false);
            return (process_tools()?.stop)(t, grace_ns, confirm);
        }
        Pick::Nothing => return Ok(alloc::vec![ToolOutput::Text(String::from("no tool call is running"))]),
    };
    if c.done() {
        return hand_over(sh, &c, "had finished");
    }
    c.ask_stop_with(grace_ns);
    klog::event!(Info, "mcp", "call.stop", call = c.id, tool = c.tool.as_str(), caller_waits = !c.abandoned(), pid = c.pid(), grace_ms = grace_ms);
    // A program gets its grace between SIGINT and SIGKILL on top.
    let wait = STOP_WAIT_NS + if c.pid() != 0 { grace_ns } else { 0 };
    sh.env.wait(&c.wq, &mut || c.done(), wait);
    if c.done() {
        return hand_over(sh, &c, "stopped");
    }
    klog::event!(Warn, "mcp", "call.stop_ignored", call = c.id, tool = c.tool.as_str(), waited_ms = wait / 1_000_000; "the tool has not looked at its stop flag yet");
    let mut s = report(&c, "still running");
    s.push_str("\nstop asked; the tool ends when it next looks at its stop flag (output shows when)");
    Ok(alloc::vec![ToolOutput::Text(s)])
}
