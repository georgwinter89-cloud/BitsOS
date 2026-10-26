//! MCP tool `events` (owner RB, safety Read; RB-7a, B6): the kernel log and the per-CPU trace rings in one
//! time-ordered answer, filtered by time range, level, subsystem, name and fields.
//!
//! Answer: a head line `events from=<s> to=<s> shown=<n> more=<0|1> trace_overwritten=<n> suppressed=<n>`, then
//! one line per event: `#<seq> [time] L subsystem name fields | message` for log records (as `log` shows them),
//! `~<cpu>.<pos> [time] L subsystem name fields` for trace entries (context switches, IRQ storms, slow
//! syscalls, lock waits, page-fault bursts, marks). Without `from` the newest `max` events of the range, with
//! `from` the oldest. `more=1`: more events matched than shown (narrow the range or follow on with `from`).
//! A read never blocks: a busy log ring gives `busy`, a deadline reached gives `partial`.

use crate::tool::{bad, opt_level, opt_str, opt_u64};
use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;
use klog::trace::Entry;
use klog::{FieldFilter, Filter, Record};
use mcp::{Json, ToolContext, ToolError, ToolOutput, ToolResult};

const DEFAULT_MAX: usize = 200;
/// Log records copied per read while streaming (field filters): bounded memory per step.
const PAGE: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Source {
    All,
    Log,
    Trace,
}

pub(crate) struct Query<'a> {
    pub filter: Filter<'a>,
    pub fields: FieldFilter,
    pub from_ns: u64,
    pub to_ns: u64,
    /// Oldest first from `from_ns` (true) or the newest of the range (false).
    pub forward: bool,
    pub max: usize,
    pub source: Source,
}

/// One answer line with its sort key.
struct Line {
    time_ns: u64,
    /// 0 = log (by sequence), 1 = trace (by cpu, position).
    order: (u8, u64, u64),
    text: String,
}

#[derive(Default)]
struct Found {
    lines: Vec<Line>,
    more: bool,
    busy: bool,
    partial: bool,
}

pub(crate) fn secs(args: &Json, key: &str) -> Result<Option<u64>, ToolError> {
    match args.get(key) {
        None | Some(Json::Null) => Ok(None),
        Some(v) => match v.as_f64() {
            Some(s) if (0.0..1.8e10).contains(&s) => Ok(Some((s * 1e9) as u64)),
            _ => Err(bad(key)),
        },
    }
}

pub(crate) fn stamp(ns: u64) -> String {
    alloc::format!("{}.{:06}", ns / 1_000_000_000, (ns / 1000) % 1_000_000)
}

pub(crate) fn parse(args: &Json, now: u64) -> Result<Query<'_>, ToolError> {
    if !matches!(args, Json::Obj(_) | Json::Null) {
        return Err(bad("arguments"));
    }
    let max = match opt_u64(args, "max")? {
        None => DEFAULT_MAX,
        Some(m) if (1..=crate::tool::MAX_LINES).contains(&m) => m as usize,
        Some(_) => return Err(bad("max")),
    };
    let from = secs(args, "from")?;
    let last = secs(args, "last")?;
    if from.is_some() && last.is_some() {
        return Err(bad("last"));
    }
    let to_ns = secs(args, "to")?.unwrap_or(u64::MAX);
    let from_ns = from.or(last.map(|l| now.saturating_sub(l))).unwrap_or(0);
    if from_ns > to_ns {
        return Err(bad("to"));
    }
    let fields = match opt_str(args, "fields")? {
        None => FieldFilter::default(),
        Some(f) => FieldFilter::parse(f).map_err(|_| bad("fields"))?,
    };
    let source = match opt_str(args, "source")? {
        None | Some("all") => Source::All,
        Some("log") => Source::Log,
        Some("trace") => Source::Trace,
        Some(_) => return Err(bad("source")),
    };
    let filter = Filter { level: opt_level(args)?, subsystem: opt_str(args, "subsystem")?, name: opt_str(args, "name")? };
    Ok(Query { filter, fields, from_ns, to_ns, forward: from.is_some(), max, source })
}

fn log_line(r: &Record) -> Line {
    Line { time_ns: r.time_ns, order: (0, r.seq, 0), text: alloc::format!("#{} {}", r.seq, r.render()) }
}

/// Log records of the query: without field filters one bounded read; with them a stream through the range
/// that keeps only `max` matches (plus one to know about `more`).
fn from_log(q: &Query<'_>, deadline: u64, out: &mut Found) {
    if q.fields.is_empty() && !q.forward {
        let page = klog::read_tail_span(q.max, &q.filter, q.from_ns, q.to_ns);
        out.busy |= page.busy;
        out.lines.extend(page.records.iter().map(log_line));
        return;
    }
    let mut kept: VecDeque<Line> = VecDeque::new();
    let mut since = 0;
    loop {
        let page = klog::read_span(since, PAGE, &q.filter, q.from_ns, q.to_ns);
        out.busy |= page.busy;
        for r in page.records.iter().filter(|r| q.fields.matches(&r.text)) {
            kept.push_back(log_line(r));
            if kept.len() > q.max {
                out.more = true;
                if q.forward {
                    kept.pop_back();
                    out.lines.extend(kept);
                    return;
                }
                kept.pop_front();
            }
        }
        if page.records.len() < PAGE || page.busy || page.next <= since {
            break;
        }
        since = page.next;
        if time::now_ns() > deadline {
            out.partial = true;
            break;
        }
    }
    out.lines.extend(kept);
}

pub(crate) fn entry_line(e: &Entry) -> String {
    let mut s = String::new();
    let _ = write!(s, "~{}.{} [{:5}.{:06}] {} {} {} {}", e.cpu, e.pos, e.time_ns / 1_000_000_000, (e.time_ns / 1000) % 1_000_000,
        e.level().letter(), e.subsystem(), e.name(), e.text());
    s
}

/// Trace entries visited between two looks at the deadline.
const DEADLINE_EVERY: u32 = 1024;

/// Fixed-size text buffer for matching field filters without allocating per entry.
struct StackText {
    buf: [u8; 192],
    len: usize,
    full: bool,
}

impl Write for StackText {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let end = self.len + s.len();
        if end > self.buf.len() {
            self.full = true;
            return Err(core::fmt::Error);
        }
        self.buf[self.len..end].copy_from_slice(s.as_bytes());
        self.len = end;
        Ok(())
    }
}

fn fields_match(q: &Query<'_>, e: &Entry) -> bool {
    if q.fields.is_empty() {
        return true;
    }
    let mut t = StackText { buf: [0; 192], len: 0, full: false };
    if e.write_text(&mut t).is_ok() && !t.full {
        // Only `str` pieces were written, cut at no boundary.
        return core::str::from_utf8(&t.buf[..t.len]).is_ok_and(|s| q.fields.matches(s));
    }
    q.fields.matches(&e.text())
}

/// The kept candidates across all rings: at most `max`, the newest (backward) or oldest (forward) by time.
struct Best {
    heap: alloc::collections::BinaryHeap<Cand>,
    forward: bool,
    max: usize,
    /// A match was left out (pushed out or not taken).
    more: bool,
}

/// Ordered so that the heap's top is the candidate to drop first.
struct Cand {
    key: (u64, u32, u64),
    forward: bool,
    e: Entry,
}

impl PartialEq for Cand {
    fn eq(&self, o: &Self) -> bool {
        self.key == o.key
    }
}
impl Eq for Cand {}
impl PartialOrd for Cand {
    fn partial_cmp(&self, o: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for Cand {
    fn cmp(&self, o: &Self) -> core::cmp::Ordering {
        // Forward keeps the oldest: drop the newest first (max-heap). Backward keeps the newest: drop the oldest.
        if self.forward { self.key.cmp(&o.key) } else { o.key.cmp(&self.key) }
    }
}

impl Best {
    /// False when `e` would be dropped at once (so are all after it in this ring's scan order).
    fn offer(&mut self, e: &Entry) -> bool {
        let c = Cand { key: (e.time_ns, e.cpu, e.pos), forward: self.forward, e: *e };
        if self.heap.len() == self.max {
            let top = self.heap.peek().map(|t| t.key);
            let worse = match top {
                Some(t) if self.forward => c.key >= t,
                Some(t) => c.key <= t,
                None => false,
            };
            self.more = true;
            if worse {
                return false;
            }
            self.heap.pop();
        }
        self.heap.push(c);
        true
    }
}

/// Trace entries of the query: one candidate list of at most `max` across all rings (bounded memory however
/// many CPUs), the deadline checked every `DEADLINE_EVERY` entries, text rendered only for the kept ones.
fn from_trace(q: &Query<'_>, deadline: u64, out: &mut Found) {
    let mut best = Best { heap: alloc::collections::BinaryHeap::with_capacity(q.max.min(256)), forward: q.forward, max: q.max, more: false };
    let mut visited = 0u32;
    'rings: for r in klog::trace::rings() {
        let mut late = false;
        r.scan(!q.forward, |e| {
            visited = visited.wrapping_add(1);
            if visited % DEADLINE_EVERY == 0 && time::now_ns() > deadline {
                late = true;
                return false;
            }
            // Past the range in scan order (ring order is time order): the rest of this ring is outside too.
            if (q.forward && e.time_ns > q.to_ns) || (!q.forward && e.time_ns < q.from_ns) {
                return false;
            }
            if !(q.from_ns..=q.to_ns).contains(&e.time_ns) || !q.filter.accepts(e.level(), e.subsystem(), e.name()) {
                return true;
            }
            if !fields_match(q, e) {
                return true;
            }
            // Scan order is time order within a ring: once one is worse than the list, so is the rest.
            best.offer(e)
        });
        if late {
            out.partial = true;
            break 'rings;
        }
    }
    out.more |= best.more;
    out.lines.extend(best.heap.into_iter().map(|c| Line { time_ns: c.e.time_ns, order: (1, c.e.cpu as u64, c.e.pos), text: entry_line(&c.e) }));
}

fn run(q: &Query<'_>, deadline: u64) -> Found {
    let mut f = Found::default();
    if q.source != Source::Trace {
        from_log(q, deadline, &mut f);
    }
    if q.source != Source::Log {
        from_trace(q, deadline, &mut f);
    }
    f.lines.sort_by_key(|l| (l.time_ns, l.order));
    if f.lines.len() > q.max {
        f.more = true;
        if q.forward {
            f.lines.truncate(q.max);
        } else {
            f.lines.drain(..f.lines.len() - q.max);
        }
    }
    f
}

pub fn events(ctx: &ToolContext, args: &Json) -> ToolResult {
    if let Some(r) = crate::journal::reach_back(ctx, "events", args, events) {
        return r;
    }
    let now = klog::now();
    let q = parse(args, now)?;
    let found = run(&q, ctx.deadline_ns);
    let overwritten: u64 = klog::trace::rings().map(|r| r.overwritten()).sum();
    let mut out = String::new();
    let _ = write!(out, "events from={} to={} shown={} more={} trace_overwritten={} suppressed={}", stamp(q.from_ns),
        stamp(q.to_ns.min(now)), found.lines.len(), found.more as u8, overwritten, klog::ratelimit::suppressed());
    if found.busy {
        out.push_str(" busy (the log was held too long; some log records may be missing: call again)");
    }
    if found.partial {
        out.push_str(" partial (time ran out: narrow the range)");
    }
    for l in &found.lines {
        out.push('\n');
        out.push_str(&l.text);
    }
    Ok(alloc::vec![ToolOutput::Text(out)])
}

mcp::mcp_tool!(
    name: "events",
    owner: "RB",
    safety: Read,
    description: "Kernel events: the log and the per-CPU trace rings (context switches, IRQ storms, slow syscalls, lock waits, page-fault bursts) in time order. Lines '#seq [time] ...' are log records, '~cpu.pos [time] ...' trace entries. Range in seconds since boot (the [time] of the lines): from/to, or last (the last n seconds). Without from: the newest max events; with from: the oldest. more=1 in the head line: more matched. Filters: level (most verbose: error, warn, info, debug, trace), subsystem (with its dotted sub-subsystems), name (prefix), fields (space-separated terms that must all hold: key=value, key!=value, key>n, key<n, key>=n, key<=n, or a bare key), source (all, log, trace). Earlier boots and times before the rings: since/until (wall time, local: 'YYYY-MM-DD HH:MM', 'HH:MM' = the last 24 h, or unix seconds) instead of from/to/last answer from the journal (bitsjournal, weeks back).",
    schema: r#"{"type":"object","properties":{"since":{"type":"string","description":"wall time (local): 'YYYY-MM-DD HH:MM[:SS]', 'HH:MM' (the last 24 h) or unix seconds; answered from the journal (all boots, weeks back)"},"until":{"type":"string","description":"wall time like since (default now)"},"from":{"type":"number","minimum":0,"description":"seconds since boot"},"to":{"type":"number","minimum":0,"description":"seconds since boot"},"last":{"type":"number","minimum":0,"description":"the last n seconds (instead of from)"},"level":{"type":"string","enum":["error","warn","info","debug","trace"]},"subsystem":{"type":"string"},"name":{"type":"string","description":"prefix of the event name"},"fields":{"type":"string","description":"e.g. 'cpu=3 us>500'"},"source":{"type":"string","enum":["all","log","trace"]},"max":{"type":"integer","minimum":1,"maximum":2000,"description":"lines, default 200"}},"additionalProperties":false}"#,
    handler: events,
);

#[cfg(test)]
mod tests {
    use super::*;
    use klog::trace::Kind;

    fn args(s: &str) -> Json {
        Json::parse(s).unwrap()
    }

    #[test]
    fn arguments_parse_and_bad_ones_name_themselves() {
        let a = args(r#"{"from":1.5,"to":3,"level":"warn","subsystem":"net","fields":"cpu=3 us>500","max":5,"source":"trace"}"#);
        let q = parse(&a, 10_000_000_000).unwrap();
        assert_eq!((q.from_ns, q.to_ns, q.forward, q.max, q.source), (1_500_000_000, 3_000_000_000, true, 5, Source::Trace));
        assert!(q.fields.matches("cpu=3 us=900") && !q.fields.matches("cpu=3 us=10"));
        let a = args(r#"{"last":2}"#);
        let q = parse(&a, 10_000_000_000).unwrap();
        assert_eq!((q.from_ns, q.to_ns, q.forward), (8_000_000_000, u64::MAX, false));
        let e = |s: &str| parse(&args(s), 1).err();
        assert_eq!(e(r#"{"from":1,"last":2}"#), Some(bad("last")));
        assert_eq!(e(r#"{"from":5,"to":4}"#), Some(bad("to")));
        assert_eq!(e(r#"{"from":-1}"#), Some(bad("from")));
        assert_eq!(e(r#"{"fields":"a>b"}"#), Some(bad("fields")));
        assert_eq!(e(r#"{"source":"x"}"#), Some(bad("source")));
        assert_eq!(e(r#"{"max":2001}"#), Some(bad("max")));
        assert_eq!(e(r#"{"level":"loud"}"#), Some(bad("level")));
        assert_eq!(e("[1]"), Some(bad("arguments")));
        assert!(e("null").is_none());
    }

    #[test]
    fn a_trace_entry_renders_as_one_line() {
        let e = Entry { cpu: 3, pos: 9, time_ns: 2_000_123_000, kind: Kind::Irq, thread: 0, name_id: 0, a: 65, b: 120_000 };
        assert_eq!(entry_line(&e), "~3.9 [    2.000123] W irq trace.irq_storm vector=65 per_s=120000 cpu=3 thread=0");
    }
}
