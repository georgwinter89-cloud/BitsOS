//! `bitsjournal serve` (init starts it as uid 0): waits for a writable /data, then reads `/dev/bitsos/events`
//! and keeps what it reads. Lines are collected in memory and written as one block every `FLUSH_S` seconds (or
//! at `BLOCK_RAW` bytes, or at once when a Reload or reboot asks with `F`), each block made durable with one
//! fsync, then acked to the kernel (`A <next>`). Metric rounds (every 2 s) are kept as one mean per
//! `journal.metrics_s` (default 60 s). After a restart in the same boot the lines already kept are skipped
//! (`State`, the boot id of the `B` line); a new boot starts at 0. Beside it a thread serves the MCP tool
//! `journal`. Threads stay in the normal class (the journal waits for the disk; B4 keeps it out of the upper
//! classes).

use crate::line::{self, Item};
use crate::settings::{self, Settings};
use crate::store::{self, Writer};
use crate::{DIR, NODE, log, log_warn, now_ms, query};
use mcpreg::{Json, Reply, Safety, Service, Tool};
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Seconds between blocks (one fsync each): at most this much is lost to a power cut.
pub const FLUSH_S: u64 = 5;
/// A block is written at this many raw bytes at the latest.
pub const BLOCK_RAW: usize = 256 << 10;
/// Read buffer for the node (a metric line is at most ~3 KiB, a record ~400 bytes).
const READ_BUF: usize = 256 << 10;
/// While writing fails (a full or broken /data), at most this much waits in memory; then the node is not read
/// until a write works again: the kernel ring keeps what it can and the node reports the rest as lost (`G`/`g`
/// lines), so memory stays bounded and the gap is written down.
pub const PENDING_MAX: usize = 32 * BLOCK_RAW;
const RETAIN_EVERY: Duration = Duration::from_secs(3600);
const SETTINGS_EVERY: Duration = Duration::from_secs(60);

/// How far this boot is kept (written to `<dir>/state` after each block; a restart in the same boot skips what
/// is below). Not fsynced: after a power cut the boot changed anyway.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct State {
    pub boot: u64,
    /// The zone's offset (minutes east of UTC) the kernel named in its `B` line, for local times.
    pub off_min: i32,
    pub seq_next: u64,
    pub round_next: u64,
    pub trace: BTreeMap<u32, u64>,
}

impl State {
    pub fn to_text(&self) -> String {
        let t: Vec<String> = self.trace.iter().map(|(c, p)| format!("{c}:{p}")).collect();
        format!("boot {:016x} seq {} round {} trace {} off {}\n", self.boot, self.seq_next, self.round_next, t.join(","), self.off_min)
    }
    pub fn parse(s: &str) -> Option<State> {
        let w: Vec<&str> = s.split_whitespace().collect();
        if w.len() < 6 || w[0] != "boot" || w[2] != "seq" || w[4] != "round" {
            return None;
        }
        let mut st = State { boot: u64::from_str_radix(w[1], 16).ok()?, seq_next: w[3].parse().ok()?, round_next: w[5].parse().ok()?, ..State::default() };
        if let Some(i) = w.iter().position(|&x| x == "off") {
            st.off_min = w.get(i + 1)?.parse().ok()?;
        }
        if let (Some("trace"), Some(t)) = (w.get(6).copied(), w.get(7).filter(|t| **t != "off")) {
            for cp in t.split(',') {
                let (c, p) = cp.split_once(':')?;
                st.trace.insert(c.parse().ok()?, p.parse().ok()?);
            }
        }
        Some(st)
    }
}

/// Metric rounds summed until `journal.metrics_s` has passed.
#[derive(Default)]
struct MetricSum {
    first_wall: Option<u64>,
    round: u64,
    time_ns: u64,
    wall: u64,
    /// series -> (sum, count, last, unit)
    series: BTreeMap<String, (f64, u32, f64, String)>,
}

impl MetricSum {
    fn add(&mut self, round: u64, time_ns: u64, wall: u64, pairs: &str) {
        self.first_wall.get_or_insert(wall);
        (self.round, self.time_ns, self.wall) = (round, time_ns, wall);
        for (k, v, unit) in line::pairs(pairs) {
            let e = self.series.entry(k.to_string()).or_insert_with(|| (0.0, 0, 0.0, unit.to_string()));
            e.0 += v;
            e.1 += 1;
            e.2 = v;
        }
    }

    fn due(&self, now_wall: u64, every_s: u64) -> bool {
        self.first_wall.is_some_and(|f| now_wall.saturating_sub(f) >= every_s * 1000)
    }

    /// The mean of each series (totals, unit `n`: the last value) as `M` lines of at most ~3 KiB.
    fn take(&mut self) -> Vec<String> {
        if self.first_wall.take().is_none() {
            return Vec::new();
        }
        let head = format!("M {} {} {}", self.round, self.time_ns, self.wall);
        let mut out = Vec::new();
        let mut l = head.clone();
        for (k, (sum, n, last, unit)) in std::mem::take(&mut self.series) {
            if l.len() > 3000 {
                out.push(std::mem::replace(&mut l, head.clone()));
            }
            let v = if unit == "n" { last } else { sum / n.max(1) as f64 };
            let s = format!("{v:.3}");
            l.push_str(&format!(" {k}={}{unit}", s.trim_end_matches('0').trim_end_matches('.')));
        }
        out.push(l);
        out
    }
}

/// Lines waiting for the next block, shared with the query thread (boot id, line).
pub type Pending = Arc<Mutex<Vec<(u64, String)>>>;

/// What the journal keeps, without the node and the clock (host-tested).
pub struct Journal {
    pub dir: PathBuf,
    pub writer: Writer,
    pub pending: Pending,
    pending_bytes: usize,
    first_pending: Option<Instant>,
    /// What was kept of the current boot (the floor for skipping after a restart).
    pub state: State,
    /// Every record below it was handed to us (kept or skipped as kept before).
    pub seen_next: u64,
    /// A Reload/reboot waits for this sequence number.
    pub drain: Option<u64>,
    pub off_min: Arc<AtomicI32>,
    metrics: MetricSum,
    pub metrics_s: u64,
    pub lines: u64,
    pub skipped: u64,
    pub raw_bytes: u64,
    /// A new segment was begun since the retention rules last ran.
    pub rotated: bool,
}

impl Journal {
    pub fn open(dir: &Path, s: &Settings) -> Journal {
        let state = fs::read_to_string(dir.join("state")).ok().and_then(|t| State::parse(&t)).unwrap_or_default();
        // Europe/Berlin's standard offset until the kernel names the zone.
        let off = if state.boot != 0 { state.off_min } else { 60 };
        Journal {
            dir: dir.to_path_buf(),
            writer: Writer::new(dir, s.seg_max()),
            pending: Arc::new(Mutex::new(Vec::new())),
            pending_bytes: 0,
            first_pending: None,
            state,
            seen_next: 0,
            drain: None,
            off_min: Arc::new(AtomicI32::new(off)),
            metrics: MetricSum::default(),
            metrics_s: s.metrics_s,
            lines: 0,
            skipped: 0,
            raw_bytes: 0,
            rotated: false,
        }
    }

    fn keep(&mut self, l: String) {
        self.pending_bytes += l.len() + 1;
        self.raw_bytes += l.len() as u64 + 1;
        self.lines += 1;
        self.first_pending.get_or_insert_with(Instant::now);
        let boot = self.state.boot;
        if let Ok(mut p) = self.pending.lock() {
            p.push((boot, l));
        }
    }

    /// Takes one line of the node.
    pub fn feed(&mut self, l: &str) {
        let Some(it) = line::parse(l) else {
            self.skipped += 1;
            return;
        };
        let st = &mut self.state;
        match it {
            Item::Boot { id, off_min, .. } => {
                self.off_min.store(off_min, Ordering::Relaxed);
                st.off_min = off_min;
                if id != st.boot {
                    *st = State { boot: id, off_min, ..State::default() };
                    self.seen_next = 0;
                    self.keep(l.to_string());
                }
            }
            Item::Event { seq, .. } => {
                self.seen_next = self.seen_next.max(seq + 1);
                if seq < st.seq_next {
                    self.skipped += 1;
                    return;
                }
                st.seq_next = seq + 1;
                self.keep(l.to_string());
            }
            Item::Gap { before, .. } => {
                self.seen_next = self.seen_next.max(before);
                if before > st.seq_next {
                    st.seq_next = before;
                    self.keep(l.to_string());
                }
            }
            Item::Trace { cpu, pos, .. } => {
                let floor = st.trace.entry(cpu).or_insert(0);
                if pos < *floor {
                    self.skipped += 1;
                    return;
                }
                *floor = pos + 1;
                self.keep(l.to_string());
            }
            Item::TraceGap { .. } => self.keep(l.to_string()),
            Item::Metric { round, time_ns, wall, pairs } => {
                if round < st.round_next {
                    self.skipped += 1;
                    return;
                }
                st.round_next = round;
                let Some(w) = wall else { return };
                self.metrics.add(round, time_ns, w, pairs);
                if self.metrics.due(w, self.metrics_s) {
                    for m in self.metrics.take() {
                        self.keep(m);
                    }
                }
            }
            Item::Flush { target } => self.drain = Some(self.drain.map_or(target, |d| d.max(target))),
            Item::Idle { next, .. } => self.seen_next = self.seen_next.max(next),
        }
    }

    /// Whether a block should be written now.
    pub fn due(&self) -> bool {
        self.drain.is_some_and(|t| self.seen_next >= t)
            || self.pending_bytes >= BLOCK_RAW
            || self.first_pending.is_some_and(|t| t.elapsed() >= Duration::from_secs(FLUSH_S))
    }

    /// Whether the node must not be read now (writing failed and `PENDING_MAX` is reached).
    pub fn full(&self) -> bool {
        self.pending_bytes >= PENDING_MAX
    }

    /// Writes what is pending (with the metric sum when a drain asks) and returns the sequence number to ack.
    /// Blocks of one boot each (the head carries the boot id) and at most `store::CHUNK_RAW` raw bytes; each
    /// block's lines leave `pending` as soon as it is durable, so a failure later in the same flush neither
    /// loses nor repeats them.
    pub fn flush(&mut self, now_s: u64) -> io::Result<u64> {
        if self.drain.is_some() {
            for m in self.metrics.take() {
                self.keep(m);
            }
        }
        let lines: Vec<(u64, String)> = self.pending.lock().map(|p| p.clone()).unwrap_or_default();
        if !lines.is_empty() {
            let mut i = 0;
            while i < lines.len() {
                let boot = lines[i].0;
                let (mut j, mut raw) = (i, 0usize);
                while j < lines.len() && lines[j].0 == boot && (j == i || raw + lines[j].1.len() + 1 <= store::CHUNK_RAW) {
                    raw += lines[j].1.len() + 1;
                    j += 1;
                }
                let text: Vec<String> = lines[i..j].iter().map(|(_, l)| l.clone()).collect();
                self.rotated |= self.writer.append(&store::encode_block(boot, &text), now_s)?;
                if let Ok(mut p) = self.pending.lock() {
                    p.drain(..j - i);
                }
                self.pending_bytes = self.pending_bytes.saturating_sub(raw);
                i = j;
            }
            let tmp = self.dir.join("state.tmp");
            if fs::write(&tmp, self.state.to_text()).is_ok() {
                let _ = fs::rename(&tmp, self.dir.join("state"));
            }
        }
        self.pending_bytes = 0;
        self.first_pending = None;
        if self.drain.is_some_and(|t| self.seen_next >= t) {
            self.drain = None;
        }
        Ok(self.seen_next)
    }
}

const JOURNAL_SCHEMA: &str = r#"{"type":"object","properties":{"kind":{"type":"string","enum":["events","metrics","status"],"description":"default events; status: the journal's files and settings"},"since":{"type":"number","description":"unix seconds (default: an hour before until)"},"until":{"type":"number","description":"unix seconds (default now)"},"level":{"type":"string","enum":["error","warn","info","debug","trace"]},"subsystem":{"type":"string"},"name":{"type":"string","description":"events: prefix of the event name; metrics: comma-separated parts of series names"},"fields":{"type":"string","description":"e.g. 'peer=10.0.2.2:443 bytes>1000'"},"source":{"type":"string","enum":["all","log","trace"]},"boot":{"type":"string","description":"hex prefix of a boot id (the '-- boot' lines)"},"order":{"type":"string","enum":["oldest","newest"]},"max":{"type":"integer","minimum":1,"maximum":2000,"description":"events: lines (default 200); metrics: points per series (default 30)"},"agg":{"type":"string","enum":["avg","min","max","last"]}}}"#;

pub fn tools() -> Vec<Tool> {
    vec![Tool::new(
        "journal",
        "The journal of all boots (B6, weeks back): kernel events, trace entries and metrics kept on /data. Easier through events/metrics with since/until (local times); here since/until are unix seconds. Events: '<local time> #seq [s since boot] L subsystem name fields | message' ('~cpu.pos' for trace entries), '-- boot <id>' where a boot begins; oldest first (order newest: the newest max). Metrics: kind metrics, one mean per minute stored, max points per series. kind status: files, size, oldest time, settings.",
        JOURNAL_SCHEMA,
        Safety::Read,
    )
    .expect("schema is JSON")]
}

/// `kind: status`: the journal's files and settings (the desk's size check over MCP).
pub fn status(dir: &Path, pending: usize, off_min: i32, s: &Settings) -> String {
    let segs = store::segments(dir).unwrap_or_default();
    let bytes: u64 = segs.iter().map(|g| g.bytes).sum();
    let oldest = segs.first().map_or(0, |g| g.start_s * 1000);
    let newest = segs.last().map_or(0, |g| g.mtime_s * 1000);
    format!(
        "journal status dir={} segments={} bytes={bytes} oldest={} newest={} pending_lines={pending} keep_days={} max_mb={} metrics_s={} enabled={}",
        dir.display(),
        segs.len(),
        crate::local_time(oldest, off_min),
        crate::local_time(newest, off_min),
        s.keep_days,
        s.max_mb,
        s.metrics_s,
        s.enabled
    )
}

pub fn answer(dir: &Path, pending: &Pending, off_min: i32, args: &Json) -> Reply {
    if args.get("kind").and_then(Json::as_str) == Some("status") {
        let n = pending.lock().map_or(0, |p| p.len());
        return Reply::text(status(dir, n, off_min, &settings::load(settings::SOCKET).0));
    }
    let q = match query::Query::from_json(args, now_ms()) {
        Ok(q) => q,
        Err(e) => return Reply::bad_argument(e),
    };
    let p = pending.lock().map(|p| p.clone()).unwrap_or_default();
    match q.kind {
        query::Kind::Events => Reply::text(query::events(&q, dir, &p, off_min)),
        query::Kind::Metrics => Reply::text(query::metrics(&q, dir, &p, off_min)),
    }
}

fn mcp_loop(dir: PathBuf, pending: Pending, off: Arc<AtomicI32>) {
    let mut delay = 1;
    let mut said = String::new();
    loop {
        let r = Service::connect(settings::OWNER).and_then(|mut s| {
            s.register(&tools())?;
            log("MCP tool journal registered (events/metrics with since/until reach it)");
            delay = 1;
            s.serve(|c, _| if c.name == "journal" { answer(&dir, &pending, off.load(Ordering::Relaxed), &c.arguments) } else { Reply::bad_argument(format!("unknown tool {}", c.name)) })
        });
        if let Err(e) = r {
            let m = e.to_string();
            if m != said {
                log(&format!("MCP: {m}; retrying"));
                said = m;
            }
        }
        std::thread::sleep(Duration::from_secs(delay));
        delay = (delay * 2).min(60);
    }
}

/// Waits until /data can be written; false when this boot has none (the reason was logged).
fn wait_data() -> bool {
    let mut said = String::new();
    loop {
        match crate::data_state(fs::read_to_string(crate::DATA_STATE).ok().as_deref()) {
            crate::DataState::Ready => return true,
            crate::DataState::Pending => std::thread::sleep(Duration::from_secs(1)),
            crate::DataState::Never(why) => {
                if why != said {
                    log(&format!("no writable /data on this boot ({why}): the journal waits"));
                    said = why;
                }
                std::thread::sleep(Duration::from_secs(30));
            }
        }
    }
}

fn prepare(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::create_dir_all(dir)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
}

fn retain(j: &Journal, s: &Settings) {
    match store::retain(&j.dir, now_ms() / 1000, s.keep_s(), s.max_bytes()) {
        Ok(gone) if !gone.is_empty() => {
            let bytes: u64 = gone.iter().map(|g| g.bytes).sum();
            log(&format!("retention: deleted {} segments ({bytes} bytes; journal.keep_days={}, journal.max_mb={})", gone.len(), s.keep_days, s.max_mb));
        }
        Ok(_) => {}
        Err(e) => log_warn(&format!("retention: {e}")),
    }
}

/// Tells the kernel how far the journal is durable: one write per line (the node reads whole lines per write).
fn ack(node: &mut fs::File, next: u64) -> io::Result<()> {
    node.write_all(format!("A {next}\n").as_bytes())
}

/// The service; `once`: one pass over what the node has now, then exit (tests).
pub fn serve(once: bool) -> i32 {
    wait_data();
    let dir = PathBuf::from(DIR);
    if let Err(e) = prepare(&dir) {
        log_warn(&format!("cannot make {DIR}: {e}"));
        return 1;
    }
    let (mut s, why) = settings::load(settings::SOCKET);
    if let Some(w) = why {
        log(&format!("settings: defaults ({w})"));
    }
    let mut j = Journal::open(&dir, &s);
    retain(&j, &s);
    {
        let (d, p, o) = (dir.clone(), j.pending.clone(), j.off_min.clone());
        if !once {
            std::thread::Builder::new().name("journal-mcp".into()).spawn(move || mcp_loop(d, p, o)).ok();
        }
    }
    log(&format!("keeping {NODE} in {DIR} (journal.keep_days={}, journal.max_mb={}, metrics every {} s)", s.keep_days, s.max_mb, s.metrics_s));
    let mut buf = vec![0u8; READ_BUF];
    let (mut last_retain, mut last_settings) = (Instant::now(), Instant::now());
    let mut said_write = false;
    'open: loop {
        if !s.enabled {
            std::thread::sleep(SETTINGS_EVERY);
            s = settings::load(settings::SOCKET).0;
            continue;
        }
        let mut node = match OpenOptions::new().read(true).write(true).open(NODE) {
            Ok(f) => f,
            Err(e) => {
                log_warn(&format!("{NODE}: {e}; retrying in 5 s"));
                std::thread::sleep(Duration::from_secs(5));
                continue;
            }
        };
        // The first ack attaches us: from now on a Reload waits for the journal.
        let _ = ack(&mut node, 0);
        let mut said_full = false;
        loop {
            if j.full() {
                // Writing fails and memory holds PENDING_MAX: retry the write without reading on.
                match j.flush(now_ms() / 1000) {
                    Ok(ack_to) => {
                        log(&format!("the journal writes again ({} lines kept while it could not)", j.lines));
                        (said_full, said_write) = (false, false);
                        let _ = ack(&mut node, ack_to);
                    }
                    Err(e) => {
                        if !said_full {
                            log_warn(&format!("writing the journal: {e}; {PENDING_MAX} bytes wait in memory: not reading on (the kernel reports what is lost)"));
                            said_full = true;
                        }
                        std::thread::sleep(Duration::from_secs(1));
                    }
                }
                continue;
            }
            let n = match node.read(&mut buf) {
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    log_warn(&format!("reading {NODE}: {e}; opening it again"));
                    std::thread::sleep(Duration::from_secs(1));
                    continue 'open;
                }
            };
            let text = String::from_utf8_lossy(&buf[..n]);
            for l in text.lines() {
                j.feed(l);
            }
            if j.due() || (once && n == 0) {
                match j.flush(now_ms() / 1000) {
                    Ok(ack_to) => {
                        said_write = false;
                        let _ = ack(&mut node, ack_to);
                        if std::mem::take(&mut j.rotated) {
                            retain(&j, &s);
                        }
                    }
                    Err(e) => {
                        if !said_write {
                            log_warn(&format!("writing the journal: {e} (kept in memory, retrying)"));
                            said_write = true;
                        }
                        std::thread::sleep(Duration::from_secs(1));
                    }
                }
            }
            if once && text.starts_with("I ") {
                let _ = j.flush(now_ms() / 1000).map(|a| ack(&mut node, a));
                return 0;
            }
            if last_settings.elapsed() >= SETTINGS_EVERY {
                last_settings = Instant::now();
                let (ns, _) = settings::load(settings::SOCKET);
                if ns != s {
                    log(&format!("settings: journal.enabled={} keep_days={} max_mb={} metrics_s={}", ns.enabled, ns.keep_days, ns.max_mb, ns.metrics_s));
                    j.writer.seg_max = ns.seg_max();
                    j.metrics_s = ns.metrics_s;
                    let shrink = ns.keep_days < s.keep_days || ns.max_mb < s.max_mb;
                    s = ns;
                    if shrink {
                        retain(&j, &s);
                    }
                    if !s.enabled {
                        let _ = j.flush(now_ms() / 1000);
                        log("journal.enabled=false: not keeping new events");
                        continue 'open;
                    }
                }
            }
            if last_retain.elapsed() >= RETAIN_EVERY {
                last_retain = Instant::now();
                retain(&j, &s);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::tests::tmp;

    const W: u64 = 1_792_749_600_000;

    fn ev(seq: u64, name: &str) -> String {
        format!("E {seq} {} {} I 0 1 rbtest {name} n={seq}", seq * 1000, W + seq)
    }

    #[test]
    fn state_round_trip() {
        let st = State { boot: 0xabc, off_min: 120, seq_next: 9, round_next: 4, trace: [(0, 5), (3, 7)].into_iter().collect() };
        assert_eq!(State::parse(&st.to_text()), Some(st));
        assert_eq!(State::parse("boot zz seq 1 round 2 trace "), None);
        assert_eq!(State::parse("boot 1 seq 1 round 2 trace ").map(|s| s.trace.len()), Some(0));
    }

    #[test]
    fn two_boots_a_restart_skips_what_was_kept_and_a_drain_flushes() {
        let d = tmp("serve");
        let s = Settings { metrics_s: 10, ..Settings::default() };
        let mut j = Journal::open(&d, &s);
        for l in ["B 000000000000000a 0 0 1000 1792749600000 120".to_string(), ev(0, "boot.one"), ev(1, "t.a"), "G 5 3".into(), ev(5, "t.b"), "T 0 2 9000 1792749600009 D rbtest t.mark a=1 b=2".into()] {
            j.feed(&l);
        }
        for r in 0..3u64 {
            j.feed(&format!("M {r} {} {} cpu.all.busy={}% klog.lost={r}n", r * 2_000_000_000, W + r * 2000, 10 * (r + 1)));
        }
        assert_eq!((j.lines, j.seen_next, j.off_min.load(Ordering::Relaxed)), (6, 6, 120), "B, three records, the gap and the trace entry; metrics still summed");
        j.feed("F 6");
        assert!(j.due(), "a drain whose target was seen writes at once");
        assert_eq!(j.flush(W / 1000).unwrap(), 6);
        assert_eq!((j.drain, j.writer.fsyncs), (None, 1));
        // Restart in the same boot: the node hands out everything again from its start.
        let mut j2 = Journal::open(&d, &s);
        assert_eq!(j2.state.seq_next, 6);
        for l in ["B 000000000000000a 0 0 1000 1792749600000 120".to_string(), ev(0, "boot.one"), ev(1, "t.a"), "G 5 3".into(), ev(5, "t.b"), "T 0 2 9000 1792749600009 D rbtest t.mark a=1 b=2".into(), ev(6, "t.c")] {
            j2.feed(&l);
        }
        assert_eq!((j2.lines, j2.seen_next), (1, 7), "only t.c is new");
        j2.flush(W / 1000 + 5).unwrap();
        // A new boot (a Reload): numbers start at 0 again and are kept.
        let mut j3 = Journal::open(&d, &s);
        for l in ["B 000000000000000b 1 0 1000 1792749700000 120".to_string(), ev(0, "boot.two")] {
            j3.feed(&l);
        }
        assert_eq!((j3.lines, j3.state.boot), (2, 0xb));
        let p = j3.pending.clone();
        let q = query::Query::from_json(&Json::parse(r#"{"since":1792749600,"until":1792749800,"subsystem":"rbtest"}"#).unwrap(), W).unwrap();
        let a = query::events(&q, &d, &p.lock().unwrap(), 120);
        assert!(a.contains("shown=6") && a.contains("-- boot 000000000000000a") && a.contains("-- boot 000000000000000b"), "{a}");
        assert_eq!(a.matches("t.a ").count(), 1, "no duplicate after the restart: {a}");
        let st = status(&d, 0, 0, &s);
        assert!(st.starts_with("journal status ") && st.contains(" segments=1 ") && st.contains(" oldest=2026-10-23 "), "{st}");
        let m = query::metrics(&query::Query::from_json(&Json::parse(r#"{"kind":"metrics","since":1792749600,"until":1792749610,"max":1}"#).unwrap(), W).unwrap(), &d, &[], 0);
        assert!(m.contains("cpu.all.busy % 20") && m.contains("klog.lost n 2"), "the drain wrote the partial metric mean: {m}");
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_big_flush_is_split_and_a_failed_write_keeps_the_rest_once() {
        let d = tmp("chunks");
        let mut j = Journal::open(&d, &Settings::default());
        j.feed("B 0000000000000001 0 0 1 1792749600000 120");
        // About 3 MiB raw: more than one block may hold.
        let pad = "x".repeat(1000);
        for i in 0..3000u64 {
            j.feed(&format!("E {i} {} {} I 0 1 rbtest t.big n={i} pad={pad}", i * 1000, W + i));
        }
        assert!(!j.full());
        let to = j.flush(W / 1000).unwrap();
        assert_eq!(to, 3000);
        let mut heads = Vec::new();
        let seg = j.writer.current().unwrap().path.clone();
        store::walk(&seg, |h, _, _| {
            heads.push(*h);
            Ok(true)
        })
        .unwrap();
        assert!(heads.len() >= 3 && heads.iter().all(|h| h.raw_len as usize <= store::CHUNK_RAW), "{} blocks", heads.len());
        assert_eq!(heads.iter().map(|h| h.lines as u64).sum::<u64>(), 3001, "every line once");
        // /data goes away: the flush fails, the lines stay pending (once), and the next good flush writes them.
        j.feed(&ev(3000, "t.after"));
        let saved = j.dir.clone();
        j.writer = Writer::new(&saved.join("gone"), 1 << 20);
        assert!(j.flush(W / 1000).is_err());
        assert_eq!(j.pending.lock().unwrap().len(), 1);
        j.writer = Writer::new(&saved, 1 << 20);
        j.flush(W / 1000).unwrap();
        assert!(j.pending.lock().unwrap().is_empty());
        let a = query::events(&query::Query::from_json(&Json::parse(r#"{"since":1792749600,"until":1792749700,"name":"t.after"}"#).unwrap(), W).unwrap(), &d, &[], 0);
        assert!(a.contains("shown=1"), "{a}");
        fs::remove_dir_all(&d).unwrap();
    }

    /// CONTRACTS §4: what one event costs in the journal (raw and stored bytes, CPU of parse + compress).
    #[test]
    fn cost_per_event() {
        let d = tmp("cost");
        let mut j = Journal::open(&d, &Settings::default());
        j.feed("B 0000000000000001 0 0 1 1792749600000 120");
        const N: u64 = 20_000;
        let t0 = Instant::now();
        for i in 0..N {
            let peer = format!("192.168.100.{}:{}", i % 50, 40_000 + i % 997);
            j.feed(&format!("E {i} {} {} I {} {} pm conn.closed exe=brave peer={peer} name=\"www.example{}.org\" sent={} received={} ms={}", i * 3_000_000, W + i * 3, i % 16, 100 + i % 7, i % 13, i * 31 % 9000, i * 77 % 100_000, i % 5000));
            if j.pending_bytes >= BLOCK_RAW {
                j.flush(W / 1000).unwrap();
            }
        }
        j.flush(W / 1000).unwrap();
        let cpu = t0.elapsed();
        let stored: u64 = store::segments(&d).unwrap().iter().map(|s| s.bytes).sum();
        let raw = j.raw_bytes / N;
        let disk = stored as f64 / N as f64;
        eprintln!("journal cost: {raw} raw bytes/event, {disk:.1} stored bytes/event, {:.2} us/event (parse + compress + write, host)", cpu.as_secs_f64() * 1e6 / N as f64);
        assert!(disk < raw as f64 / 2.0, "compression at least 2:1 ({disk} of {raw})");
        fs::remove_dir_all(&d).unwrap();
    }
}
