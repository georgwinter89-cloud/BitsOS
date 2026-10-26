//! Tool workers (V2-PLAN §6, spec/net.md §5.1 "tool work that may block runs in worker threads"): a tool's
//! handler never runs in a connection's KernelRt thread. The connection hands the call to a worker (class
//! Normal) and waits for it, bounded by the call's deadline; a tool that does not return in time leaves its
//! worker behind and the client gets "Still running"; its result then waits for `output`/`stop` (`calls.rs`).
//!
//! Workers stay for the next call instead of ending when idle: the scheduler does not free a finished thread's
//! stack yet (G0), so ending and respawning would cost a stack per call. The pool grows to the most calls
//! running at once (R1: no fixed count) and ends with the server.

use crate::calls::{Active, Call};
use crate::server::{Shared, SpawnFailed, ThreadKind};
use crate::catalog::Tool;
use crate::{Json, ToolContext, ToolError};
use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};
use ksync::{SpinLock, WaitQueue};

/// An idle worker looks at `stop` this often.
const IDLE_SLICE_NS: u64 = 100_000_000;

struct Job {
    tool: Tool,
    ctx: ToolContext,
    args: Json,
    call: Arc<Call>,
}

pub(crate) struct Pool {
    queue: SpinLock<VecDeque<Job>>,
    /// Workers waiting for a job that no call has claimed yet.
    unclaimed: AtomicUsize,
    alive: AtomicUsize,
    in_tool: AtomicUsize,
    wq: WaitQueue,
}

impl Pool {
    pub fn new() -> Self {
        Pool { queue: SpinLock::named("mcp.jobs", VecDeque::new()), unclaimed: AtomicUsize::new(0), alive: AtomicUsize::new(0), in_tool: AtomicUsize::new(0), wq: WaitQueue::new() }
    }
    pub fn alive(&self) -> usize {
        self.alive.load(Ordering::Acquire)
    }
    pub fn in_tool(&self) -> usize {
        self.in_tool.load(Ordering::Acquire)
    }
    /// Workers alive and not inside a tool.
    pub fn waiting_workers(&self) -> usize {
        self.alive().saturating_sub(self.in_tool())
    }
    pub fn wake_all(&self) {
        self.wq.wake_all();
    }
    fn claim(&self) -> bool {
        self.unclaimed.try_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1)).is_ok()
    }
}

klog::counter!(WORKERS_STARTED, "mcp.workers_started");

/// Makes a call of it (in the server's table, for `output`/`stop`) and hands it to a waiting worker, or to a
/// new one.
pub(crate) fn submit(sh: &Arc<Shared>, tool: &Tool, ctx: ToolContext, args: &Json) -> Result<Arc<Call>, SpawnFailed> {
    crate::calls::prune(sh);
    let id = sh.next_call.fetch_add(1, Ordering::AcqRel);
    let call = Arc::new(Call::new(id, String::from(tool.name()), ctx.deadline_ns, sh.env.clone()));
    sh.calls.lock().push(call.clone());
    let job = Job { tool: tool.clone(), ctx, args: args.clone(), call: call.clone() };
    if sh.pool.claim() {
        sh.pool.queue.lock().push_back(job);
        sh.pool.wq.wake_all();
        return Ok(call);
    }
    sh.pool.alive.fetch_add(1, Ordering::AcqRel);
    let wsh = sh.clone();
    if sh.env.spawn("mcp-worker", ThreadKind::Worker, Box::new(move || worker(wsh, job))).is_err() {
        sh.pool.alive.fetch_sub(1, Ordering::AcqRel);
        crate::calls::forget(sh, &call);
        klog::event!(Error, "mcp", "worker.spawn_failed", tool = tool.name(); "no thread for the tool");
        return Err(SpawnFailed);
    }
    WORKERS_STARTED.inc();
    klog::event!(Debug, "mcp", "worker.start", workers = sh.pool.alive());
    Ok(call)
}

/// Counts the worker out even if a handler panics (host tests unwind; the kernel's panic stops everything).
struct Alive<'a>(&'a Shared);
impl Drop for Alive<'_> {
    fn drop(&mut self) {
        self.0.pool.alive.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Gives the caller an answer even if the handler panics, and counts the worker out of `in_tool`.
struct InTool<'a> {
    sh: &'a Shared,
    call: &'a Call,
}
impl Drop for InTool<'_> {
    fn drop(&mut self) {
        self.sh.pool.in_tool.fetch_sub(1, Ordering::AcqRel);
        if !self.call.done() {
            self.call.finish(Err(ToolError::Failed(String::from("the tool broke off (panic)"))));
        }
    }
}

fn run(sh: &Shared, job: Job) {
    sh.pool.in_tool.fetch_add(1, Ordering::AcqRel);
    let guard = InTool { sh, call: &job.call };
    let r = {
        let _linked = Active::link(&job.ctx, &job.call);
        job.tool.invoke(&job.ctx, &job.args)
    };
    job.call.finish(r);
    drop(guard);
    if job.call.abandoned() {
        let ms = sh.now().saturating_sub(job.call.started_ns) / 1_000_000;
        klog::event!(Info, "mcp", "tool.late", tool = job.tool.name(), call = job.call.id, ms = ms; "returned after its caller got Still running; the result waits for output");
    }
}

fn worker(sh: Arc<Shared>, first: Job) {
    let _alive = Alive(&sh);
    run(&sh, first);
    loop {
        sh.pool.unclaimed.fetch_add(1, Ordering::AcqRel);
        loop {
            let pool = &sh.pool;
            sh.env.wait(&pool.wq, &mut || !pool.queue.lock().is_empty() || sh.stop.load(Ordering::Acquire), IDLE_SLICE_NS);
            // Take the job in its own statement: in `if let` the lock guard would live through the whole
            // tool run, and the next `submit` or idle worker would spin on the queue lock forever (a frozen
            // kernel on the cooperative scheduler, found by ktest `mcp.tool.ktest` with two workers).
            let job = pool.queue.lock().pop_front();
            if let Some(job) = job {
                // The call that queued it claimed one waiting worker; this one is it.
                run(&sh, job);
                break;
            }
            // End with the server, unless a call claimed this worker a moment ago (its job is on the way).
            if sh.stop.load(Ordering::Acquire) && pool.claim() {
                return;
            }
        }
    }
}
