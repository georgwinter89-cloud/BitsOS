//! The `run` MCP tool (AB-2 registered it, MC-5a made it a job runner; Safety Change; V2-PLAN §6,
//! spec/net.md §5.2): starts a program from the file tree (or an ELF image sent along as base64) with arguments,
//! environment and stdin, and answers with its exit status, its stdout and stderr (separately, each kept up to
//! `OUTPUT_CAP`) and the V2 fault message when a fault ended it.
//!
//! Deadline: the call's `timeout_ms` (run's default 30 s, from its schema). When it passes, the answer is
//! "Still running ... (call n)" with the program's pid and its output so far; the program keeps running until it
//! ends or is killed. `output {"call": n}` then shows its new stdout/stderr and, once it ended, the whole result;
//! `stop {"call": n}` kills it (SIGKILL, A1's contract); `stop {"pid": p}` sends SIGINT, then SIGKILL after the
//! stop's grace (MC-5b, spec/net.md §5.2; `grace_ms` with a call does the same; a stopping server kills at once). The call number is the job id, the pid finds it too
//! (`ToolContext::set_job_pid`). `kill_after_ms` ends it by itself (SIGKILL). MC-5b's `ps`/`kill`: `ps.rs`.
//!
//! B3: the MCP layer checks `McpAction("run")`; the tool also checks reading the program's path, or
//! `McpAction("run.elf_b64")` for an image sent along. B4: the tool runs in an MCP worker (class Normal) and
//! waits in short slices, looking at `stop` each time; the program's writes never block on the capture.

mod ktests;
pub(crate) mod progs;
mod ps;
mod ps_ktests;
mod svc_ktests;

use crate::stdio::Input;
use crate::{Process, SpawnOptions, status::ExitStatus};
use alloc::format;
use alloc::string::String;
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};
use ksync::SpinLock;
use mcp::{Json, ToolContext, ToolError, ToolOutput, ToolResult};
use policy::ProgramId;
use uapi::errno::*;
use vfs::{Errno, Events, File};

/// Kept per stream for the answer (a bound against a program that prints without end; MCP answers stay below
/// 4 MB).
pub const OUTPUT_CAP: usize = 1 << 20;
/// Unread progress kept per stream once the deadline came; older bytes give way (counted, never blocking).
const LIVE_KEEP: usize = 1 << 20;
/// Most JSON-escaped bytes a stream may take in the answer: two streams of binary output (each byte a
/// six-byte `\u00XX` escape) would not fit into 4 MB otherwise.
const SHOW_MAX: usize = 1_750 * 1024;
/// run's own default deadline (its schema's `timeout_ms.default`).
pub const DEFAULT_DEADLINE_MS: u64 = 30_000;
const SLICE_NS: u64 = 50_000_000;
/// Progress starts this long before the deadline, so the "Still running" answer carries the output so far.
const LEAD_NS: u64 = 2 * SLICE_NS;
/// After a kill, how long the tool waits for the program's thread to end before it answers.
const KILL_WAIT_NS: u64 = 5_000_000_000;
/// After the tool answered, how long `retry_kill` goes on nudging a killed program that is still in user mode.
const KILL_RETRY_NS: u64 = 60_000_000_000;
/// A killed program still in user mode after this long gets its CPU interrupted (`nudge`), and again after
/// each further interval.
const NUDGE_EVERY_NS: u64 = 200_000_000;
/// Programs started over MCP until packages (PK) give them their own ids.
pub const MCP_PROGRAM: ProgramId = ProgramId(1);
/// init's and the console shell's PATH (userland INIT_PATH, UL-5): stick programs come last.
/// WY-1a CR6: XDG_RUNTIME_DIR is the directory init makes for user 1000 (bitscomp's Wayland and control sockets).
/// UL-6: the console shell's language too (glibc falls back to C when the locale is missing).
const DEFAULT_ENV: [&str; 4] = ["PATH=/bin:/sbin:/usr/bin:/boot/BITSOS2/bin", "HOME=/", "XDG_RUNTIME_DIR=/run/user/1000", "LANG=de_DE.UTF-8"];
/// UL-5: programs on the stick (\\BITSOS2\\bin under FS-7's /boot mount); a bare name not in /bin is looked up here.
const STICK_BIN: &str = "/boot/BITSOS2/bin";
/// The name of a program sent as `elf_b64` without a `path`.
const INLINE_NAME: &str = "/mcp/inline";
const S_IFIFO: u32 = 0o010000;

fn bad(what: &str) -> ToolError {
    ToolError::BadArgument(String::from(what))
}

/// A list of strings (`null`/absent: None).
fn strings(args: &Json, key: &str) -> Result<Option<Vec<String>>, ToolError> {
    match args.get(key) {
        None | Some(Json::Null) => Ok(None),
        Some(Json::Arr(a)) => a.iter().map(|v| v.as_str().map(String::from).ok_or_else(|| bad(key))).collect::<Result<Vec<_>, _>>().map(Some),
        Some(_) => Err(bad(key)),
    }
}

fn base64(args: &Json, key: &str) -> Result<Option<Vec<u8>>, ToolError> {
    match args.get(key) {
        None | Some(Json::Null) => Ok(None),
        Some(Json::Str(s)) => mcp::b64::decode(s).map(Some).ok_or_else(|| bad(key)),
        Some(_) => Err(bad(key)),
    }
}

/// What the program gets.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Request {
    /// The file to open, or (with `image`) only the program's name.
    pub path: String,
    /// The ELF sent along (`elf_b64`) instead of a file.
    pub image: Option<Vec<u8>>,
    pub argv: Vec<String>,
    pub envp: Vec<String>,
    pub stdin: Vec<u8>,
    pub kill_after_ns: Option<u64>,
    /// The working directory (default "/").
    pub cwd: Option<String>,
    /// `path` was a bare name (looked up in /bin, then the stick's program directory).
    pub bare: bool,
}

/// Checks the arguments (pure, host-tested).
pub(crate) fn parse(args: &Json) -> Result<Request, ToolError> {
    let image = base64(args, "elf_b64")?;
    if image.as_ref().is_some_and(Vec::is_empty) {
        return Err(bad("elf_b64 (empty)"));
    }
    let given = match (args.get("path"), &image) {
        (Some(Json::Str(s)), _) => s.as_str(),
        (None | Some(Json::Null), Some(_)) => INLINE_NAME,
        _ => return Err(bad("path")),
    };
    if given.is_empty() || given.contains('\0') || given.len() > 4096 {
        return Err(bad("path"));
    }
    // A bare name is looked up in /bin, then in the stick's program directory (`resolve_bare`, UL-5).
    let bare = !given.contains('/') && image.is_none();
    let path = if given.contains('/') { String::from(given) } else { format!("/bin/{given}") };
    let argv = match (strings(args, "argv")?, strings(args, "args")?) {
        (Some(_), Some(_)) => return Err(bad("argv and args together")),
        (Some(v), None) if !v.is_empty() => v,
        (Some(_), None) | (None, None) => alloc::vec![String::from(given)],
        (None, Some(rest)) => core::iter::once(String::from(given)).chain(rest).collect(),
    };
    let envp = match args.get("env") {
        None | Some(Json::Null) => DEFAULT_ENV.iter().map(|s| String::from(*s)).collect(),
        Some(Json::Arr(_)) => strings(args, "env")?.unwrap_or_default(),
        Some(Json::Obj(kv)) => kv
            .iter()
            .map(|(k, v)| match v {
                Json::Str(s) if !k.is_empty() && !k.contains('=') => Ok(format!("{k}={s}")),
                _ => Err(bad("env")),
            })
            .collect::<Result<Vec<_>, _>>()?,
        Some(_) => return Err(bad("env")),
    };
    if argv.iter().chain(envp.iter()).any(|s| s.contains('\0')) {
        return Err(bad("argv/env: NUL inside a string"));
    }
    let stdin = match (args.get("stdin"), base64(args, "stdin_b64")?) {
        (None | Some(Json::Null), b) => b.unwrap_or_default(),
        (Some(Json::Str(s)), None) => s.as_bytes().to_vec(),
        (Some(Json::Str(_)), Some(_)) => return Err(bad("stdin and stdin_b64 together")),
        (Some(_), _) => return Err(bad("stdin")),
    };
    // FS-1's per-process working directory: any absolute path (checked to be a directory at the spawn).
    let cwd = match args.get("cwd") {
        None | Some(Json::Null) => None,
        Some(Json::Str(s)) if s.starts_with('/') && !s.contains('\0') && s.len() <= 4096 => Some(s.clone()),
        Some(_) => return Err(bad("cwd (an absolute directory path)")),
    };
    let kill_after_ns = match args.get("kill_after_ms") {
        None | Some(Json::Null) => None,
        Some(v) => match v.as_u64() {
            Some(ms) if (1..=86_400_000).contains(&ms) => Some(ms * 1_000_000),
            _ => return Err(bad("kill_after_ms")),
        },
    };
    Ok(Request { path, image, argv, envp, stdin, kill_after_ns, cwd, bare })
}

// ---------------------------------------------------------------------------------------------------------
// The capture of one output stream.

struct StreamState {
    /// The first `cap` bytes, for the answer (grows by doubling: amortised, at most `cap` in all).
    kept: Vec<u8>,
    /// Bytes past `cap` (or refused by the heap).
    dropped: u64,
    /// Once the deadline is near: bytes not yet handed out as progress (the newest `LIVE_KEEP`). Its room is
    /// reserved once in `start_live`, so a write never allocates for it and gives way at the front in O(bytes):
    /// the program's write costs O(its own length), never O(LIVE_KEEP).
    live: Option<VecDeque<u8>>,
    /// Stream offset of `live[0]`.
    live_from: u64,
    total: u64,
}

/// One output stream of a `run` program (S_IFIFO like a shell redirection, never a terminal). The program never
/// blocks on it: what does not fit is counted.
pub(crate) struct Stream {
    state: SpinLock<StreamState>,
    cap: usize,
}

/// New progress of a stream: the bytes, the stream offset where they start, and the offset they end at.
pub(crate) struct Live {
    pub bytes: Vec<u8>,
    pub from: u64,
    pub end: u64,
}

impl Stream {
    pub fn new(cap: usize) -> Arc<Stream> {
        let st = StreamState { kept: Vec::new(), dropped: 0, live: None, live_from: 0, total: 0 };
        Arc::new(Stream { state: SpinLock::named("proc.run_stream", st), cap })
    }
    pub fn contents(&self) -> Vec<u8> {
        self.state.lock().kept.clone()
    }
    pub fn dropped(&self) -> u64 {
        self.state.lock().dropped
    }
    pub fn total(&self) -> u64 {
        self.state.lock().total
    }
    /// From now on writes are kept for progress too, starting with what was kept so far. `kept` is the stream's
    /// start (bytes 0..len); once anything was dropped it no longer reaches the end, so progress then starts
    /// empty at the end of the stream (the next block names the gap) instead of passing the start off as the tail.
    /// False when the heap has no room for the progress buffer (the answer at the end still comes).
    pub fn start_live(&self) -> bool {
        if self.state.lock().live.is_some() {
            return true;
        }
        // Reserved outside the lock: the program's writes do not wait for the heap.
        let mut q = VecDeque::new();
        if q.try_reserve_exact(LIVE_KEEP).is_err() {
            return false;
        }
        let mut g = self.state.lock();
        let g = &mut *g;
        if g.live.is_none() {
            if g.dropped == 0 {
                q.extend(&g.kept[g.kept.len().saturating_sub(LIVE_KEEP)..]);
            }
            g.live_from = g.total - q.len() as u64;
            g.live = Some(q);
        }
        true
    }
    /// Takes the unread progress (None when there is none). The copy's room is taken outside the lock; what
    /// came after that look stays for the next call.
    pub fn take_live(&self) -> Option<Live> {
        let n = {
            let g = self.state.lock();
            let l = g.live.as_ref()?;
            if l.is_empty() {
                return None;
            }
            l.len()
        };
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(n).ok()?;
        let mut g = self.state.lock();
        let g = &mut *g;
        let live = g.live.as_mut()?;
        let take = live.len().min(n);
        let from = g.live_from;
        bytes.extend(live.drain(..take));
        g.live_from = from + take as u64;
        (take > 0).then_some(Live { bytes, from, end: from + take as u64 })
    }
}

impl File for Stream {
    fn read_at(&self, _pos: u64, _buf: &mut [u8]) -> Result<usize, Errno> {
        Err(EBADF)
    }
    fn write_at(&self, _pos: u64, buf: &[u8]) -> Result<usize, Errno> {
        let mut g = self.state.lock();
        let g = &mut *g;
        g.total += buf.len() as u64;
        // Only while nothing was dropped: `kept` stays the stream's start without holes (a write the heap
        // refused once must not be followed by later bytes that look adjacent).
        let room = if g.dropped == 0 { self.cap.saturating_sub(g.kept.len()).min(buf.len()) } else { 0 };
        if room > 0 && g.kept.try_reserve(room).is_ok() {
            g.kept.extend_from_slice(&buf[..room]);
            g.dropped += (buf.len() - room) as u64;
        } else {
            g.dropped += buf.len() as u64;
        }
        if let Some(live) = g.live.as_mut() {
            // The newest LIVE_KEEP bytes stay; the oldest give way at the front (the gap shows in the next
            // progress). Within the room reserved in start_live: no allocation, nothing moved.
            let tail = &buf[buf.len().saturating_sub(LIVE_KEEP)..];
            let over = (live.len() + tail.len()).saturating_sub(LIVE_KEEP);
            live.drain(..over.min(live.len()));
            live.extend(tail);
            g.live_from = g.total - live.len() as u64;
        }
        Ok(buf.len())
    }
    fn mode(&self) -> u32 {
        S_IFIFO | 0o600
    }
    fn events(&self) -> Events {
        Events(Events::OUT)
    }
}

// ---------------------------------------------------------------------------------------------------------
// Texts.

/// `b` as text for the answer: invalid UTF-8 as U+FFFD, cut so its JSON form stays within `SHOW_MAX`.
fn shown(b: &[u8]) -> String {
    let t = String::from_utf8_lossy(b);
    let mut used = 0usize;
    for (i, c) in t.char_indices() {
        used += match c {
            '"' | '\\' | '\n' | '\r' | '\t' => 2,
            c if (c as u32) < 0x20 || c == '\u{7f}' => 6,
            c => c.len_utf8(),
        };
        if used > SHOW_MAX {
            let rest = t[i..].chars().count();
            return format!("{}\n[... {rest} more characters not shown: binary output takes more room as text; the answer stays below 4 MB]\n", &t[..i]);
        }
    }
    t.into_owned()
}

/// The program's working directory (FS's `FsState`), set before it runs: an existing directory of its view.
fn set_cwd(p: &crate::Process, dir: &str) -> Result<(), vfs::Errno> {
    let f = p.ns.open(dir)?;
    if f.mode() & 0o170000 != 0o040000 {
        return Err(uapi::errno::ENOTDIR);
    }
    p.fds.fs.set_cwd(p.ns.lookup_path(dir.as_bytes(), true)?);
    Ok(())
}

/// The answer text (pure): first line `pid <p> ended: exit <n>` or `... signal <s> (<NAME>), code <128+s>`,
/// then the fault message or the kill reason, then both streams.
pub(crate) fn answer(pid: u32, st: ExitStatus, why: Option<&str>, out: &[u8], out_dropped: u64, err: &[u8], err_dropped: u64) -> String {
    let mut s = format!("pid {pid} ended: {}", st.describe());
    if let ExitStatus::Signaled(_) = st {
        s.push_str(&format!(", code {}", st.shell_code()));
    }
    s.push('\n');
    if let Some(w) = why {
        s.push_str(w);
        s.push('\n');
    }
    push_streams(&mut s, out, out_dropped, err, err_dropped);
    s
}

/// Both streams as in the answer: `--- stdout (n bytes[, last k not kept]) ---`, the text, the same for stderr.
fn push_streams(s: &mut String, out: &[u8], out_dropped: u64, err: &[u8], err_dropped: u64) {
    for (name, b, dropped) in [("stdout", out, out_dropped), ("stderr", err, err_dropped)] {
        s.push_str(&format!("--- {name} ({} bytes{}) ---\n", b.len() as u64 + dropped, if dropped > 0 { format!(", last {dropped} not kept") } else { String::new() }));
        let t = shown(b);
        s.push_str(&t);
        if !t.is_empty() && !t.ends_with('\n') {
            s.push('\n');
        }
    }
}

/// The error text of a killed program whose thread has not ended: what happens next, and its output so far.
pub(crate) fn pending_text(pid: u32, next: &str, out: &Stream, err: &Stream) -> String {
    let mut s = format!("pid {pid}: killed, but its thread has not ended (it never left user mode); {next}\n");
    push_streams(&mut s, &out.contents(), out.dropped(), &err.contents(), err.dropped());
    s
}

/// One progress block: `--- stdout bytes 120..240 ---` (and the gap when older bytes gave way), then the text.
pub(crate) fn progress(name: &str, l: &Live, shown_until: u64) -> String {
    let mut s = format!("--- {name} bytes {}..{}", l.from, l.end);
    if l.from > shown_until {
        s.push_str(&format!(" ({} bytes before not shown)", l.from - shown_until));
    }
    s.push_str(" ---\n");
    s.push_str(&String::from_utf8_lossy(&l.bytes));
    if !s.ends_with('\n') {
        s.push('\n');
    }
    s
}

// ---------------------------------------------------------------------------------------------------------
// The tool.

/// `run` programs whose thread has not ended yet (before and past their deadline, and killed ones still being
/// retried). Every run event carries it, so a job nobody stops shows in the log; `sched` lists its thread under
/// the program's name. It drops only when the program's thread has ended (or, after a give-up without a thread
/// to wait on, never: then it is counted until a Reload).
static RUNNING: AtomicUsize = AtomicUsize::new(0);

/// One running `run` program in `RUNNING`, for as long as this lives.
pub(crate) struct Running(());

impl Running {
    pub(crate) fn new() -> Running {
        RUNNING.fetch_add(1, Ordering::Relaxed);
        Running(())
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        RUNNING.fetch_sub(1, Ordering::Relaxed);
    }
}

pub(crate) fn running() -> usize {
    RUNNING.load(Ordering::Relaxed)
}

/// What one job may hold at most besides its program: the kept start and the progress buffer of both streams.
const JOB_CAPTURE_MAX: u64 = 2 * (OUTPUT_CAP + LIVE_KEEP) as u64;

/// R1: no fixed job count. A new job is refused when free memory no longer covers the worst-case capture of
/// every running job and this one, plus 1/16 of all memory kept for the rest of the system (pure, host-tested).
pub(crate) fn room_for_a_job(free: u64, total: u64, running: usize) -> Result<(), String> {
    let need = (running as u64).saturating_add(1).saturating_mul(JOB_CAPTURE_MAX).saturating_add(total / 16);
    if free >= need {
        return Ok(());
    }
    Err(format!(
        "not started: {} MiB free is below the {} MiB needed (the output capture of {running} running jobs and this one, plus 1/16 of memory for the system); stop a job first (stop {{\"call\":n}})",
        free >> 20,
        need >> 20
    ))
}

/// A program that never makes a syscall sees a kill only at its next interrupt (the user-IRQ hook), and a CPU
/// running one user thread alone gets none (tickless, no LAPIC timer before CP-5). A do-nothing KernelRt thread
/// pinned to that CPU preempts it: the reschedule IPI is that interrupt. The CPU is read anew on every nudge:
/// a nudge that lands on a CPU the thread just left is harmless and the next one (200 ms later) follows it; a
/// thread still in user mode can only have moved by being preempted, which delivered the kill anyway.
/// `thread_stats` is the only per-thread CPU lookup sched offers; it runs at most 5 times a second per killed
/// job that has not ended. A stop-gap until `Process::kill` interrupts the thread's CPU itself (change request 1
/// in plan/notes/mc-MC-5a.md).
fn nudge(pid: u32, tid: u64) {
    let Some(t) = sched::thread_stats().into_iter().find(|t| t.id == tid) else {
        klog::event!(Debug, "proc", "run.nudge_no_thread", pid = pid, thread = tid; "its thread is not in the table any more (on its way out)");
        return;
    };
    let opts = sched::SpawnOpts { cpu: Some(t.cpu), pin: true, ..Default::default() };
    match sched::spawn_with("run-nudge", sched::Class::KernelRt, opts, || {}) {
        Ok(_) => klog::event!(Debug, "proc", "run.nudge", pid = pid, thread = tid, cpu = t.cpu, state = t.state),
        Err(e) => klog::event!(Warn, "proc", "run.nudge_failed", pid = pid, thread = tid, cpu = t.cpu, error = format!("{e:?}"); "the next nudge tries again"),
    }
}

/// Nudges killed `p` every `NUDGE_EVERY_NS` (bounded waits, B4) until its thread ended (true, event
/// `run.kill_late_end`) or `KILL_RETRY_NS` passed (false, Error event `run.kill_given_up`).
fn keep_nudging(p: &Process) -> bool {
    let start = time::now_ns();
    loop {
        if p.wait(Some(NUDGE_EVERY_NS)).is_some() {
            klog::event!(Info, "proc", "run.kill_late_end", pid = p.pid, ms = time::now_ns().saturating_sub(start) / 1_000_000);
            return true;
        }
        if time::now_ns().saturating_sub(start) > KILL_RETRY_NS {
            klog::event!(Error, "proc", "run.kill_given_up", pid = p.pid, thread = p.thread_id(), retried_s = KILL_RETRY_NS / 1_000_000_000, running = running();
                "killed, but its thread never left user mode; it stays counted as running until it ends or a Reload");
            return false;
        }
        nudge(p.pid, p.thread_id());
    }
}

/// How long the retry thread sleeps per look after giving up (it only waits, so the count stays exact).
const AFTER_GIVE_UP_NS: u64 = 10_000_000_000;

/// A killed program still in user mode when the tool has to answer: a thread of its own (`run-kill-retry`)
/// takes the job over, goes on nudging (`keep_nudging`) and, should that give up, waits on in bounded slices
/// until the thread really ended, so `RUNNING` drops only then. None when the thread took the job; the job back
/// when no thread could be started (the caller then nudges on itself).
pub(crate) fn retry_kill(p: Arc<Process>, job: Running) -> Option<Running> {
    let pid = p.pid;
    // The job goes through a slot: should the spawn fail, its closure (and the slot's other half) is dropped,
    // and the job comes back from here instead of being dropped with it.
    let slot = Arc::new(SpinLock::named("proc.run_retry_job", Some(job)));
    let theirs = slot.clone();
    let spawned = sched::spawn("run-kill-retry", sched::Class::Normal, move || {
        let _job = theirs.lock().take();
        if !keep_nudging(&p) {
            while p.wait(Some(AFTER_GIVE_UP_NS)).is_none() {}
            klog::event!(Info, "proc", "run.kill_late_end", pid = p.pid, after_give_up = true);
        }
    });
    match spawned {
        Ok(_) => None,
        Err(e) => {
            klog::event!(Warn, "proc", "run.kill_retry_no_thread", pid = pid, error = format!("{e:?}"); "the tool's worker goes on nudging itself");
            slot.lock().take()
        }
    }
}

/// B3 before anything starts: reading the program's file, or `McpAction("run.elf_b64")` for an image sent
/// along (the MCP layer already checked `McpAction("run")`).
pub(crate) fn admit(caller: &policy::Subject, r: &Request) -> Result<(), ToolError> {
    let access = match r.image {
        Some(_) => policy::Access::McpAction { tool: "run.elf_b64" },
        None => policy::Access::Path { path: &r.path, write: false, create: false },
    };
    policy::check(caller, &access).map_err(|d| {
        klog::event!(Info, "proc", "run.refused", path = r.path.as_str(), inline = r.image.is_some(), reason = d.reason.as_str());
        ToolError::Refused(d.reason)
    })?;
    // Memory for one more job (no frame allocator on the host: nothing to ask).
    if let Some(f) = mm_core::frames() {
        let st = f.stats();
        room_for_a_job(st.free_bytes, st.total_bytes, running()).map_err(|why| {
            klog::event!(Warn, "proc", "run.no_memory", path = r.path.as_str(), free_mb = st.free_bytes >> 20, running = running());
            ToolError::Failed(why)
        })?;
    }
    Ok(())
}

/// A bare name missing from /bin runs from the stick's program directory when it is there (UL-5).
fn resolve_bare(r: &mut Request) {
    if !r.bare || vfs::root_ns().lookup(&r.path).is_ok() {
        return;
    }
    let name = &r.path["/bin/".len()..];
    let stick = format!("{STICK_BIN}/{name}");
    if vfs::root_ns().lookup(&stick).is_ok() {
        r.path = stick;
    }
}

fn run(ctx: &ToolContext, args: &Json) -> ToolResult {
    let mut r = parse(args)?;
    resolve_bare(&mut r);
    admit(&ctx.caller, &r)?;
    let out = Stream::new(OUTPUT_CAP);
    let err = Stream::new(OUTPUT_CAP);
    let argv: Vec<&str> = r.argv.iter().map(String::as_str).collect();
    let envp: Vec<&str> = r.envp.iter().map(String::as_str).collect();
    let mut o = SpawnOptions::new(&argv, &envp, MCP_PROGRAM);
    o.stdin = Some(Input::new(r.stdin) as Arc<dyn File>);
    o.stdout = Some(out.clone() as Arc<dyn File>);
    o.stderr = Some(err.clone() as Arc<dyn File>);
    // Held until its working directory is set (FS-1's cwd), then released.
    o.held = true;
    let spawned = match &r.image {
        Some(image) => crate::spawn_image_with(&r.path, image, &o),
        None => crate::spawn_with(&r.path, &o),
    };
    let p = spawned.map_err(|e| ToolError::Failed(format!("{}: {e}", r.path)))?;
    if let Some(dir) = &r.cwd
        && let Err(e) = set_cwd(&p, dir)
    {
        p.kill(uapi::ab::signal::SIGKILL);
        let _ = p.wait(Some(KILL_WAIT_NS));
        let why = if e == uapi::errno::ENOTDIR { "not a directory" } else { "not found" };
        return Err(ToolError::BadArgument(format!("cwd {dir}: {why} (errno {e})")));
    }
    ctx.set_job_pid(p.pid);
    p.release();
    // Counted until the program's thread has ended: dropped below after the loop, or handed on with it.
    let mut job = Some(Running::new());
    klog::event!(Info, "proc", "run.start", pid = p.pid, path = r.path.as_str(), args = r.argv.len(), inline = r.image.is_some(), deadline_ms = ctx.remaining_ns() / 1_000_000, running = running());
    let start = ctx.now_ns();
    let mut why: Option<String> = None;
    let mut killed_at: Option<u64> = None;
    // `stop` sent SIGINT at this time (its grace runs; then SIGKILL).
    let mut interrupted_at: Option<u64> = None;
    let mut live = false;
    let mut shown_until = [0u64; 2];
    let mut nudged = 0u64;
    loop {
        let ended = p.wait(Some(SLICE_NS)).is_some();
        // Near the deadline: the output so far goes to the call's progress, so the "Still running" answer
        // carries it and `output` shows what comes later.
        if !ended && !live && ctx.remaining_ns() <= LEAD_NS {
            live = true;
            if !(out.start_live() & err.start_live()) {
                klog::event!(Warn, "proc", "run.progress_no_memory", pid = p.pid; "no room for the progress buffer: output shows up only in the final result");
            }
            let call = ctx.call_id().unwrap_or(0);
            ctx.output(&format!("pid {} {} is running (job = call {call}); its output so far:\n", p.pid, r.path));
            klog::event!(Info, "proc", "run.still_running", pid = p.pid, call = call, stdout = out.total(), stderr = err.total(), running = running());
        }
        if live {
            for (i, (name, s)) in [("stdout", &out), ("stderr", &err)].into_iter().enumerate() {
                if let Some(l) = s.take_live() {
                    ctx.output(&progress(name, &l, shown_until[i]));
                    shown_until[i] = l.end;
                }
            }
        }
        if ended {
            break;
        }
        let now = ctx.now_ns();
        if let Some(t) = killed_at {
            if now.saturating_sub(t) >= nudged * NUDGE_EVERY_NS + NUDGE_EVERY_NS {
                nudged += 1;
                nudge(p.pid, p.thread_id());
            }
            if now.saturating_sub(t) > KILL_WAIT_NS {
                // A busy program sees the kill only at its next interrupt or syscall; the answer cannot wait
                // for ever, the nudging goes on without it.
                let back = job.take().and_then(|j| retry_kill(p.clone(), j));
                klog::event!(Warn, "proc", "run.kill_pending", pid = p.pid, waited_ms = KILL_WAIT_NS / 1_000_000, retry_thread = back.is_none(), running = running();
                    "killed, but its thread has not left user mode");
                let Some(j) = back else {
                    let next = "after 5 s; a kernel thread goes on interrupting its CPU for up to 60 s (log: run.kill_late_end or run.kill_given_up)";
                    return Err(ToolError::Failed(pending_text(p.pid, next, &out, &err)));
                };
                // No thread for it: this worker goes on nudging (B4: bounded slices), and answers as usual if
                // that ends it.
                if keep_nudging(&p) {
                    job = Some(j);
                    break;
                }
                // Still counted (it may end yet; nothing is left to wait on it): until a Reload.
                core::mem::forget(j);
                let next = "after 65 s of nudging without a thread of its own; it stays counted as running until a Reload ends it";
                return Err(ToolError::Failed(pending_text(p.pid, next, &out, &err)));
            }
            continue;
        }
        if let Some(t) = interrupted_at {
            let grace = ctx.stop_grace_ns();
            if now.saturating_sub(t) >= grace {
                p.kill(uapi::ab::signal::SIGKILL);
                let w = format!("stopped by the stop tool (SIGINT, then SIGKILL after {} ms)", grace / 1_000_000);
                klog::event!(Info, "proc", "run.kill", pid = p.pid, reason = w.as_str());
                why = Some(w);
                killed_at = Some(now);
            }
            continue;
        }
        if ctx.stop_requested() && ctx.stop_grace_ns() > 0 {
            // spec/net.md §5.2: SIGINT first (from "root", SI_USER as kill(1) would send it); a program without
            // a handler ends at once, one with a handler gets the grace to end by itself.
            let info = crate::signal::SigInfo::user(uapi::ab::signal::SIGINT, uapi::ab::siginfo::SI_USER, 0, 0);
            let sent = p.send_info(info, crate::signal::To::Group, false).unwrap_or(crate::signal::Sent::Dropped);
            // A stopped job keeps SIGINT pending: continue it so it can take it (bash's `kill` of a stopped job).
            if sent != crate::signal::Sent::Dropped && p.signals.raw(|s| s.group_stopped()) {
                p.send_signal(uapi::ab::signal::SIGCONT);
            }
            klog::event!(Info, "proc", "run.interrupt", pid = p.pid, outcome = format!("{sent:?}").as_str(), grace_ms = ctx.stop_grace_ns() / 1_000_000);
            why = Some(String::from("stopped by the stop tool (SIGINT)"));
            if sent == crate::signal::Sent::Fatal {
                // Ending already: nudged like a kill (a thread in user mode sees it at its next interrupt).
                killed_at = Some(now);
            } else {
                interrupted_at = Some(now);
            }
            continue;
        }
        let reason = if ctx.stop_requested() {
            Some(String::from("stopped by the stop tool (SIGKILL)"))
        } else {
            r.kill_after_ns.filter(|k| now.saturating_sub(start) >= *k).map(|k| format!("killed after kill_after_ms={} (SIGKILL)", k / 1_000_000))
        };
        if let Some(w) = reason {
            p.kill(uapi::ab::signal::SIGKILL);
            klog::event!(Info, "proc", "run.kill", pid = p.pid, reason = w.as_str());
            why = Some(w);
            killed_at = Some(now);
        }
    }
    drop(job);
    let st = p.status().unwrap_or(ExitStatus::Exited(0));
    let why = p.end_message().or(why);
    klog::event!(Info, "proc", "run.end", pid = p.pid, status = st.describe().as_str(), stdout = out.total(), stderr = err.total(), ms = ctx.now_ns().saturating_sub(start) / 1_000_000, running = running());
    Ok(alloc::vec![ToolOutput::Text(answer(p.pid, st, why.as_deref(), &out.contents(), out.dropped(), &err.contents(), err.dropped()))])
}

mcp::mcp_tool!(
    name: "run",
    owner: "AB",
    safety: Change,
    description: "Starts an x86-64 Linux program (static, static-PIE or dynamic) and answers with how it ended (exit code, or signal and the V2 fault message), its stdout and its stderr (separately, each kept up to 1 MiB). path: absolute, or a bare name looked up in /bin, then /boot/BITSOS2/bin; or elf_b64: the program itself as base64 (path then only names it). argv: the whole argument vector (default [path]); or args: the arguments after the name. env: list of \"K=V\" or an object (default PATH=/bin:/sbin:/usr/bin:/boot/BITSOS2/bin, HOME=/, XDG_RUNTIME_DIR=/run/user/1000, LANG=de_DE.UTF-8). stdin: text, or stdin_b64: bytes as base64 (then end of file). cwd: its working directory, an absolute directory path (default /). kill_after_ms: end it after this long. timeout_ms (default 30000): when it passes the answer is 'Still running ... (call n)' with the pid and the output so far, and the program keeps running: output {\"call\":n} (or {\"pid\":p}) shows its new output and, once it ended, the result; stop {\"call\":n} kills it (SIGKILL), stop {\"pid\":p} sends SIGINT, then SIGKILL after a grace.",
    schema: r#"{"type":"object","properties":{"path":{"type":"string","description":"/bin/hello or hello"},"elf_b64":{"type":"string","description":"the ELF file as base64 instead of a file on BitsOS"},"argv":{"type":"array","items":{"type":"string"}},"args":{"type":"array","items":{"type":"string"}},"env":{"type":["array","object"]},"stdin":{"type":"string"},"stdin_b64":{"type":"string"},"cwd":{"type":"string"},"kill_after_ms":{"type":"integer","minimum":1},"timeout_ms":{"type":"integer","minimum":1,"default":30000,"description":"deadline of the answer; the program runs on after it"}}}"#,
    handler: run,
);

#[cfg(test)]
mod tests {
    use super::*;

    fn p(t: &str) -> Result<Request, ToolError> {
        parse(&Json::parse(t).unwrap())
    }

    #[test]
    fn arguments() {
        let r = p(r#"{"path":"hello"}"#).unwrap();
        assert_eq!(r.path, "/bin/hello");
        assert_eq!(r.argv, ["hello"]);
        assert_eq!(r.envp, ["PATH=/bin:/sbin:/usr/bin:/boot/BITSOS2/bin", "HOME=/", "XDG_RUNTIME_DIR=/run/user/1000", "LANG=de_DE.UTF-8"]);
        assert_eq!(r.image, None);
        let r = p(r#"{"path":"/bin/x","args":["-a","b"],"env":{"A":"1"},"stdin":"in","kill_after_ms":5}"#).unwrap();
        assert_eq!(r.argv, ["/bin/x", "-a", "b"]);
        assert_eq!(r.envp, ["A=1"]);
        assert_eq!(r.stdin, b"in");
        assert_eq!(r.kill_after_ns, Some(5_000_000));
        let r = p(r#"{"path":"/bin/x","argv":["x0","y"],"env":["K=V"],"cwd":"/"}"#).unwrap();
        assert_eq!(r.argv, ["x0", "y"]);
        assert_eq!(r.envp, ["K=V"]);
        assert_eq!(p(r#"{"path":"x","argv":[]}"#).unwrap().argv, ["x"]);
        // stdin as base64 (any bytes), an image sent along.
        assert_eq!(p(r#"{"path":"x","stdin_b64":"AP9h"}"#).unwrap().stdin, [0, 0xff, b'a']);
        let r = p(r#"{"elf_b64":"f0VMRg==","args":["1"]}"#).unwrap();
        assert_eq!(r.path, INLINE_NAME);
        assert_eq!(r.image.as_deref(), Some(&b"\x7fELF"[..]));
        assert_eq!(r.argv, [INLINE_NAME, "1"]);
        let r = p(r#"{"elf_b64":"f0VMRg==","path":"spin"}"#).unwrap();
        assert_eq!((r.path.as_str(), r.argv.as_slice()), ("/bin/spin", &[String::from("spin")][..]));
    }

    #[test]
    fn hostile_arguments_are_bad_arguments() {
        for t in [
            r#"{}"#,
            r#"{"path":""}"#,
            r#"{"path":5}"#,
            r#"{"path":"a\u0000b"}"#,
            r#"{"path":"x","argv":["a",3]}"#,
            r#"{"path":"x","argv":"a"}"#,
            r#"{"path":"x","argv":["a"],"args":["b"]}"#,
            r#"{"path":"x","args":["a\u0000"]}"#,
            r#"{"path":"x","env":{"A":1}}"#,
            r#"{"path":"x","env":{"A=B":"1"}}"#,
            r#"{"path":"x","env":5}"#,
            r#"{"path":"x","stdin":[1]}"#,
            r#"{"path":"x","cwd":"tmp"}"#,
            r#"{"path":"x","cwd":5}"#,
            r#"{"path":"x","kill_after_ms":0}"#,
            r#"{"path":"x","kill_after_ms":-1}"#,
            r#"{"path":"x","kill_after_ms":1e30}"#,
            r#"{"path":"x","stdin":"a","stdin_b64":"YQ=="}"#,
            r#"{"path":"x","stdin_b64":"not base64!"}"#,
            r#"{"path":"x","stdin_b64":5}"#,
            r#"{"elf_b64":""}"#,
            r#"{"elf_b64":"%%%%"}"#,
            r#"{"elf_b64":"QQ==","path":7}"#,
            r#"{"elf_b64":["QQ=="]}"#,
        ] {
            assert!(matches!(p(t), Err(ToolError::BadArgument(_))), "{t}");
        }
        let long = format!(r#"{{"path":"{}"}}"#, "a".repeat(5000));
        assert!(p(&long).is_err());
    }

    #[test]
    fn answer_text() {
        let a = answer(7, ExitStatus::Exited(0), None, b"hi\n", 0, b"", 0);
        assert_eq!(a, "pid 7 ended: exit 0\n--- stdout (3 bytes) ---\nhi\n--- stderr (0 bytes) ---\n");
        let a = answer(8, ExitStatus::Signaled(11), Some("Program x ... Page fault at 0x0: not mapped"), b"part", 5, b"e", 0);
        assert!(a.starts_with("pid 8 ended: signal 11 (SIGSEGV), code 139\nProgram x"));
        assert!(a.contains("--- stdout (9 bytes, last 5 not kept) ---\npart\n"));
    }

    #[test]
    fn binary_output_still_fits_into_4_mb() {
        // 1 MiB of control bytes per stream would be 6 MiB each as JSON.
        let junk: Vec<u8> = (0..OUTPUT_CAP).map(|i| (i % 32) as u8).collect();
        let a = answer(1, ExitStatus::Exited(0), None, &junk, 0, &junk, 0);
        let mut j = String::new();
        mcp::json::escape_into(&a, &mut j);
        assert!(j.len() < mcp::rpc::ANSWER_MAX - 64 * 1024, "{} bytes as JSON", j.len());
        assert!(a.contains("--- stderr (1048576 bytes) ---\n") && a.matches("more characters not shown").count() == 2);
        // Plain text is never cut.
        let text = alloc::vec![b'A'; OUTPUT_CAP];
        assert!(!answer(1, ExitStatus::Exited(0), None, &text, 0, b"", 0).contains("not shown"));
    }

    #[test]
    fn stream_keeps_the_start_and_streams_the_rest() {
        let s = Stream::new(10);
        assert_eq!(s.write_at(0, b"hello ").unwrap(), 6);
        assert!(s.take_live().is_none(), "no progress before the deadline");
        s.start_live();
        let l = s.take_live().unwrap();
        assert_eq!((l.bytes.as_slice(), l.from, l.end), (&b"hello "[..], 0, 6));
        assert!(s.take_live().is_none());
        assert_eq!(s.write_at(0, b"world!!").unwrap(), 7, "the program never sees a short write");
        assert_eq!((s.contents().as_slice(), s.dropped(), s.total()), (&b"hello worl"[..], 3, 13));
        let l = s.take_live().unwrap();
        assert_eq!((l.bytes.as_slice(), l.from, l.end), (&b"world!!"[..], 6, 13), "progress goes past the cap");
        assert_eq!(s.read_at(0, &mut [0u8; 4]), Err(EBADF));
        // More than LIVE_KEEP unread: the oldest give way and the block says so.
        let big = alloc::vec![b'x'; LIVE_KEEP + 100];
        s.write_at(0, b"abc").unwrap();
        s.write_at(0, &big).unwrap();
        let l = s.take_live().unwrap();
        assert_eq!((l.bytes.len(), l.from, l.end), (LIVE_KEEP, 13 + 103, 13 + 3 + big.len() as u64));
        let head = format!("--- stdout bytes 116..{} (103 bytes before not shown) ---\nxxx", l.end);
        assert!(progress("stdout", &l, 13).starts_with(&head));
    }

    #[test]
    fn progress_after_the_cap_starts_at_the_end_not_at_the_start() {
        // More than the cap written before the deadline: the kept start must not be labelled as the tail.
        let s = Stream::new(4);
        s.write_at(0, b"abcdefgh").unwrap();
        assert_eq!((s.contents().as_slice(), s.dropped(), s.total()), (&b"abcd"[..], 4, 8));
        s.start_live();
        assert!(s.take_live().is_none(), "nothing new yet");
        s.write_at(0, b"XY").unwrap();
        let l = s.take_live().unwrap();
        assert_eq!((l.bytes.as_slice(), l.from, l.end), (&b"XY"[..], 8, 10));
        assert!(progress("stdout", &l, 0).starts_with("--- stdout bytes 8..10 (8 bytes before not shown) ---\nXY"));
        assert_eq!(s.contents(), b"abcd", "the answer keeps the start");
        // Exactly the cap, nothing dropped: the kept bytes are the whole stream and go out as progress.
        let s = Stream::new(4);
        s.write_at(0, b"abcd").unwrap();
        s.start_live();
        let l = s.take_live().unwrap();
        assert_eq!((l.bytes.as_slice(), l.from, l.end), (&b"abcd"[..], 0, 4));
    }

    /// Denies everything for program 4242 only (installed once for this test binary; every other subject is
    /// allowed, as by the G0 stub).
    struct DenyTestProgram;
    const DENIED: ProgramId = ProgramId(4242);
    impl policy::Engine for DenyTestProgram {
        fn check(&self, s: &policy::Subject, a: &policy::Access<'_>) -> Result<(), policy::Denied> {
            if s.program == DENIED { Err(policy::Denied { reason: format!("test rule denies {a:?}") }) } else { Ok(()) }
        }
    }
    static DENY: DenyTestProgram = DenyTestProgram;

    #[test]
    fn policy_is_asked_before_anything_starts() {
        policy::set_engine(&DENY);
        let denied = policy::Subject { program: DENIED, pid: 9, uid: 1000 };
        let ctx = ToolContext { caller: denied, deadline_ns: u64::MAX };
        // A file: reading its path is asked. Refused (not Failed): nothing was looked up or spawned, since on
        // the host a spawn would fail with "Failed".
        match run(&ctx, &Json::parse(r#"{"path":"hello"}"#).unwrap()) {
            Err(ToolError::Refused(r)) => assert!(r.contains("Path") && r.contains("\"/bin/hello\"") && r.contains("write: false"), "{r}"),
            other => panic!("{other:?}"),
        }
        // An image sent along: the McpAction run.elf_b64 is asked.
        let img = progs::elf(progs::SPIN);
        let mut b = String::new();
        mcp::b64::encode_into(&img, &mut b);
        match run(&ctx, &Json::parse(&format!(r#"{{"elf_b64":"{b}"}}"#)).unwrap()) {
            Err(ToolError::Refused(r)) => assert!(r.contains("McpAction") && r.contains("run.elf_b64"), "{r}"),
            other => panic!("{other:?}"),
        }
        // Bad arguments are still bad arguments (checked first), and other callers are admitted.
        assert!(matches!(run(&ctx, &Json::parse("{}").unwrap()), Err(ToolError::BadArgument(_))));
        let r = parse(&Json::parse(r#"{"path":"hello"}"#).unwrap()).unwrap();
        assert_eq!(admit(&policy::Subject::KERNEL, &r), Ok(()));
        assert_eq!(admit(&policy::Subject { program: ProgramId(7), pid: 9, uid: 1000 }, &r), Ok(()));
    }

    #[test]
    fn progress_buffer_is_reserved_once_and_gives_way_at_the_front() {
        let s = Stream::new(8);
        s.write_at(0, b"0123").unwrap();
        assert!(s.start_live());
        let cap = s.state.lock().live.as_ref().unwrap().capacity();
        assert!(cap >= LIVE_KEEP);
        // Many writes past LIVE_KEEP unread: the room never grows (no allocation on the write path), the newest
        // LIVE_KEEP bytes stay, the offsets stay right.
        let chunk = alloc::vec![b'z'; 64 * 1024 + 3];
        for _ in 0..40 {
            s.write_at(0, &chunk).unwrap();
        }
        let total = 4 + 40 * chunk.len() as u64;
        {
            let g = s.state.lock();
            let l = g.live.as_ref().unwrap();
            assert_eq!((l.capacity(), l.len(), g.live_from), (cap, LIVE_KEEP, total - LIVE_KEEP as u64));
        }
        let l = s.take_live().unwrap();
        assert_eq!((l.bytes.len(), l.from, l.end), (LIVE_KEEP, total - LIVE_KEEP as u64, total));
        assert_eq!(s.state.lock().live.as_ref().unwrap().capacity(), cap, "taking keeps the room");
        // Then adjacent blocks again.
        s.write_at(0, b"ab").unwrap();
        let l = s.take_live().unwrap();
        assert_eq!((l.bytes.as_slice(), l.from, l.end), (&b"ab"[..], total, total + 2));
        assert!(s.start_live(), "a second start keeps the buffer");
        assert!(s.take_live().is_none());
    }

    #[test]
    fn room_for_a_job_scales_with_memory_not_a_count() {
        const G: u64 = 1 << 30;
        // 32 GB with 30 GB free: thousands of jobs before a refusal.
        assert!(room_for_a_job(30 * G, 32 * G, 0).is_ok());
        assert!(room_for_a_job(30 * G, 32 * G, 5000).is_ok());
        let max = ((30 * G - 2 * G) / JOB_CAPTURE_MAX) as usize;
        assert!(room_for_a_job(30 * G, 32 * G, max - 1).is_ok());
        let e = room_for_a_job(30 * G, 32 * G, max).unwrap_err();
        assert!(e.starts_with("not started: 30720 MiB free") && e.contains("stop a job first"), "{e}");
        // Little memory left: refused even with no job running; no overflow on absurd counts.
        assert!(room_for_a_job(10 << 20, 32 * G, 0).is_err());
        assert!(room_for_a_job(u64::MAX - 1, 64 * G, usize::MAX).is_err());
    }

    #[test]
    fn pending_text_carries_the_output_so_far() {
        let (out, err) = (Stream::new(16), Stream::new(16));
        out.write_at(0, b"partial").unwrap();
        err.write_at(0, b"oops\n").unwrap();
        let t = pending_text(42, "next steps", &out, &err);
        assert_eq!(t, "pid 42: killed, but its thread has not ended (it never left user mode); next steps\n--- stdout (7 bytes) ---\npartial\n--- stderr (5 bytes) ---\noops\n");
    }

    #[test]
    fn running_counts_jobs() {
        // No other host test gets as far as a spawn, so nobody else holds one.
        let before = running();
        let (a, b) = (Running::new(), Running::new());
        assert_eq!(running(), before + 2);
        drop((a, b));
        assert_eq!(running(), before);
    }

    #[test]
    fn registered_with_owner_safety_and_own_deadline() {
        let t = mcp::find("run").unwrap();
        assert_eq!((t.owner, t.safety), ("AB", mcp::Safety::Change));
        let s = Json::parse(t.input_schema).unwrap();
        let d = s.get("properties").and_then(|p| p.get("timeout_ms")).and_then(|t| t.get("default")).and_then(Json::as_u64);
        assert_eq!(d, Some(DEFAULT_DEADLINE_MS));
    }
}
