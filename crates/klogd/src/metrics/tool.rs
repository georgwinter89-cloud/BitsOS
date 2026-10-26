//! MCP tool `metrics` (owner RB, safety Read; RB-7b, B6): the sampler's series over a time range.
//!
//! Answer: a head line `metrics from=<s> to=<s> period_s=2 rounds=<n> points=<n> series=<n> more=<0|1>
//! history_from=<s>`, a line `time <t> ...` (end of each point's span, seconds since boot as in `[time]` of
//! the log), then one line per series `<name> <unit> <v> ...` with `-` where it had no value. A point covers
//! several rounds when the range has more rounds than `max`: the mean, or with `agg` the min, max or last
//! value (totals, unit `n`, default to the last).

use super::store::{Store, Unit};
use crate::events::{secs, stamp};
use crate::tool::{bad, opt_str, opt_u64};
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;
use mcp::{Json, ToolContext, ToolError, ToolOutput, ToolResult};

const DEFAULT_MAX: usize = 30;
const DEFAULT_LAST_NS: u64 = 60_000_000_000;
/// Values copied out of the store per answer at most (the copy is 16 bytes a value).
pub const MAX_VALUES: usize = 40_000;
/// Text per answer at most: a series line that would cross it is left out and `more=1` says so.
pub const MAX_ANSWER_BYTES: usize = 256 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Agg {
    Avg,
    Min,
    Max,
    Last,
}

#[derive(Debug)]
pub struct Query {
    /// A series is shown when its name contains one of these (all when empty).
    pub names: Vec<String>,
    pub from_ns: u64,
    pub to_ns: u64,
    pub max: usize,
    pub agg: Option<Agg>,
}

pub fn parse(args: &Json, now: u64) -> Result<Query, ToolError> {
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
    let to_ns = secs(args, "to")?.unwrap_or(now);
    let from_ns = from.unwrap_or_else(|| to_ns.saturating_sub(last.unwrap_or(DEFAULT_LAST_NS)));
    if from_ns > to_ns {
        return Err(bad("to"));
    }
    let names = opt_str(args, "name")?.map_or_else(Vec::new, |n| n.split(',').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect());
    let agg = match opt_str(args, "agg")? {
        None => None,
        Some("avg") => Some(Agg::Avg),
        Some("min") => Some(Agg::Min),
        Some("max") => Some(Agg::Max),
        Some("last") => Some(Agg::Last),
        Some(_) => return Err(bad("agg")),
    };
    Ok(Query { names, from_ns, to_ns, max, agg })
}

/// What an answer shows, copied out of the store (so the store's lock is not held while text is written).
#[derive(Debug, Default)]
pub struct Table {
    pub rounds: u64,
    /// End time of each point.
    pub times: Vec<u64>,
    pub series: Vec<(String, Unit, Vec<Option<u64>>)>,
    pub more: bool,
    pub history_from: Option<u64>,
}

fn fold(vals: impl Iterator<Item = u64>, agg: Agg) -> Option<u64> {
    let (mut n, mut sum, mut lo, mut hi, mut last) = (0u128, 0u128, u64::MAX, 0u64, None);
    for v in vals {
        (n, sum, lo, hi, last) = (n + 1, sum + v as u128, lo.min(v), hi.max(v), Some(v));
    }
    last?;
    Some(match agg {
        Agg::Avg => (sum / n) as u64,
        Agg::Min => lo,
        Agg::Max => hi,
        Agg::Last => last.unwrap_or(0),
    })
}

pub fn collect(s: &Store, q: &Query) -> Table {
    let r = s.range(q.from_ns, q.to_ns);
    let n = r.end - r.start;
    let points = (n as usize).min(q.max);
    // Point i covers rounds start + i*n/points .. start + (i+1)*n/points.
    let span = |i: usize| (r.start + i as u64 * n / points as u64)..(r.start + (i as u64 + 1) * n / points as u64);
    let mut t = Table { rounds: n, history_from: s.time(s.rounds().start), ..Table::default() };
    t.times = (0..points).filter_map(|i| s.time(span(i).end - 1)).collect();
    for (name, series) in s.series() {
        if !q.names.is_empty() && !q.names.iter().any(|w| name.contains(w.as_str())) {
            continue;
        }
        if (t.series.len() + 1) * points.max(1) > MAX_VALUES {
            t.more = true;
            break;
        }
        let agg = q.agg.unwrap_or(if series.unit.is_total() { Agg::Last } else { Agg::Avg });
        let vals = (0..points).map(|i| fold(span(i).filter_map(|round| series.at(round)), agg)).collect();
        t.series.push((String::from(name), series.unit, vals));
    }
    t
}

pub fn render(t: &Table, q: &Query, now: u64) -> String {
    // Lines first, so the head can say how many series fit under `MAX_ANSWER_BYTES`.
    let mut body = String::from("\ntime");
    for &tm in &t.times {
        let _ = write!(body, " {}.{:03}", tm / 1_000_000_000, (tm / 1_000_000) % 1000);
    }
    let (mut shown, mut more) = (0, t.more);
    let mut line = String::new();
    for (name, unit, vals) in &t.series {
        line.clear();
        let _ = write!(line, "\n{name} {}", unit.label());
        for v in vals {
            line.push(' ');
            match v {
                Some(v) => unit.write(*v, &mut line),
                None => line.push('-'),
            }
        }
        // 200 bytes stay free for the head line.
        if body.len() + line.len() + 200 > MAX_ANSWER_BYTES {
            more = true;
            break;
        }
        body.push_str(&line);
        shown += 1;
    }
    let mut out = String::with_capacity(body.len() + 200);
    let _ = write!(out, "metrics from={} to={} period_s={} rounds={} points={} series={} more={} history_from={}", stamp(q.from_ns),
        stamp(q.to_ns.min(now)), super::PERIOD_NS / 1_000_000_000, t.rounds, t.times.len(), shown, more as u8,
        t.history_from.map_or_else(|| String::from("-"), stamp));
    out.push_str(&body);
    out
}

/// No store: `metrics=off`, or the sampler did not start.
fn off_answer() -> ToolResult {
    Ok(alloc::vec![ToolOutput::Text(String::from("metrics off (CONFIG metrics=off, or the sampler did not start: see log subsystem=klog name=metrics.)"))])
}

pub fn metrics(ctx: &ToolContext, args: &Json) -> ToolResult {
    if let Some(r) = crate::journal::reach_back(ctx, "metrics", args, metrics) {
        return r;
    }
    let now = klog::now();
    let q = parse(args, now)?;
    let wait = ctx.deadline_ns.saturating_sub(time::now_ns()).min(1_000_000_000);
    let table = match super::STORE.lock_timeout(wait) {
        Some(g) => match g.as_ref() {
            Some(s) => collect(s, &q),
            None => return off_answer(),
        },
        None => return Err(ToolError::Failed(String::from("metrics store busy: call again"))),
    };
    Ok(alloc::vec![ToolOutput::Text(render(&table, &q, now))])
}

mcp::mcp_tool!(
    name: "metrics",
    owner: "RB",
    safety: Read,
    description: "Machine metrics sampled every 2 s, as series over a time range: cpu.<n>|all.busy/user/system (%), proc.<pid>.<name>.cpu (% of one CPU, programs that used CPU), mem.total/free/used.<kind> (B), net.<card>.rx_bytes/tx_bytes (B/s), rx_frames/tx_frames/rx_dropped/tx_dropped (/s), disk.<name>.requests/dispatches (/s), queued/errors/timeouts (n), temp.tctl/ccd<i> (C), power.pkg (W), metrics.cost (% of one CPU the sampler takes), and every kernel counter (totals, n). Range in seconds since boot (the [time] of the log): from/to, or last (default the last 60 s). name: comma-separated parts of series names (e.g. 'cpu.all,temp,igc'). max: points per series (default 30); a point covers several rounds then, agg says how (avg, min, max, last; totals default to last). Lines: 'time t...', then '<series> <unit> v...' with '-' for no value. Earlier boots and times before the rings: since/until (wall time, local: 'YYYY-MM-DD HH:MM', 'HH:MM' = the last 24 h, or unix seconds) instead of from/to/last answer from the journal (bitsjournal, weeks back).",
    schema: r#"{"type":"object","properties":{"since":{"type":"string","description":"wall time (local): 'YYYY-MM-DD HH:MM[:SS]', 'HH:MM' (the last 24 h) or unix seconds; answered from the journal (all boots, weeks back)"},"until":{"type":"string","description":"wall time like since (default now)"},"name":{"type":"string","description":"comma-separated parts of series names"},"from":{"type":"number","minimum":0,"description":"seconds since boot"},"to":{"type":"number","minimum":0,"description":"seconds since boot"},"last":{"type":"number","minimum":0,"description":"the last n seconds (instead of from), default 60"},"max":{"type":"integer","minimum":1,"maximum":2000,"description":"points per series, default 30"},"agg":{"type":"string","enum":["avg","min","max","last"]}},"additionalProperties":false}"#,
    handler: metrics,
);

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Json {
        Json::parse(s).unwrap()
    }

    #[test]
    fn arguments_parse_and_bad_ones_name_themselves() {
        let q = parse(&args(r#"{"name":"cpu.all, temp","from":2,"to":10.5,"max":4,"agg":"max"}"#), 99_000_000_000).unwrap();
        assert_eq!((q.names.len(), q.from_ns, q.to_ns, q.max, q.agg), (2, 2_000_000_000, 10_500_000_000, 4, Some(Agg::Max)));
        let q = parse(&Json::Null, 99_000_000_000).unwrap();
        assert_eq!((q.from_ns, q.to_ns, q.max, q.agg, q.names.is_empty()), (39_000_000_000, 99_000_000_000, 30, None, true));
        let e = |s: &str| parse(&args(s), 1).err();
        assert_eq!(e(r#"{"from":1,"last":2}"#), Some(bad("last")));
        assert_eq!(e(r#"{"from":5,"to":4}"#), Some(bad("to")));
        assert_eq!(e(r#"{"max":0}"#), Some(bad("max")));
        assert_eq!(e(r#"{"agg":"sum"}"#), Some(bad("agg")));
        assert_eq!(e(r#"{"name":3}"#), Some(bad("name")));
        assert_eq!(e("[1]"), Some(bad("arguments")));
    }

    #[test]
    fn points_fold_rounds_and_the_answer_has_its_shape() {
        let mut s = Store::new(1 << 20);
        for r in 0..6u64 {
            s.begin((r + 1) * 2_000_000_000);
            s.put("cpu.all.busy", Unit::Pct, r * 100);
            s.put("klog.x", Unit::Count, r);
            if r != 1 {
                s.put("temp.tctl", Unit::Celsius, 50_000 + r * 125);
            }
            s.end();
        }
        let q = parse(&args(r#"{"from":0,"to":12,"max":3,"name":"cpu,tctl,klog"}"#), 12_000_000_000).unwrap();
        let t = collect(&s, &q);
        let text = render(&t, &q, 12_000_000_000);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "metrics from=0.000000 to=12.000000 period_s=2 rounds=6 points=3 series=3 more=0 history_from=2.000000");
        assert_eq!(lines[1], "time 4.000 8.000 12.000");
        assert_eq!(lines[2], "cpu.all.busy % 0.5 2.5 4.5", "mean of two rounds each");
        assert_eq!(lines[3], "klog.x n 1 3 5", "totals show the last value");
        assert_eq!(lines[4], "temp.tctl C 50 50.312 50.562", "round 1 had no value: the point is round 0 alone");
        let q = parse(&args(r#"{"from":13,"name":"cpu"}"#), 20_000_000_000).unwrap();
        assert_eq!(render(&collect(&s, &q), &q, 20_000_000_000).lines().nth(2), Some("cpu.all.busy %"), "a range without rounds");
    }

    #[test]
    fn hostile_arguments_never_panic() {
        let now = 50_000_000_000;
        for (a, want) in [
            (r#"{"from":5,"to":4}"#, Some("to")),
            (r#"{"last":2,"from":1}"#, Some("last")),
            (r#"{"from":-1}"#, Some("from")),
            (r#"{"to":1e300}"#, Some("to")),
            (r#"{"last":-5}"#, Some("last")),
            (r#"{"last":"x"}"#, Some("last")),
            (r#"{"max":0}"#, Some("max")),
            (r#"{"max":2001}"#, Some("max")),
            (r#"{"max":-1}"#, Some("max")),
            (r#"{"max":1.5}"#, Some("max")),
            (r#"{"agg":""}"#, Some("agg")),
            (r#"{"name":["cpu"]}"#, Some("name")),
            ("\"cpu\"", Some("arguments")),
            ("17", Some("arguments")),
            (r#"{"max":2000}"#, None),
            (r#"{"last":1e9}"#, None),
            (r#"{"name":",,, ,"}"#, None),
            (r#"{"from":0,"to":0}"#, None),
        ] {
            let got = parse(&args(a), now);
            match want {
                Some(name) => assert_eq!(got.err(), Some(bad(name)), "{a}"),
                None => assert!(got.is_ok(), "{a}: {got:?}"),
            }
        }
        // NaN cannot be written in JSON; it reaches `secs` only as a number that fails its range check.
        let q = parse(&args(r#"{"name":",,, ,"}"#), now).unwrap();
        assert!(q.names.is_empty(), "only commas: every series");
        let q = parse(&args(r#"{"last":1e9}"#), now).unwrap();
        assert_eq!((q.from_ns, q.to_ns), (0, now), "a huge `last` starts at boot");
    }

    #[test]
    fn the_largest_answer_stays_under_the_cap() {
        let mut s = Store::new(64 << 20);
        for r in 0..2100u64 {
            s.begin((r + 1) * 2_000_000_000);
            for i in 0..100 {
                s.put(&alloc::format!("net.card{i}.rx_bytes"), Unit::BytesPerS, u64::MAX - 2 - r);
            }
            s.end();
        }
        let now = 2101 * 2_000_000_000;
        let q = parse(&args(r#"{"max":2000,"last":1e9}"#), now).unwrap();
        let t = collect(&s, &q);
        let text = render(&t, &q, now);
        assert!(text.len() <= MAX_ANSWER_BYTES, "{} bytes", text.len());
        let head = text.lines().next().unwrap();
        assert!(head.contains(" points=2000 ") && head.contains(" more=1 "), "{head}");
        let shown: usize = head.split(" series=").nth(1).unwrap().split(' ').next().unwrap().parse().unwrap();
        assert_eq!(text.lines().count(), shown + 2, "the head counts the lines that fit");
        assert!(shown >= 1);
    }

    #[test]
    fn metrics_off_answers_in_words() {
        assert!(matches!(off_answer(), Ok(ref v) if matches!(v.first(), Some(ToolOutput::Text(t)) if t.starts_with("metrics off"))));
    }
}
