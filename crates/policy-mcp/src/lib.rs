//! The connection view for Georg and Claude (B3, PM-2): MCP tool `connections` and the console command of the
//! same name, over `policy::connections`. Per program (its process name, pid and ProgramId; the kernel's own
//! connections as "BitsOS kernel") each connection: live or ended, protocol, peer address and port, the name the
//! program resolved itself, bytes each way, first and last time (local clock time once the wall clock is set,
//! else seconds since boot).
//!
//! policy knows pids, not process names: this crate tells it the owner's names when a connection opens
//! (`policy::Owners`, PM-2c), and a query names what is still unnamed from processes still running (started
//! before the connection), so names stay after the process ends.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

mod ktests;

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::Write;
use ksync::SpinLock;
use mcp::{Json, ToolContext, ToolError, ToolOutput, ToolResult};
use policy::{ConnFilter, ConnInfo, ProgramId};

/// Connections listed when `max` is not given.
const DEFAULT_MAX: u64 = 200;
/// At most per answer (one line is about 150 bytes; MCP answers stay far below 4 MB).
pub const MAX_LINES: u64 = 5000;

/// The clock for times in the answer: monotonic now, and the wall clock's UTC ns plus the local offset.
#[derive(Clone, Copy)]
pub struct Clock {
    pub mono_ns: u64,
    pub wall_ns: Option<u64>,
    pub offset_min: i32,
}

impl Clock {
    pub fn now() -> Clock {
        Clock { mono_ns: time::now_ns(), wall_ns: time::wall::realtime_ns(), offset_min: time::wall::utc_offset_minutes() }
    }

    fn at(&self, t: u64) -> String {
        match self.wall_ns {
            Some(w) => {
                let wall = w.saturating_sub(self.mono_ns.saturating_sub(t)) / 1_000_000_000;
                let local = (wall as i64 + self.offset_min as i64 * 60).rem_euclid(86_400);
                format!("{:02}:{:02}:{:02}", local / 3600, local / 60 % 60, local % 60)
            }
            None => format!("t+{}s", t / 1_000_000_000),
        }
    }
}

/// Stores the program's names with connections of processes still running.
pub fn name_programs(v: &mut [ConnInfo]) {
    let late = core::mem::take(&mut *LATE.lock());
    drop(late); // outside the lock
    for c in v.iter_mut().filter(|c| c.program_name.is_none() && c.subject.pid != 0) {
        if let Some(p) = proc::find(c.subject.pid).filter(|p| p.start_ns <= c.first_ns) {
            c.set_program(program_name(&p.path, &p.name), &p.name);
        }
    }
}

/// The owner names policy stores with a connection when it opens (PM-2c): the closes of a program that has
/// ended (Brave's helpers, Cobalt) then still name it. The process that runs this code is usually the owner
/// (connect, send) and is found without the table; else the table (a peer's connection to a program's server,
/// accepted in the net poll). The table lock is a short one that takes nothing of net or policy under it.
struct Owners;

impl policy::Owners for Owners {
    fn names(&self, s: &policy::Subject, at_ns: u64) -> Option<(Arc<str>, Arc<str>)> {
        // The running image is held by its thread, so dropping our copy never ends it.
        if let Some(p) = proc::current().filter(|p| p.pid == s.pid && p.start_ns <= at_ns) {
            return Some(names_of(&p));
        }
        let p = proc::find(s.pid)?;
        let r = (p.start_ns <= at_ns).then(|| names_of(&p));
        late_drop(p);
        r
    }
}

fn names_of(p: &proc::Process) -> (Arc<str>, Arc<str>) {
    (Arc::from(program_name(&p.path, &p.name)), Arc::from(p.name.as_str()))
}

/// Processes reaped while `Owners` held them (the table's reference went meanwhile): their last drop (memory,
/// mount namespace, a file table still shared at its exit) must not run in the net path, which may hold the
/// card's lock. Dropped by the next query (`name_programs`, tool or console thread, no lock held).
static LATE: SpinLock<Vec<Arc<proc::Process>>> = SpinLock::named("pm.owners.late", Vec::new());

fn late_drop(p: Arc<proc::Process>) {
    // Finished: the parent may reap it while we hold it (then ours is the last reference), so it is kept here
    // whatever the count says now. A live one stays in the table (count 1 only when an execve replaced the
    // image during the lookup; an execve exactly between this check and our drop is not covered).
    if p.is_finished() || Arc::strong_count(&p) == 1 {
        klog::event!(Debug, "pm", "owners.late_drop", pid = p.pid);
        LATE.lock().push(p);
    }
}

static OWNERS: Owners = Owners;

fn install_owners() -> Result<(), &'static str> {
    policy::set_owners(&OWNERS);
    klog::event!(Info, "pm", "owners.installed");
    Ok(())
}
// Core: before any network or program starts, so every connection is named from its open on.
mcp::__kreg::initcall!(mcp::__kreg::Level::Core, "pm.owners", install_owners);

/// A process's name in the view: the file name of the executable it runs (`path`, resolved at execve as
/// /proc/<pid>/exe is), not its comm (`comm`, the file name execve was given, linux-6.6/fs/exec.c:1366
/// `__set_task_comm(me, kbasename(bprm->filename), true)`): Chromium's helpers exec /proc/self/exe and are all
/// `exe`. The comm when the path has no file name.
pub(crate) fn program_name<'a>(path: &'a str, comm: &'a str) -> &'a str {
    path.rsplit('/').next().filter(|b| !b.is_empty()).unwrap_or(comm)
}

fn bytes(n: u64) -> String {
    match n {
        0..1_000 => format!("{n} B"),
        1_000..1_000_000 => format!("{}.{} kB", n / 1_000, n / 100 % 10),
        1_000_000..1_000_000_000 => format!("{}.{} MB", n / 1_000_000, n / 100_000 % 10),
        _ => format!("{}.{} GB", n / 1_000_000_000, n / 100_000_000 % 10),
    }
}

/// The answer text: a head line, then per program a line and its connections (one per line, key=value).
pub fn render(v: &[ConnInfo], clock: &Clock, kept: (usize, usize), names: usize) -> String {
    let mut out = String::new();
    let _ = write!(out, "connections: {} live, {} ended (the newest {} ended ones are kept), {} names known; shown {}",
        kept.0, kept.1, policy::conn_keep(), names, v.len());
    let mut groups: BTreeMap<(u32, u32), Vec<&ConnInfo>> = BTreeMap::new();
    for c in v {
        groups.entry((c.subject.program.0, c.subject.pid)).or_default().push(c);
    }
    for ((program, pid), cs) in &groups {
        let who = match (pid, cs.iter().find(|c| c.program_name.is_some())) {
            (0, _) => String::from("BitsOS kernel"),
            (_, Some(c)) => match (c.program_name.as_deref().unwrap_or("-"), c.comm.as_deref()) {
                (n, Some(comm)) => format!("{n} pid={pid} comm={comm}"),
                (n, None) => format!("{n} pid={pid}"),
            },
            (_, None) => format!("pid={pid} (ended)"),
        };
        let (bin, bout) = cs.iter().fold((0u64, 0u64), |a, c| (a.0 + c.bytes_in, a.1 + c.bytes_out));
        let own = if *program == ProgramId::SYSTEM.0 { String::from("BitsOS's own") } else { format!("program={program}") };
        let _ = write!(out, "\n== {who} {own}: {} connections, in {}, out {}", cs.len(), bytes(bin), bytes(bout));
        for c in cs {
            let state = if c.live() { "live " } else { "ended" };
            let dir = if c.accepted { " from-peer" } else { "" };
            let _ = write!(out, "\n{state} {} {}:{} name={} in={} out={} first={} last={} local_port={}{dir}",
                policy::proto_name(c.proto), c.remote, c.remote_port, c.name.as_deref().unwrap_or("-"), c.bytes_in, c.bytes_out,
                clock.at(c.first_ns), clock.at(c.closed_ns.unwrap_or(c.last_ns)), c.local_port);
        }
    }
    if v.is_empty() {
        out.push_str("\n(no connections)");
    }
    out
}

fn bad(name: &str) -> ToolError {
    ToolError::BadArgument(String::from(name))
}

fn opt_u64(args: &Json, key: &str) -> Result<Option<u64>, ToolError> {
    match args.get(key) {
        None | Some(Json::Null) => Ok(None),
        Some(v) => v.as_u64().map(Some).ok_or_else(|| bad(key)),
    }
}

fn opt_bool(args: &Json, key: &str) -> Result<Option<bool>, ToolError> {
    match args.get(key) {
        None | Some(Json::Null) => Ok(None),
        Some(v) => v.as_bool().map(Some).ok_or_else(|| bad(key)),
    }
}

fn opt_str<'a>(args: &'a Json, key: &str) -> Result<Option<&'a str>, ToolError> {
    match args.get(key) {
        None | Some(Json::Null) => Ok(None),
        Some(v) => v.as_str().filter(|s| s.len() <= 255 && !s.chars().any(char::is_control)).map(Some).ok_or_else(|| bad(key)),
    }
}

/// The view as text for a filter (tool and console).
pub fn view(f: &ConnFilter<'_>, max: usize) -> String {
    // Process names are stored only here, for processes still running: every query names all their
    // connections (also the ended ones, so they keep the name after the process), then filters, as the text also
    // matches process names.
    let mut v = policy::connections(&ConnFilter { text: None, live_only: false, ..*f }, usize::MAX);
    name_programs(&mut v);
    v.retain(|c| (!f.live_only || c.live()) && f.text.is_none_or(|t| c.matches(t)));
    v.truncate(max);
    render(&v, &Clock::now(), policy::connection_counts(), policy::dns_names())
}

fn connections(_ctx: &ToolContext, args: &Json) -> ToolResult {
    if !matches!(args, Json::Obj(_) | Json::Null) {
        return Err(bad("arguments"));
    }
    let max = match opt_u64(args, "max")? {
        None => DEFAULT_MAX,
        Some(m) if (1..=MAX_LINES).contains(&m) => m,
        Some(_) => return Err(bad("max")),
    } as usize;
    let program = opt_u64(args, "program")?.map(|p| u32::try_from(p).map(ProgramId).map_err(|_| bad("program"))).transpose()?;
    let pid = opt_u64(args, "pid")?.map(|p| u32::try_from(p).map_err(|_| bad("pid"))).transpose()?;
    let bitsos = match opt_str(args, "who")? {
        None | Some("all") => None,
        Some("bitsos") => Some(true),
        Some("programs") => Some(false),
        Some(_) => return Err(bad("who")),
    };
    let f = ConnFilter { live_only: opt_bool(args, "live")?.unwrap_or(false), program, pid, bitsos, text: opt_str(args, "filter")? };
    Ok(alloc::vec![ToolOutput::Text(view(&f, max))])
}

mcp::mcp_tool!(
    name: "connections",
    owner: "PM",
    safety: Read,
    description: "The connection view (B3): per program every live and ended connection with peer address and port, the name the program itself resolved (from the DNS answers on its own sockets; '-' if it resolved none), bytes in and out, first and last time. 'live' = only open connections; 'filter' = part of a name, address:port or program name (the executable's file name, or the process's comm, shown as comm= when it differs); 'program'/'pid' = one program; 'who' = bitsos (BitsOS's own parts) or programs. Ended connections are kept up to a setting (oldest go first); each end is also a 'pm conn.closed' event in the log.",
    schema: r#"{"type":"object","properties":{"live":{"type":"boolean","description":"only open connections"},"filter":{"type":"string","maxLength":255,"description":"part of the name, address:port, program name or comm"},"program":{"type":"integer","minimum":0,"maximum":4294967295},"pid":{"type":"integer","minimum":0,"maximum":4294967295},"who":{"type":"string","enum":["all","bitsos","programs"]},"max":{"type":"integer","minimum":1,"maximum":5000,"description":"connections listed, default 200"}},"additionalProperties":false}"#,
    handler: connections,
);

/// `connections [live] [<filter>]`: the view on the console.
fn console_connections(c: &mut console::monitor::Ctx) -> Result<(), String> {
    let mut f = ConnFilter::default();
    let args: Vec<&str> = c.args.clone();
    for a in &args {
        match *a {
            "live" => f.live_only = true,
            t if f.text.is_none() => f.text = Some(t),
            _ => return Err(String::from("usage: connections [live] [name, address or program]")),
        }
    }
    for line in view(&f, DEFAULT_MAX as usize).lines() {
        c.println(line);
    }
    Ok(())
}
console::command!(name: "connections", args: "[live] [filter]", help: "which program talks to which server (names, bytes, times)", physical_only: false, run: console_connections);

/// Calls the tool as the MCP server would (tests).
pub fn call(args: &str) -> Result<String, ToolError> {
    let t = mcp::find("connections").ok_or_else(|| ToolError::Failed(String::from("connections tool not registered")))?;
    let args = Json::parse(args).map_err(|e| ToolError::Failed(format!("bad test JSON at {}", e.at)))?;
    let ctx = ToolContext { caller: policy::Subject::KERNEL, deadline_ns: time::now_ns().saturating_add(10_000_000_000) };
    match (t.handler)(&ctx, &args)?.into_iter().next() {
        Some(ToolOutput::Text(s)) => Ok(s),
        _ => Err(ToolError::Failed(String::from("no text answer"))),
    }
}
