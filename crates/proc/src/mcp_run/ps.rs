//! MC-5b: the `ps` and `kill` MCP tools, and the process back end of `output`/`stop` by pid (spec/net.md §5.2:
//! V1's `tasks`/`stop_task` become `ps` (pid, threads, core, state, CPU time) and `kill` (pid, signal)).
//!
//! `ps` reads the process table (`crate::processes`, zombies included) and sched's thread table once: state
//! letters as Linux `task_state` (linux-6.6/fs/proc/array.c:130: R, S, T, Z, X; sched knows no uninterruptible
//! sleep, so there is no D), CPU time from SC-6 (`Group::cpu_times`, per thread `ThreadStat`), memory from MM-5b
//! (resident and committed pages of the address space, the system's commit total and limit), the command line.
//! Kernel threads have no process: they are counted (listed with `kernel: true`) and never signalled.
//!
//! `kill` follows kill(2) (`kill_pid_info`: a thread id names its whole process; signal 0 probes; 1..=64) with
//! the rights of root (MCP acts as BitsOS itself, so AB-6's uid rule always allows it), B3's
//! `Access::Signal` per target, and two MCP rules: PID 1 only with `confirm: true` (then SIGKILL and SIGSTOP
//! reach it as from the kernel, `force`; other signals still need a handler of init's, as in Linux), kernel
//! threads never. The siginfo is SI_USER with pid 0 and uid 0 (as from a root process outside every namespace).

use super::{NUDGE_EVERY_NS, nudge};
use crate::Process;
use crate::signal::{Sent, SigInfo, To};
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::Write;
use mcp::{Json, StopTarget, ToolContext, ToolError, ToolOutput, ToolResult};
use uapi::ab::signal::{SIGCONT, SIGINT, SIGKILL, SIGSTOP, SIGTSTP, SIGTTIN, SIGTTOU};
use uapi::ab::signal_names;

const KIB_PER_PAGE: u64 = hal::PAGE_SIZE as u64 / 1024;
/// A command line is shown up to this many bytes (a table row, not a dump; `read_file /proc/<pid>/cmdline` has
/// all of it).
const CMDLINE_SHOWN: usize = 1024;
/// `kill` waits this long by default for the signal's effect (end, stop, continue).
const DEFAULT_WAIT_MS: u64 = 1_000;
/// The longest `wait_ms` (B4: a bounded wait in a worker).
const MAX_WAIT_MS: u64 = 10_000;
/// After `stop`'s SIGKILL, how long it waits for the end before it answers.
const AFTER_KILL_NS: u64 = 500_000_000;
const SLICE_NS: u64 = 20_000_000;

// ---------------------------------------------------------------------------------------------------------
// State letters and rows.

/// sched's state of one kernel thread as Linux's letter.
fn letter(stat: Option<&sched::ThreadStat>) -> char {
    match stat.map(|s| s.state) {
        Some("running" | "runnable") => 'R',
        Some("sleeping") => 'S',
        // An ended thread still listed is a zombie thread (Linux keeps an exited leader as Z until the group
        // ends); X only for one sched no longer knows.
        Some("dead") => 'Z',
        _ => 'X',
    }
}

/// A process's state as `/proc/<pid>/stat` shows it (the leader's, linux-6.6/fs/proc/array.c `do_task_stat`):
/// Z once every thread ended (until reaped), T in a group stop, else the leader thread's R/S; a group whose
/// leader called pthread_exit shows Z as in Linux (its leader is a zombie until the group ends).
pub(crate) fn process_state(p: &Process, stats: &BTreeMap<u64, &sched::ThreadStat>) -> char {
    if p.is_finished() {
        return 'Z';
    }
    if p.signals.raw(|s| s.group_stopped()) {
        return 'T';
    }
    match p.threads().iter().find(|t| t.tid() == p.pid) {
        Some(t) => letter(stats.get(&t.kthread()).copied()),
        None => 'Z',
    }
}

fn secs(ns: u64) -> String {
    format!("{}.{:03}", ns / 1_000_000_000, ns % 1_000_000_000 / 1_000_000)
}

/// The command line (argv joined by spaces; `[name]` without one), control characters as `?` (procps), cut.
fn cmdline(p: &Process) -> String {
    let mut s = if p.argv.is_empty() { format!("[{}]", p.name) } else { p.argv.join(" ") };
    s = s.chars().map(|c| if c.is_control() { '?' } else { c }).collect();
    if s.len() > CMDLINE_SHOWN {
        let mut cut = CMDLINE_SHOWN;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
        s.push_str("...");
    }
    s
}

const HEADER: &str = "  PID  PPID  PGID   SID  UID S  THR CORE     TIME     USER      SYS    RSS_KB COMMIT_KB COMMAND";

/// One process row of the table.
fn row(p: &Process, stats: &BTreeMap<u64, &sched::ThreadStat>) -> String {
    let st = process_state(p, stats);
    let ended = p.is_finished();
    let ct = p.cpu_times();
    let core = (!ended).then(|| stats.get(&p.thread_id()).map(|s| s.cpu)).flatten();
    let (rss, commit) = if ended {
        (String::from("-"), String::from("-"))
    } else {
        (format!("{}", p.mm.rss_pages() * KIB_PER_PAGE), format!("{}", p.mm.committed_pages() * KIB_PER_PAGE))
    };
    format!(
        "{:>5} {:>5} {:>5} {:>5} {:>4} {st} {:>4} {:>4} {:>8} {:>8} {:>8} {:>9} {:>9} {}",
        p.pid,
        p.ppid(),
        p.pgid(),
        p.sid(),
        p.cred().euid,
        p.thread_count(),
        core.map_or(String::from("-"), |c| format!("{c}")),
        secs(ct.user_ns + ct.system_ns),
        secs(ct.user_ns),
        secs(ct.system_ns),
        rss,
        commit,
        cmdline(p)
    )
}

/// The thread lines below a process row (key=value, one per thread).
fn thread_rows(p: &Process, stats: &BTreeMap<u64, &sched::ThreadStat>, out: &mut String) {
    let stopped = p.signals.raw(|s| s.group_stopped());
    let mut threads = p.threads();
    threads.sort_by_key(|t| t.tid());
    for t in threads {
        let s = stats.get(&t.kthread()).copied();
        let st = if stopped { 'T' } else { letter(s) };
        let _ = write!(out, "      tid={} state={st}", t.tid());
        match s {
            Some(s) => {
                let _ = writeln!(
                    out,
                    " core={} time={} user={} sys={} vcsw={} ivcsw={} class={:?} kthread={}",
                    s.cpu,
                    secs(s.runtime_ns),
                    secs(s.user_ns),
                    secs(s.system_ns),
                    s.nvcsw,
                    s.nivcsw,
                    s.class,
                    s.id
                );
            }
            None => out.push_str(" (its kernel thread is not in the table: ending)\n"),
        }
    }
}

/// The process a pid names: its own, or the process of the thread with that id (kill(2)'s `kill_pid_info`).
fn process_of(pid: u32) -> Option<Arc<Process>> {
    crate::find(pid).or_else(|| crate::find_thread(pid).and_then(|t| t.group().image()))
}

/// A pid that names no process: ESRCH. Kernel threads have no pid (sched's ids are another number space), so a
/// pid never names one: they are never signalled, and `ps {"kernel": true}` lists them.
fn no_process(pid: u32) -> ToolError {
    ToolError::Failed(format!("no process {pid} (ESRCH)"))
}

fn stats_map(all: &[sched::ThreadStat]) -> BTreeMap<u64, &sched::ThreadStat> {
    all.iter().map(|s| (s.id, s)).collect()
}

/// One row for a process (with its threads), used by `output` for a pid no call runs.
fn describe(p: &Process) -> String {
    let all = sched::thread_stats();
    let stats = stats_map(&all);
    let mut s = format!("{HEADER}\n{}\n", row(p, &stats));
    thread_rows(p, &stats, &mut s);
    s
}

// ---------------------------------------------------------------------------------------------------------
// ps

fn bool_arg(args: &Json, key: &str) -> Result<Option<bool>, ToolError> {
    match args.get(key) {
        None | Some(Json::Null) => Ok(None),
        Some(v) => v.as_bool().map(Some).ok_or_else(|| ToolError::BadArgument(String::from(key))),
    }
}

fn id_arg(args: &Json, key: &str) -> Result<Option<u32>, ToolError> {
    match args.get(key) {
        None | Some(Json::Null) => Ok(None),
        Some(v) => v.as_u64().filter(|n| (1..=u64::from(u32::MAX)).contains(n)).map(|n| Some(n as u32)).ok_or_else(|| ToolError::BadArgument(String::from(key))),
    }
}

fn ps(_ctx: &ToolContext, args: &Json) -> ToolResult {
    let pid = id_arg(args, "pid")?;
    let threads = bool_arg(args, "threads")?.unwrap_or(pid.is_some());
    let kernel = bool_arg(args, "kernel")?.unwrap_or(false);
    let all = sched::thread_stats();
    let stats = stats_map(&all);
    let mut procs = match pid {
        Some(p) => alloc::vec![process_of(p).ok_or_else(|| no_process(p))?],
        None => crate::processes(),
    };
    procs.sort_by_key(|p| p.pid);
    // Kernel threads: every sched thread that runs no process's thread.
    let user: alloc::collections::BTreeSet<u64> = crate::processes().iter().flat_map(|p| p.threads().into_iter().map(|t| t.kthread())).collect();
    let kthreads: Vec<&sched::ThreadStat> = all.iter().filter(|s| !user.contains(&s.id) && s.state != "dead").collect();
    let mut out = String::new();
    if pid.is_none() {
        let zombies = procs.iter().filter(|p| p.is_finished()).count();
        let stopped = procs.iter().filter(|p| !p.is_finished() && p.signals.raw(|s| s.group_stopped())).count();
        let nthreads: usize = procs.iter().map(|p| p.thread_count()).sum();
        let _ = writeln!(
            out,
            "processes {} (zombie {zombies}, stopped {stopped}), their threads {nthreads}, kernel threads {}; committed {} KiB of the limit {} KiB",
            procs.len(),
            kthreads.len(),
            mm::commit::committed() * KIB_PER_PAGE,
            mm::commit::limit() * KIB_PER_PAGE
        );
    }
    out.push_str(HEADER);
    out.push('\n');
    for p in &procs {
        out.push_str(&row(p, &stats));
        out.push('\n');
        if threads {
            thread_rows(p, &stats, &mut out);
        }
    }
    if kernel {
        out.push_str("kernel threads (never signalled):\n");
        for s in kthreads {
            let _ = writeln!(out, "      kthread={} state={} core={} time={} class={:?} name={}", s.id, letter(Some(s)), s.cpu, secs(s.runtime_ns), s.class, s.name);
        }
    }
    Ok(alloc::vec![ToolOutput::Text(out)])
}

mcp::mcp_tool!(
    name: "ps",
    owner: "MC",
    safety: Read,
    description: "Lists the processes (zombies too): pid, ppid, pgid, sid, uid, state (R running or runnable, S sleeping, T stopped, Z ended and not yet reaped), thread count, the CPU core it last ran on, CPU time (total, user, system, in seconds), resident and committed memory (KiB) and the command line. The first line has the counts and the system's committed memory and its limit. pid: only that process (a thread id names its process) with its threads; threads: true lists every process's threads (tid, state, core, CPU time, context switches, class); kernel: true also lists the kernel threads.",
    schema: r#"{"type":"object","properties":{"pid":{"type":"integer","minimum":1},"threads":{"type":"boolean"},"kernel":{"type":"boolean"}}}"#,
    handler: ps,
);

// ---------------------------------------------------------------------------------------------------------
// kill

/// glibc's SIGRTMIN and SIGRTMAX (32 and 33 are NPTL's own), as kill(1) and procps name real-time signals.
const SIGRTMIN: u32 = 34;
const SIGRTMAX: u32 = 64;

/// A signal by number (0..=64, 0 probes) or name (`SIGTERM`, `TERM`, `term`, `SIGRTMIN+3`, `RTMAX-1`).
pub(crate) fn parse_signal(v: &Json) -> Result<u32, ToolError> {
    let bad = || ToolError::BadArgument(String::from("signal (0..=64 or a name such as SIGTERM or SIGRTMIN+1)"));
    if let Some(n) = v.as_u64() {
        return if n <= 64 { Ok(n as u32) } else { Err(bad()) };
    }
    let s = v.as_str().ok_or_else(bad)?;
    let up = s.to_ascii_uppercase();
    let name = if up.starts_with("SIG") { up } else { format!("SIG{up}") };
    let rt = |rest: &str, base: u32, up: bool| -> Option<u32> {
        let off: u32 = match rest {
            "" => 0,
            r if r.starts_with(if up { '+' } else { '-' }) => r[1..].parse().ok()?,
            _ => return None,
        };
        let n = if up { base.checked_add(off)? } else { base.checked_sub(off)? };
        (SIGRTMIN..=SIGRTMAX).contains(&n).then_some(n)
    };
    if let Some(rest) = name.strip_prefix("SIGRTMIN") {
        return rt(rest, SIGRTMIN, true).ok_or_else(bad);
    }
    if let Some(rest) = name.strip_prefix("SIGRTMAX") {
        return rt(rest, SIGRTMAX, false).ok_or_else(bad);
    }
    (1..=31).find(|&n| signal_names::name(n) == name).ok_or_else(bad)
}

/// A signal's name for answers and events: SIGTERM, SIGRTMIN+1, or "signal 33" for the two below SIGRTMIN.
fn shown(sig: u32) -> String {
    match sig {
        0 => String::from("signal 0"),
        1..=31 => String::from(signal_names::name(sig)),
        SIGRTMIN => String::from("SIGRTMIN"),
        n if n > SIGRTMIN && n <= SIGRTMAX => format!("SIGRTMIN+{}", n - SIGRTMIN),
        n => format!("signal {n}"),
    }
}

/// The rule for PID 1 (init) and B3, before anything is sent. `Refused` names why.
fn may_send(ctx_caller: &policy::Subject, p: &Process, sig: u32, confirm: bool) -> Result<(), ToolError> {
    if p.pid == 1 && sig != 0 && !confirm {
        let name = shown(sig);
        klog::event!(Warn, "proc", "mcp.kill_refused", pid = 1, signal = name.as_str(), reason = "init without confirm");
        return Err(ToolError::Refused(format!("PID 1 (init) gets {name} only with confirm: true")));
    }
    policy::check(ctx_caller, &policy::Access::Signal { target_pid: p.pid, signal: sig }).map_err(|d| {
        klog::event!(Info, "proc", "mcp.kill_refused", pid = p.pid, signal = sig, reason = d.reason.as_str());
        ToolError::Refused(d.reason)
    })
}

/// Sends `sig` to `p` as root (SI_USER, pid 0, uid 0); PID 1 (already confirmed) gets SIGKILL/SIGSTOP forced.
fn send(p: &Process, sig: u32) -> Sent {
    let force = p.pid == 1 && (sig == SIGKILL || sig == SIGSTOP);
    let info = SigInfo::user(sig, uapi::ab::siginfo::SI_USER, 0, 0);
    let sent = p.send_info(info, To::Group, force).unwrap_or(Sent::Dropped);
    klog::event!(Info, "proc", "mcp.kill", pid = p.pid, signal = shown(sig).as_str(), outcome = format!("{sent:?}").as_str());
    sent
}

/// Waits (bounded, B4) until `done(p)` for every process, nudging the CPU of one that is ending but still in
/// user mode (MC-5a's `nudge`). True when all are done.
fn settle(ps: &[Arc<Process>], ns: u64, done: impl Fn(&Process) -> bool) -> bool {
    let start = time::now_ns();
    let mut nudged = 0u64;
    loop {
        let left: Vec<&Arc<Process>> = ps.iter().filter(|p| !done(p)).collect();
        if left.is_empty() {
            return true;
        }
        let gone = time::now_ns().saturating_sub(start);
        if gone >= ns {
            return false;
        }
        let _ = left[0].wait(Some(SLICE_NS.min(ns - gone)));
        if time::now_ns().saturating_sub(start) >= (nudged + 1) * NUDGE_EVERY_NS {
            nudged += 1;
            for p in left.iter().filter(|p| p.status().is_some() && !p.is_finished()) {
                nudge(p.pid, p.thread_id());
            }
        }
    }
}

/// Whether a pending SIGTSTP/SIGTTIN/SIGTTOU will stop `p`: its default action and not blocked by every thread
/// (a handler or a mask keeps it pending, and `kill` should not wait the whole `wait_ms` for a stop).
fn stops_by_default(p: &Process, sig: u32) -> bool {
    p.signals.raw(|s| {
        s.action(sig).handler == uapi::ab::sigabi::SIG_DFL && s.thread_keys().iter().any(|&k| s.blocked_of(k) & crate::signal::bit(sig) == 0)
    })
}

/// What a signal's effect is, to wait for: the end, a stop, a continue (None: nothing to see). `may_stop`: a
/// pending job-control stop signal will stop the group (`stops_by_default`); SIGSTOP always does.
fn effect(sig: u32, sent: Sent, may_stop: bool) -> Option<fn(&Process) -> bool> {
    fn ended(p: &Process) -> bool {
        p.is_finished()
    }
    fn stopped(p: &Process) -> bool {
        p.is_finished() || p.signals.raw(|s| s.group_stopped())
    }
    fn running(p: &Process) -> bool {
        p.is_finished() || !p.signals.raw(|s| s.group_stopped())
    }
    match (sig, sent) {
        (_, Sent::Fatal) | (SIGKILL, _) => Some(ended),
        (SIGSTOP, Sent::Pending) => Some(stopped),
        (SIGTSTP | SIGTTIN | SIGTTOU, Sent::Pending) if may_stop => Some(stopped),
        (SIGCONT, _) => Some(running),
        _ => None,
    }
}

fn outcome_text(sig: u32, sent: Sent) -> &'static str {
    match sent {
        _ if sig == SIGCONT => "continues it if it was stopped (and runs a handler if it has one)",
        Sent::Fatal => "its default action ends it",
        Sent::Pending => "pending (it has a handler, blocks it, or it is a stop signal)",
        Sent::Dropped => "ignored (SIG_IGN, ignored by default, or init without a handler for it)",
    }
}

fn after(p: &Process) -> String {
    match (p.is_finished(), p.status()) {
        (true, Some(st)) => format!("ended: {}", st.describe()),
        _ if p.signals.raw(|s| s.group_stopped()) => String::from("stopped (T)"),
        (false, Some(st)) => format!("ending ({}), a thread is still on its way out", st.describe()),
        _ => String::from("running"),
    }
}

fn kill(ctx: &ToolContext, args: &Json) -> ToolResult {
    let pid = id_arg(args, "pid")?;
    let pgid = id_arg(args, "pgid")?;
    let sig = match args.get("signal") {
        None | Some(Json::Null) => uapi::ab::signal::SIGTERM,
        Some(v) => parse_signal(v)?,
    };
    let confirm = bool_arg(args, "confirm")?.unwrap_or(false);
    let wait_ms = match args.get("wait_ms") {
        None | Some(Json::Null) => DEFAULT_WAIT_MS,
        Some(v) => v.as_u64().filter(|w| *w <= MAX_WAIT_MS).ok_or_else(|| ToolError::BadArgument(format!("wait_ms (0..={MAX_WAIT_MS})")))?,
    };
    let name = shown(sig);
    let targets: Vec<Arc<Process>> = match (pid, pgid) {
        (Some(p), None) => alloc::vec![process_of(p).ok_or_else(|| no_process(p))?],
        (None, Some(g)) => {
            let m: Vec<Arc<Process>> = crate::processes().into_iter().filter(|p| p.pgid() == g).collect();
            if m.is_empty() {
                return Err(ToolError::Failed(format!("no process group {g} (ESRCH)")));
            }
            m
        }
        _ => return Err(ToolError::BadArgument(String::from("pid or pgid: give one of them"))),
    };
    let mut text = String::new();
    let mut sent_to = Vec::new();
    let mut refused = Vec::new();
    for p in &targets {
        if p.is_finished() {
            let _ = writeln!(text, "pid {} ({}): already ended, a zombie until pid {} reaps it ({}); nothing sent", p.pid, p.name, p.ppid(), after(p));
            continue;
        }
        if let Err(e) = may_send(&ctx.caller, p, sig, confirm) {
            let ToolError::Refused(why) = &e else { return Err(e) };
            let _ = writeln!(text, "pid {} ({}): refused: {why}", p.pid, p.name);
            refused.push(e);
            continue;
        }
        if sig == 0 {
            let _ = writeln!(text, "pid {} ({}): exists, {}", p.pid, p.name, after(p));
            continue;
        }
        let sent = send(p, sig);
        sent_to.push((p.clone(), sent));
    }
    // One target refused and nothing else: the call itself is refused.
    if targets.len() == 1 && refused.len() == 1 {
        return Err(refused.pop().unwrap_or(ToolError::Refused(String::new())));
    }
    // Wait for what the signal does (bounded), all targets together.
    let waiting: Vec<(Arc<Process>, fn(&Process) -> bool)> =
        sent_to.iter().filter_map(|(p, s)| effect(sig, *s, matches!(sig, SIGTSTP | SIGTTIN | SIGTTOU) && stops_by_default(p, sig)).map(|f| (p.clone(), f))).collect();
    if !waiting.is_empty() && wait_ms > 0 {
        let ps: Vec<Arc<Process>> = waiting.iter().map(|(p, _)| p.clone()).collect();
        let fs: Vec<fn(&Process) -> bool> = waiting.iter().map(|(_, f)| *f).collect();
        settle(&ps, wait_ms * 1_000_000, |p| ps.iter().position(|q| core::ptr::eq(&**q, p)).is_none_or(|i| fs[i](p)));
    }
    for (p, s) in &sent_to {
        let _ = writeln!(text, "pid {} ({}): sent {name}: {}; now {}", p.pid, p.name, outcome_text(sig, *s), after(p));
    }
    Ok(alloc::vec![ToolOutput::Text(String::from(text.trim_end()))])
}

mcp::mcp_tool!(
    name: "kill",
    owner: "MC",
    safety: Change,
    description: "Sends a signal to a process (pid; a thread id names its process) or to every process of a process group (pgid, e.g. a console job), as root. signal: a number 0..64 (0 only checks the process exists) or a name (SIGTERM, TERM, SIGRTMIN+1; default SIGTERM). Waits up to wait_ms (default 1000) for its effect (the end, a stop, a continue) and answers per process how the signal was taken (ends it, pending for a handler, ignored) and its state after. PID 1 (init) only with confirm: true (then SIGKILL reaches it); kernel threads are never signalled. A run program killed here ends its run call as usual.",
    schema: r#"{"type":"object","properties":{"pid":{"type":"integer","minimum":1},"pgid":{"type":"integer","minimum":1},"signal":{"type":["integer","string"],"default":"SIGTERM"},"wait_ms":{"type":"integer","minimum":0,"maximum":10000,"default":1000},"confirm":{"type":"boolean","description":"needed to signal PID 1 (init)"}}}"#,
    handler: kill,
);

// ---------------------------------------------------------------------------------------------------------
// output/stop by pid for a process no MCP call runs (mcp's `ProcessTools`).

fn output_of(pid: u32) -> ToolResult {
    let p = process_of(pid).ok_or_else(|| no_process(pid))?;
    let mut s = format!(
        "pid {} {} was not started by a run call of this server: its output goes to its own stdout and stderr (a terminal, a pipe or a file), not here; screen shows the console\n",
        p.pid, p.path
    );
    s.push_str(&describe(&p));
    Ok(alloc::vec![ToolOutput::Text(String::from(s.trim_end()))])
}

/// `stop {"pid"|"pgid"}`: SIGINT to each, `grace_ns` for them to end, SIGKILL to the rest, then a short wait.
/// Runs in the MCP connection's thread: every wait is bounded (grace at most mcp's MAX_GRACE_MS).
fn stop_of(target: StopTarget, grace_ns: u64, confirm: bool) -> ToolResult {
    let caller = policy::Subject::KERNEL;
    let (what, members): (String, Vec<Arc<Process>>) = match target {
        StopTarget::Pid(p) => (format!("pid {p}"), alloc::vec![process_of(p).ok_or_else(|| no_process(p))?]),
        StopTarget::Pgid(g) => {
            let m: Vec<Arc<Process>> = crate::processes().into_iter().filter(|p| p.pgid() == g && !p.is_finished()).collect();
            if m.is_empty() {
                return Err(ToolError::Failed(format!("no live process in process group {g} (ESRCH)")));
            }
            (format!("process group {g}"), m)
        }
    };
    let mut text = String::new();
    let mut live = Vec::new();
    for p in &members {
        if p.is_finished() {
            let _ = writeln!(text, "pid {} ({}): already ended ({})", p.pid, p.name, after(p));
            continue;
        }
        // Checked for SIGKILL, the strongest signal stop may send (B3, and the PID 1 rule).
        if let Err(e) = may_send(&caller, p, SIGKILL, confirm) {
            if members.len() == 1 {
                return Err(e);
            }
            let ToolError::Refused(why) = e else { return Err(e) };
            let _ = writeln!(text, "pid {} ({}): refused: {why}", p.pid, p.name);
            continue;
        }
        live.push(p.clone());
    }
    klog::event!(Info, "proc", "mcp.stop", target = what.as_str(), processes = live.len(), grace_ms = grace_ns / 1_000_000);
    let mut how: Vec<&'static str> = alloc::vec![""; live.len()];
    if grace_ns > 0 {
        for (i, p) in live.iter().enumerate() {
            let sent = send(p, SIGINT);
            // A stopped job keeps SIGINT pending (Linux too): continue it so it can take it, as bash's `kill`
            // of a stopped job does (SIGCONT after the signal).
            if sent != Sent::Dropped && p.signals.raw(|s| s.group_stopped()) {
                send(p, SIGCONT);
            }
            how[i] = "SIGINT";
        }
        settle(&live, grace_ns, |p| p.is_finished());
    }
    for (i, p) in live.iter().enumerate() {
        if p.status().is_none() {
            // Linux's SIGKILL to init needs the force; the confirm was checked above.
            send(p, SIGKILL);
            how[i] = if grace_ns > 0 { "SIGINT, then SIGKILL after the grace" } else { "SIGKILL" };
        }
    }
    settle(&live, AFTER_KILL_NS, |p| p.is_finished());
    for (i, p) in live.iter().enumerate() {
        let _ = writeln!(text, "pid {} ({}): stopped by the stop tool ({}); {}", p.pid, p.name, how[i], after(p));
    }
    Ok(alloc::vec![ToolOutput::Text(String::from(text.trim_end()))])
}

fn install() -> Result<(), &'static str> {
    mcp::set_process_tools(mcp::ProcessTools { output: output_of, stop: stop_of });
    // MC-6: a service whose process ended is dropped even while a forked child still holds its socket.
    mcp::svc::set_pid_alive(|pid| crate::find(pid).is_some_and(|p| !p.is_finished()));
    Ok(())
}
kreg::initcall!(kreg::Level::Service, "proc.mcp_process_tools", install);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signals_by_number_and_name() {
        assert_eq!(parse_signal(&Json::Num(0.0)), Ok(0));
        assert_eq!(parse_signal(&Json::Num(64.0)), Ok(64));
        assert!(parse_signal(&Json::Num(65.0)).is_err());
        assert!(parse_signal(&Json::Num(-1.0)).is_err());
        assert_eq!(parse_signal(&Json::Str(String::from("SIGTERM"))), Ok(15));
        assert_eq!(parse_signal(&Json::Str(String::from("kill"))), Ok(9));
        assert_eq!(parse_signal(&Json::Str(String::from("Int"))), Ok(2));
        assert!(parse_signal(&Json::Str(String::from("SIGNOPE"))).is_err());
        assert!(parse_signal(&Json::Bool(true)).is_err());
        assert_eq!(parse_signal(&Json::Str(String::from("SIGRTMIN"))), Ok(34));
        assert_eq!(parse_signal(&Json::Str(String::from("rtmin+3"))), Ok(37));
        assert_eq!(parse_signal(&Json::Str(String::from("SIGRTMAX"))), Ok(64));
        assert_eq!(parse_signal(&Json::Str(String::from("SIGRTMAX-30"))), Ok(34));
        for bad in ["SIGRTMIN+31", "SIGRTMAX-31", "SIGRTMIN-1", "SIGRTMAX+1", "SIGRTMIN+", "SIGRTMINX"] {
            assert!(parse_signal(&Json::Str(String::from(bad))).is_err(), "{bad}");
        }
    }

    #[test]
    fn signal_names_shown() {
        assert_eq!(shown(0), "signal 0");
        assert_eq!(shown(15), "SIGTERM");
        assert_eq!(shown(33), "signal 33");
        assert_eq!(shown(34), "SIGRTMIN");
        assert_eq!(shown(35), "SIGRTMIN+1");
        assert_eq!(shown(64), "SIGRTMIN+30");
    }

    #[test]
    fn effects_to_wait_for() {
        assert!(effect(15, Sent::Fatal, false).is_some());
        assert!(effect(SIGKILL, Sent::Dropped, false).is_some());
        assert!(effect(SIGSTOP, Sent::Pending, false).is_some());
        assert!(effect(SIGTSTP, Sent::Pending, true).is_some());
        // A handler or a mask keeps SIGTSTP pending: nothing to wait for.
        assert!(effect(SIGTSTP, Sent::Pending, false).is_none());
        assert!(effect(SIGCONT, Sent::Dropped, false).is_some());
        assert!(effect(10, Sent::Pending, false).is_none());
    }

    #[test]
    fn registered_with_owner_and_safety() {
        let ps = mcp::find("ps").expect("ps registered");
        assert_eq!((ps.owner, ps.safety), ("MC", mcp::Safety::Read));
        let k = mcp::find("kill").expect("kill registered");
        assert_eq!((k.owner, k.safety), ("MC", mcp::Safety::Change));
    }
}
