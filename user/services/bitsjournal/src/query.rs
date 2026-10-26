//! Queries on the journal: events (log records and trace entries) and metrics by wall time, boot, level,
//! subsystem (with its dotted sub-subsystems), name (prefix) and fields, in the answer shape of the kernel's
//! MCP `events` and `metrics` (crates/klogd/src/events.rs, metrics/tool.rs) with local wall times in front.
//! Reads only blocks whose time range overlaps the query and stops early when `max` is reached.

use crate::fields::FieldFilter;
use crate::line::{self, Item};
use crate::store::{self, BlockHead};
use crate::{local_time, store::Segment};
use mcpreg::Json;
use std::collections::{BTreeMap, VecDeque};
use std::path::Path;

pub const DEFAULT_MAX: usize = 200;
pub const MAX_LINES: usize = 2000;
const MAX_SERIES: usize = 200;
/// A record read this long after its time still sits in an earlier segment's range (late reads after a
/// service restart): segments starting later than `until` + this are not read.
const LATE_S: u64 = 86_400;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Events,
    Metrics,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    All,
    Log,
    Trace,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Agg {
    Avg,
    Min,
    Max,
    Last,
}

#[derive(Clone, Debug)]
pub struct Query {
    pub kind: Kind,
    pub since_ms: u64,
    pub until_ms: u64,
    pub level: Option<u8>,
    pub subsystem: Option<String>,
    pub name: Option<String>,
    pub fields: FieldFilter,
    pub source: Source,
    /// Hex prefix of a boot id.
    pub boot: Option<String>,
    pub max: usize,
    /// The newest `max` of the range instead of the oldest.
    pub newest: bool,
    /// Metrics: parts of series names (all when empty).
    pub series: Vec<String>,
    pub agg: Option<Agg>,
}

fn opt_str<'a>(a: &'a Json, k: &str) -> Result<Option<&'a str>, String> {
    match a.get(k) {
        None | Some(Json::Null) => Ok(None),
        Some(v) => v.as_str().map(Some).ok_or_else(|| format!("{k}: want text")),
    }
}

fn wall_s(a: &Json, k: &str) -> Result<Option<u64>, String> {
    match a.get(k) {
        None | Some(Json::Null) => Ok(None),
        Some(v) => {
            let n = v.as_f64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()));
            match n {
                Some(n) if (0.0..1e11).contains(&n) => Ok(Some((n * 1000.0) as u64)),
                _ => Err(format!("{k}: want unix seconds (events/metrics take local times)")),
            }
        }
    }
}

impl Query {
    /// The tool's arguments; `now_ms` for the defaults (the last hour).
    pub fn from_json(a: &Json, now_ms: u64) -> Result<Query, String> {
        let kind = match opt_str(a, "kind")? {
            None | Some("events") => Kind::Events,
            Some("metrics") => Kind::Metrics,
            Some(o) => return Err(format!("kind {o:?}: events or metrics")),
        };
        let until_ms = wall_s(a, "until")?.unwrap_or(now_ms + 1000);
        let since_ms = wall_s(a, "since")?.unwrap_or(until_ms.saturating_sub(3_600_000));
        if since_ms > until_ms {
            return Err("until: before since".into());
        }
        let level = match opt_str(a, "level")? {
            None => None,
            Some(l) => Some(["error", "warn", "info", "debug", "trace"].iter().position(|&x| x == l).ok_or("level: error, warn, info, debug or trace")? as u8),
        };
        let fields = FieldFilter::parse(opt_str(a, "fields")?.unwrap_or("")).map_err(|t| format!("fields: bad term {t:?}"))?;
        let source = match opt_str(a, "source")? {
            None | Some("all") => Source::All,
            Some("log") => Source::Log,
            Some("trace") => Source::Trace,
            Some(_) => return Err("source: all, log or trace".into()),
        };
        let max_default = if kind == Kind::Metrics { 30 } else { DEFAULT_MAX };
        let max = match a.get("max") {
            None | Some(Json::Null) => max_default,
            Some(v) => match v.as_u64() {
                Some(m) if (1..=MAX_LINES as u64).contains(&m) => m as usize,
                _ => return Err(format!("max: 1 to {MAX_LINES}")),
            },
        };
        let newest = match opt_str(a, "order")? {
            None | Some("oldest") => false,
            Some("newest") => true,
            Some(_) => return Err("order: oldest or newest".into()),
        };
        let agg = match opt_str(a, "agg")? {
            None => None,
            Some("avg") => Some(Agg::Avg),
            Some("min") => Some(Agg::Min),
            Some("max") => Some(Agg::Max),
            Some("last") => Some(Agg::Last),
            Some(_) => return Err("agg: avg, min, max or last".into()),
        };
        // `name`: an event name prefix, or for metrics comma-separated parts of series names (as `metrics`).
        let name = opt_str(a, "name")?.map(String::from);
        let series = match (kind, &name) {
            (Kind::Metrics, Some(n)) => n.split(',').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect(),
            _ => Vec::new(),
        };
        let boot = opt_str(a, "boot")?.map(|b| b.to_ascii_lowercase());
        Ok(Query { kind, since_ms, until_ms, level, subsystem: opt_str(a, "subsystem")?.map(String::from), name, fields, source, boot, max, newest, series, agg })
    }

    fn boot_ok(&self, boot: u64) -> bool {
        self.boot.as_deref().is_none_or(|p| format!("{boot:016x}").starts_with(p) || format!("{boot:x}").starts_with(p))
    }

    fn covers(&self, subsystem: &str) -> bool {
        self.subsystem.as_deref().is_none_or(|s| subsystem == s || subsystem.strip_prefix(s).is_some_and(|r| r.starts_with('.')))
    }
}

#[derive(Debug, Default)]
pub struct Scan {
    pub segments: usize,
    pub blocks: usize,
    pub damaged: usize,
}

/// Every stored line of the range, oldest block first, then the lines not yet written (`pending`): `f(boot,
/// line)` until it returns false.
pub fn scan(dir: &Path, pending: &[(u64, String)], since_ms: u64, until_ms: u64, mut f: impl FnMut(u64, &str) -> bool) -> Scan {
    let mut st = Scan::default();
    let segs: Vec<Segment> = store::segments(dir).unwrap_or_default();
    for (i, s) in segs.iter().enumerate() {
        let ends = segs.get(i + 1).map(|n| n.start_s);
        // A segment of a boot without a wall clock (named in seconds since boot) is always read: a boot query
        // finds its lines whatever the time range.
        let clockless = s.start_s < store::SANE_S;
        if !clockless && (ends.is_some_and(|e| (e + 60) * 1000 < since_ms) || s.start_s * 1000 > until_ms + LATE_S * 1000) {
            continue;
        }
        st.segments += 1;
        let mut go = true;
        let _ = store::walk(&s.path, |h: &BlockHead, at, file| {
            if !h.overlaps(since_ms, until_ms) {
                return Ok(true);
            }
            st.blocks += 1;
            match store::read_body(file, h, at)? {
                Some(body) => {
                    for l in body.lines() {
                        if !f(h.boot, l) {
                            go = false;
                            return Ok(false);
                        }
                    }
                }
                None => st.damaged += 1,
            }
            Ok(true)
        });
        if !go {
            return st;
        }
    }
    for (b, l) in pending {
        if !f(*b, l) {
            break;
        }
    }
    st
}

struct Hit {
    wall: u64,
    boot: u64,
    text: String,
}

fn render_event(it: &Item<'_>, off_min: i32) -> Option<(u64, String)> {
    match *it {
        Item::Event { seq, time_ns, wall, level, subsystem, name, text, .. } => Some((wall.unwrap_or(0), format!(
            "{} #{seq} [{:5}.{:06}] {} {subsystem} {name}{}{}",
            local_time(wall.unwrap_or(0), off_min),
            time_ns / 1_000_000_000,
            (time_ns / 1000) % 1_000_000,
            line::LEVELS[level as usize],
            if text.is_empty() { "" } else { " " },
            line::unescape(text)
        ))),
        Item::Trace { cpu, pos, time_ns, wall, level, subsystem, name, text } => Some((wall.unwrap_or(0), format!(
            "{} ~{cpu}.{pos} [{:5}.{:06}] {} {subsystem} {name} {}",
            local_time(wall.unwrap_or(0), off_min),
            time_ns / 1_000_000_000,
            (time_ns / 1000) % 1_000_000,
            line::LEVELS[level as usize],
            line::unescape(text)
        ))),
        _ => None,
    }
}

fn event_matches(q: &Query, it: &Item<'_>) -> bool {
    let (wall, level, subsystem, name, text, is_log) = match *it {
        Item::Event { wall, level, subsystem, name, text, .. } => (wall, level, subsystem, name, text, true),
        Item::Trace { wall, level, subsystem, name, text, .. } => (wall, level, subsystem, name, text, false),
        _ => return false,
    };
    // A line without wall time (a boot without a wall clock) is outside every time range: only a query by boot
    // finds it.
    let in_time = match wall {
        Some(w) => (q.since_ms..=q.until_ms).contains(&w),
        None => q.boot.is_some(),
    };
    in_time
        && match q.source {
            Source::All => true,
            Source::Log => is_log,
            Source::Trace => !is_log,
        }
        && q.level.is_none_or(|l| level <= l)
        && q.covers(subsystem)
        && q.name.as_deref().is_none_or(|n| name.starts_with(n))
        && q.fields.matches(text)
}

/// The `events` answer; `off_min` = the zone's offset for the local times.
pub fn events(q: &Query, dir: &Path, pending: &[(u64, String)], off_min: i32) -> String {
    let mut hits: VecDeque<Hit> = VecDeque::new();
    let mut more = false;
    let st = scan(dir, pending, q.since_ms, q.until_ms, |boot, l| {
        if !q.boot_ok(boot) {
            return true;
        }
        let Some(it) = line::parse(l) else { return true };
        if !event_matches(q, &it) {
            return true;
        }
        let Some((wall, text)) = render_event(&it, off_min) else { return true };
        hits.push_back(Hit { wall, boot, text });
        if hits.len() > q.max {
            more = true;
            if q.newest {
                hits.pop_front();
            } else {
                hits.pop_back();
                return false;
            }
        }
        true
    });
    let mut hits: Vec<Hit> = hits.into();
    // Within one read the kernel hands out log, then trace, then metrics: order by time (stable).
    hits.sort_by_key(|h| h.wall);
    let boots = {
        let mut b: Vec<u64> = hits.iter().map(|h| h.boot).collect();
        b.dedup();
        b.sort_unstable();
        b.dedup();
        b.len()
    };
    let mut out = format!(
        "journal events since={} until={} shown={} more={} boots={boots} blocks={}{}",
        local_time(q.since_ms, off_min),
        local_time(q.until_ms, off_min),
        hits.len(),
        more as u8,
        st.blocks,
        if st.damaged > 0 { format!(" damaged={} (blocks that could not be read)", st.damaged) } else { String::new() }
    );
    let mut last_boot = None;
    for h in &hits {
        if last_boot != Some(h.boot) {
            out.push_str(&format!("\n-- boot {:016x}", h.boot));
            last_boot = Some(h.boot);
        }
        out.push('\n');
        out.push_str(&h.text);
    }
    out
}

fn fmt_num(v: f64) -> String {
    let s = format!("{v:.3}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s == "-0" { "0".into() } else { s.into() }
}

/// The `metrics` answer: `max` points per series over the range, each the mean (totals, unit `n`: the last)
/// or `agg` of the stored values in it.
pub fn metrics(q: &Query, dir: &Path, pending: &[(u64, String)], off_min: i32) -> String {
    let n = q.max.max(1);
    let span = ((q.until_ms - q.since_ms) / n as u64).max(1);
    // series -> (unit, per point: (sum, count, min, max, last))
    let mut table: BTreeMap<String, (String, Vec<Option<(f64, u32, f64, f64, f64)>>)> = BTreeMap::new();
    let mut more = false;
    let mut rows = 0usize;
    let st = scan(dir, pending, q.since_ms, q.until_ms, |boot, l| {
        if !q.boot_ok(boot) {
            return true;
        }
        let Some(Item::Metric { wall: Some(w), pairs, .. }) = line::parse(l) else { return true };
        if !(q.since_ms..=q.until_ms).contains(&w) {
            return true;
        }
        rows += 1;
        let i = (((w - q.since_ms) / span) as usize).min(n - 1);
        for (name, v, unit) in line::pairs(pairs) {
            if !q.series.is_empty() && !q.series.iter().any(|s| name.contains(s.as_str())) {
                continue;
            }
            if !table.contains_key(name) {
                if table.len() >= MAX_SERIES {
                    more = true;
                    continue;
                }
                table.insert(name.to_string(), (unit.to_string(), vec![None; n]));
            }
            let Some((_, pts)) = table.get_mut(name) else { continue };
            let p = pts[i].get_or_insert((0.0, 0, f64::MAX, f64::MIN, 0.0));
            p.0 += v;
            p.1 += 1;
            p.2 = p.2.min(v);
            p.3 = p.3.max(v);
            p.4 = v;
        }
        true
    });
    let mut out = format!(
        "journal metrics since={} until={} rows={rows} points={n} series={} more={} blocks={}",
        local_time(q.since_ms, off_min),
        local_time(q.until_ms, off_min),
        table.len(),
        more as u8,
        st.blocks
    );
    out.push_str("\ntime");
    for i in 0..n {
        out.push(' ');
        let end = q.since_ms + span * (i as u64 + 1);
        out.push_str(&local_time(end.min(q.until_ms), off_min).replace(' ', "T"));
    }
    for (name, (unit, pts)) in &table {
        out.push_str(&format!("\n{name} {unit}"));
        let agg = q.agg.unwrap_or(if unit == "n" { Agg::Last } else { Agg::Avg });
        for p in pts {
            out.push(' ');
            match p {
                None => out.push('-'),
                Some((sum, c, mn, mx, last)) => out.push_str(&fmt_num(match agg {
                    Agg::Avg => sum / *c as f64,
                    Agg::Min => *mn,
                    Agg::Max => *mx,
                    Agg::Last => *last,
                })),
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Writer, encode_block, tests::tmp};

    const T: u64 = 1_792_749_600_000;

    fn q(json: &str) -> Query {
        Query::from_json(&Json::parse(json).unwrap(), T + 3_600_000).unwrap()
    }

    fn ev(seq: u64, wall: u64, level: &str, sub: &str, name: &str, text: &str) -> String {
        format!("E {seq} {} {wall} {level} 0 1 {sub} {name} {text}", seq * 1_000_000)
    }

    #[test]
    fn arguments() {
        let a = q(r#"{"since":1792749600,"until":1792753200,"level":"warn","subsystem":"pm","fields":"bytes>5","max":5,"order":"newest","boot":"ABC"}"#);
        assert_eq!((a.since_ms, a.until_ms, a.level, a.max, a.newest, a.boot.as_deref()), (T, T + 3_600_000, Some(1), 5, true, Some("abc")));
        let d = q("{}");
        assert_eq!((d.since_ms, d.until_ms, d.kind, d.max), (T + 1000, T + 3_601_000, Kind::Events, 200));
        let m = q(r#"{"kind":"metrics","name":"cpu.all, temp"}"#);
        assert_eq!((m.kind, m.series.len(), m.max), (Kind::Metrics, 2, 30));
        for bad in [r#"{"kind":"x"}"#, r#"{"since":5,"until":4}"#, r#"{"level":"loud"}"#, r#"{"fields":"a>b"}"#, r#"{"max":0}"#, r#"{"since":"03:12"}"#] {
            assert!(Query::from_json(&Json::parse(bad).unwrap(), T).is_err(), "{bad}");
        }
    }

    #[test]
    fn events_by_time_subsystem_name_fields_across_boots() {
        let d = tmp("query");
        let mut w = Writer::new(&d, 1 << 20);
        let b1 = vec![
            ev(1, T + 1000, "I", "pm", "conn.closed", "peer=10.0.2.2:443 bytes=900 exe=brave"),
            ev(2, T + 2000, "W", "net.dhcp", "lease.lost", "card=igc0"),
            "T 0 5 3000000000 1792749603000 W irq trace.irq_storm vector=65 per_s=120000".to_string(),
            ev(3, T + 4000, "I", "pm", "conn.lost", "peer=10.0.2.3:80 bytes=3 | two\\nlines"),
        ];
        w.append(&encode_block(0xaaaa, &b1), T / 1000).unwrap();
        let b2 = vec![ev(1, T + 10_000, "I", "pm", "conn.closed", "peer=10.0.2.9:443 bytes=50")];
        w.append(&encode_block(0xbbbb, &b2), T / 1000 + 10).unwrap();
        let pending = vec![(0xbbbb, ev(2, T + 11_000, "E", "net", "link.down", "card=igc0"))];
        let all = events(&q(r#"{"since":1792749600,"until":1792749700}"#), &d, &pending, 120);
        assert!(all.starts_with("journal events since=2026-10-23 12:00:00.000 until=2026-10-23 12:01:40.000 shown=6 more=0 boots=2"), "{all}");
        assert!(all.contains("\n-- boot 000000000000aaaa\n2026-10-23 12:00:01.000 #1 [    0.001000] I pm conn.closed peer=10.0.2.2:443"), "{all}");
        assert!(all.contains("~0.5 [    3.000000] W irq trace.irq_storm") && all.contains("| two\nlines") && all.contains("link.down"), "{all}");
        let pm = events(&q(r#"{"since":1792749600,"until":1792749700,"subsystem":"pm","fields":"bytes>=50"}"#), &d, &[], 0);
        assert!(pm.contains("shown=2") && pm.contains("10.0.2.2") && pm.contains("10.0.2.9") && !pm.contains("10.0.2.3"), "{pm}");
        let net = events(&q(r#"{"since":1792749600,"until":1792749700,"subsystem":"net","source":"log"}"#), &d, &pending, 0);
        assert!(net.contains("shown=2") && net.contains("lease.lost"), "net covers net.dhcp: {net}");
        let narrow = events(&q(r#"{"since":1792749601.5,"until":1792749604,"name":"conn."}"#), &d, &[], 0);
        assert!(narrow.contains("shown=1") && narrow.contains("conn.lost"), "{narrow}");
        let first = events(&q(r#"{"since":1792749600,"until":1792749700,"max":2}"#), &d, &[], 0);
        assert!(first.contains("shown=2 more=1") && first.contains("#1 ") && first.contains("lease.lost"), "{first}");
        let last = events(&q(r#"{"since":1792749600,"until":1792749700,"max":1,"order":"newest","boot":"bbbb"}"#), &d, &pending, 0);
        assert!(last.contains("shown=1 more=1") && last.contains("link.down"), "{last}");
        let none = events(&q(r#"{"since":1700000000,"until":1700000100}"#), &d, &[], 0);
        assert!(none.contains("shown=0 more=0") && none.contains("blocks=0"), "blocks outside the range are not read: {none}");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn hostile_arguments_and_a_boot_without_wall_clock() {
        for bad in [r#"{"max":2001}"#, r#"{"max":-1}"#, r#"{"max":"many"}"#, r#"{"fields":"=x"}"#, r#"{"fields":"peer="}"#, r#"{"fields":"a!b"}"#, r#"{"fields":"bytes>=lots"}"#, r#"{"since":"x"}"#, r#"{"until":-5}"#, r#"{"order":"random"}"#, r#"{"agg":"sum"}"#, r#"{"source":"disk"}"#] {
            assert!(Query::from_json(&Json::parse(bad).unwrap(), T).is_err(), "{bad}");
        }
        let d = tmp("qclockless");
        // Boot 0xcccc had no wall clock: its segment is named in seconds since boot, its lines carry `-`.
        let mut w = Writer::new(&d, 1 << 20);
        let dark = vec![
            "B 000000000000cccc 0 0 1000 - 60".to_string(),
            "E 1 2000000000 - I 0 1 pm conn.closed peer=10.0.2.7:443 bytes=5".to_string(),
            "T 0 3 2500000000 - D rbtest t.mark a=1".to_string(),
        ];
        w.append(&encode_block(0xcccc, &dark), 42).unwrap();
        let mut w = Writer::new(&d, 1 << 20);
        w.append(&encode_block(0xdddd, &[ev(1, T + 1000, "I", "pm", "conn.closed", "peer=10.0.2.8:443")]), T / 1000).unwrap();
        let by_time = events(&q(r#"{"since":0,"until":1792749700}"#), &d, &[], 0);
        assert!(by_time.contains("shown=1") && by_time.contains("10.0.2.8") && !by_time.contains("10.0.2.7"), "a time query does not find lines without wall time: {by_time}");
        let by_boot = events(&q(r#"{"boot":"cccc"}"#), &d, &[], 0);
        assert!(by_boot.contains("shown=2") && by_boot.contains("- #1 [    2.000000] I pm conn.closed peer=10.0.2.7:443") && by_boot.contains("~0.3 "), "a boot query does: {by_boot}");
        // An empty window, a window in the future and the largest max answer without error.
        assert!(events(&q(r#"{"since":1792749601,"until":1792749601}"#), &d, &[], 0).contains("shown=1"));
        assert!(events(&q(r#"{"since":1892749600,"until":1892749700,"max":2000}"#), &d, &[], 0).contains("shown=0 more=0"));
        let m = metrics(&q(r#"{"kind":"metrics","since":1792749600,"until":1792749600,"max":2000}"#), &d, &[], 0);
        assert!(m.starts_with("journal metrics ") && m.contains("rows=0"), "{m}");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn metrics_in_points() {
        let d = tmp("qmetrics");
        let mut w = Writer::new(&d, 1 << 20);
        let rows: Vec<String> = (0..4).map(|i| format!("M {i} {} {} cpu.all.busy={}% mem.free=100B klog.lost={i}n", i * 60, T + i * 60_000, 10 * (i + 1))).collect();
        w.append(&encode_block(1, &rows), T / 1000).unwrap();
        let m = metrics(&q(r#"{"kind":"metrics","since":1792749600,"until":1792749840,"max":2,"name":"cpu,klog"}"#), &d, &[], 0);
        let l: Vec<&str> = m.lines().collect();
        assert!(l[0].contains("rows=4 points=2 series=2"), "{m}");
        assert!(l[1].starts_with("time 2026-10-23T10:02:00.000 "), "{m}");
        assert_eq!(l[2], "cpu.all.busy % 15 35");
        assert_eq!(l[3], "klog.lost n 1 3", "totals show their last value");
        let mx = metrics(&q(r#"{"kind":"metrics","since":1792749600,"until":1792749840,"max":1,"name":"cpu","agg":"max"}"#), &d, &[], 0);
        assert!(mx.ends_with("cpu.all.busy % 40"), "{mx}");
        std::fs::remove_dir_all(&d).unwrap();
    }
}
