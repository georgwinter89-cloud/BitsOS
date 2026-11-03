//! IN-6 (B4): input-to-screen latency, measured per frame. A sample runs from the input report's arrival (the
//! evdev timestamp: for USB the time the controller's interrupt for the report was noticed, handed by xhci to
//! usb-hid's `on_report_at` (US-ev, IN-6b); the one clock CP-5) to the moment the frame that shows the reaction leaves for the framebuffer copy, with the stages in
//! between: queue (arrival -> the VT's input thread took the event), draw (-> the copy starts) and copy (-> the
//! copy is done). Paths: the console (echo, monitor), the pointer on the console, the pointer over a fullscreen
//! program (`pointer_over`), and a program's own frame (a program read the event, fbdev presented its frame:
//! needs DP's hook, change request IN-6/2). MCP tool `latency` (usb-hid's `tool_latency.rs`) reads it.
//!
//! Only frames presented while the input thread works through a batch of events count for the console paths
//! (`batch_done` forgets input that drew nothing, so a later unrelated frame is never charged with it).
//!
//! IN-8: a frame over the 20 ms budget warns only on the paths BitsOS draws itself (`Path::owned`); a program's
//! own frame (`Path::Program`) is its drawing, counted the same but summed up per program at Info (`program`).

pub mod program;

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use ksync::SpinLock;
pub use program::{Reader, Window};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Path {
    Console = 0,
    Pointer = 1,
    PointerOverProgram = 2,
    Program = 3,
}

pub const PATHS: [Path; 4] = [Path::Console, Path::Pointer, Path::PointerOverProgram, Path::Program];

impl Path {
    pub fn name(self) -> &'static str {
        match self {
            Path::Console => "console",
            Path::Pointer => "pointer",
            Path::PointerOverProgram => "pointer_over_program",
            Path::Program => "program",
        }
    }

    /// BitsOS draws the frame itself: the console, the pointer, and the arrow it draws over a program's picture
    /// (the program's drawing does not hold it up). A program's own frame is not BitsOS's to make faster.
    pub fn owned(self) -> bool {
        !matches!(self, Path::Program)
    }
}

/// What a frame's input-to-screen time means.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    InBudget,
    /// Over the budget on a path BitsOS owns: a warning (rate-limited 1, 2, 4, ...).
    Late,
    /// Over the budget in a program's own drawing: counted, summed up per program at Info.
    ProgramLate,
    /// Older than `STALE_NS`: not the input's reaction, outside the numbers.
    Stale,
}

pub fn classify(path: Path, total_ns: u64) -> Verdict {
    if total_ns > STALE_NS {
        Verdict::Stale
    } else if total_ns <= BUDGET_NS {
        Verdict::InBudget
    } else if path.owned() {
        Verdict::Late
    } else {
        Verdict::ProgramLate
    }
}

/// One frame: times in ns of the one clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sample {
    pub path: Path,
    pub arrival_ns: u64,
    pub picked_ns: u64,
    pub start_ns: u64,
    pub done_ns: u64,
}

impl Sample {
    pub fn total_ns(&self) -> u64 {
        self.start_ns.saturating_sub(self.arrival_ns)
    }
}

/// log2 buckets of microseconds: bucket i holds [2^i, 2^(i+1)) µs (bucket 0 also < 1 µs); 40 reach far past
/// any wait the clock can show.
pub const BUCKETS: usize = 40;
/// A sample older than this is counted as stale, not in the numbers: the input drew nothing for so long that
/// the frame is not its reaction (a program that read a key and showed something else much later).
pub const STALE_NS: u64 = 1_000_000_000;
/// The B4 budget (echo < 20 ms under load): a frame above it is reported (rate-limited).
pub const BUDGET_NS: u64 = 20_000_000;
/// How many recent frames are kept until `set_keep` changes it.
pub const KEEP_DEFAULT: usize = 64;

#[derive(Clone, Debug)]
pub struct PathStats {
    pub frames: u64,
    pub stale: u64,
    pub over_budget: u64,
    pub min_ns: u64,
    pub max_ns: u64,
    pub sum_ns: u64,
    pub queue_sum_ns: u64,
    pub draw_sum_ns: u64,
    pub copy_sum_ns: u64,
    pub hist: [u64; BUCKETS],
}

impl PathStats {
    const fn new() -> PathStats {
        PathStats { frames: 0, stale: 0, over_budget: 0, min_ns: u64::MAX, max_ns: 0, sum_ns: 0, queue_sum_ns: 0, draw_sum_ns: 0, copy_sum_ns: 0, hist: [0; BUCKETS] }
    }

    fn add(&mut self, s: &Sample) {
        let t = s.total_ns();
        if t > STALE_NS {
            self.stale += 1;
            return;
        }
        self.frames += 1;
        self.over_budget += (t > BUDGET_NS) as u64;
        self.min_ns = self.min_ns.min(t);
        self.max_ns = self.max_ns.max(t);
        self.sum_ns += t;
        self.queue_sum_ns += s.picked_ns.saturating_sub(s.arrival_ns);
        self.draw_sum_ns += s.start_ns.saturating_sub(s.picked_ns);
        self.copy_sum_ns += s.done_ns.saturating_sub(s.start_ns);
        self.hist[bucket(t)] += 1;
    }

    /// Upper bound (ns) of the bucket holding the `pct` percentile; 0 without frames.
    pub fn percentile_ns(&self, pct: u64) -> u64 {
        if self.frames == 0 {
            return 0;
        }
        let want = (self.frames * pct).div_ceil(100).max(1);
        let mut seen = 0;
        for (i, n) in self.hist.iter().enumerate() {
            seen += n;
            if seen >= want {
                return (2u64 << i).saturating_mul(1000).min(self.max_ns);
            }
        }
        self.max_ns
    }
}

fn bucket(ns: u64) -> usize {
    let us = ns / 1000;
    if us < 2 { 0 } else { (us.ilog2() as usize).min(BUCKETS - 1) }
}

struct Store {
    paths: [PathStats; 4],
    recent: VecDeque<Sample>,
    keep: usize,
}

static STORE: SpinLock<Store> =
    SpinLock::named("console.latency", Store { paths: [const { PathStats::new() }; 4], recent: VecDeque::new(), keep: KEEP_DEFAULT });

/// The VT's input thread (its id): only its frames are console samples.
static INPUT_THREAD: AtomicU64 = AtomicU64::new(0);
/// Earliest arrival of input the input thread took and no frame has shown yet (0 = none), and when it took it.
static PENDING: AtomicU64 = AtomicU64::new(0);
static PICKED: AtomicU64 = AtomicU64::new(0);
/// The same for events programs read from /dev/input (fbdev's frame hook ends them), with who read them: one
/// lock keeps the three together, so a frame is never charged to a reader that came after its swap. Programs'
/// reads and the present hook take it, never the input thread.
#[derive(Clone, Copy)]
struct ProgramPending {
    arrival: u64,
    picked: u64,
    who: Reader,
}
impl ProgramPending {
    const NONE: ProgramPending = ProgramPending { arrival: 0, picked: 0, who: Reader::UNKNOWN };
}
static PROGRAM: SpinLock<ProgramPending> = SpinLock::named("console.latency.program", ProgramPending::NONE);
/// Late frames on the paths BitsOS owns (the warning's 1, 2, 4, ... count).
static SLOW: AtomicU64 = AtomicU64::new(0);
/// IN-8: the per-program windows; taken only for program frames and by the tool, never by the input thread.
static PROGRAMS: SpinLock<program::Programs> = SpinLock::named("console.latency.programs", program::Programs::new());
/// Program summaries logged since the last reset.
static SUMMARIES: AtomicU64 = AtomicU64::new(0);

klog::counter!(FRAMES, "input.latency.frames");
klog::counter!(OVER_BUDGET, "input.latency.over_budget");

/// Called by the VT's input thread once at its start.
pub fn set_input_thread() {
    INPUT_THREAD.store(sched::current_id(), Ordering::Release);
}

fn on_input_thread() -> bool {
    let t = INPUT_THREAD.load(Ordering::Acquire);
    t != 0 && t == sched::current_id()
}

/// Keeps the earliest of `arrival` in (pending, picked); 0 arrivals carry no time and are ignored.
fn note(pending: &AtomicU64, picked: &AtomicU64, arrival: u64, at: u64) -> bool {
    if arrival != 0 && pending.compare_exchange(0, arrival, Ordering::AcqRel, Ordering::Acquire).is_ok() {
        picked.store(at, Ordering::Release);
        return true;
    }
    false
}

/// The input thread took an event that arrived at `arrival` (its evdev time) at `picked`.
pub fn consumed(arrival: u64, picked: u64) {
    note(&PENDING, &PICKED, arrival, picked);
}

/// The input thread finished a batch: input that drew nothing is forgotten.
pub fn batch_done() {
    PENDING.store(0, Ordering::Release);
}

/// A program read events at `at`; `arrival` the earliest of them. `who` names the reading program and is asked
/// only when this read opens the pending slot (once a frame), and outside the lock. A second program's read before
/// the next frame is not charged (one slot: the first reader since the last frame owns it).
pub fn program_read(arrival: u64, at: u64, who: impl FnOnce() -> Reader) {
    if arrival == 0 || PROGRAM.lock().arrival != 0 {
        return;
    }
    let who = who();
    let mut p = PROGRAM.lock();
    if p.arrival == 0 {
        *p = ProgramPending { arrival, picked: at, who };
    }
}

/// The start of a frame's copy, with the input it shows.
#[derive(Clone, Copy, Debug)]
pub struct Stamp {
    arrival: u64,
    picked: u64,
    start: u64,
}

/// A console frame leaves now: the input it shows (None when it shows none, or off the input thread).
pub fn leaving() -> Option<Stamp> {
    if !on_input_thread() {
        return None;
    }
    let arrival = PENDING.swap(0, Ordering::AcqRel);
    (arrival != 0).then(|| Stamp { arrival, picked: PICKED.load(Ordering::Acquire), start: time::now_ns() })
}

/// The frame did not leave after all (the screen was held): the input stays pending.
pub fn not_shown(s: Option<Stamp>) {
    if let Some(s) = s {
        note(&PENDING, &PICKED, s.arrival, s.picked);
    }
}

/// The frame's copy is done.
pub fn left(s: Option<Stamp>, path: Path) {
    if let Some(s) = s {
        record(Sample { path, arrival_ns: s.arrival, picked_ns: s.picked, start_ns: s.start, done_ns: time::now_ns() });
    }
}

/// A program's frame left (`start_ns`) and was copied (`done_ns`): DP's fbdev hook (change request IN-6/2).
pub fn program_frame(start_ns: u64, done_ns: u64) {
    let p = core::mem::replace(&mut *PROGRAM.lock(), ProgramPending::NONE);
    if p.arrival != 0 {
        record_as(Sample { path: Path::Program, arrival_ns: p.arrival, picked_ns: p.picked, start_ns, done_ns }, p.who);
    }
}

pub fn record(s: Sample) {
    record_as(s, Reader::UNKNOWN);
}

/// `who`: the program that read the input (program frames only).
pub fn record_as(s: Sample, who: Reader) {
    let t = s.total_ns();
    {
        let mut st = STORE.lock();
        st.paths[s.path as usize].add(&s);
        while st.recent.len() >= st.keep.max(1) {
            st.recent.pop_front();
        }
        if st.recent.try_reserve(1).is_ok() {
            st.recent.push_back(s);
        }
    }
    FRAMES.inc();
    match classify(s.path, t) {
        Verdict::Late => {
            OVER_BUDGET.inc();
            let n = SLOW.fetch_add(1, Ordering::Relaxed) + 1;
            if n.is_power_of_two() {
                klog::event!(Warn, "input", "latency.over_budget", path = s.path.name(), us = t / 1000, count = n;
                    "a frame showed input later than the 20 ms budget");
            }
        }
        Verdict::ProgramLate => OVER_BUDGET.inc(),
        Verdict::InBudget | Verdict::Stale => {}
    }
    if s.path == Path::Program {
        program_seen(who, t, time::now_ns());
    }
}

/// Puts a program frame into its program's window, after closing the windows a minute old.
fn program_seen(who: Reader, t: u64, now: u64) {
    flush_programs(now);
    loop {
        let want = {
            let mut p = PROGRAMS.lock();
            if p.add(who, t, now) {
                return;
            }
            p.room_wanted()
        };
        let mut more: Vec<Window> = Vec::new();
        if more.try_reserve_exact(want).is_err() {
            klog::event!(Warn, "input", "latency.program_no_room", pid = who.pid, windows = want;
                "no memory for a program's latency window; its frame stays in the path numbers only");
            return;
        }
        let old = PROGRAMS.lock().grow(more);
        drop(old);
    }
}

/// Closes the programs' windows that are a minute old: one Info summary for each with late frames. Runs at
/// every program frame and when the tool reads, so a program that stopped drawing is summed up at the next one.
pub fn flush_programs(now: u64) {
    loop {
        let Some(w) = PROGRAMS.lock().take_due(now) else { return };
        if w.late > 0 {
            SUMMARIES.fetch_add(1, Ordering::Relaxed);
            klog::event!(Info, "input", "latency.program_late", pid = w.who.pid, comm = w.who.name(), frames = w.frames,
                late = w.late, worst_us = w.worst_ns / 1000, median_us = w.median_ns() / 1000,
                window_s = now.saturating_sub(w.start_ns) / 1_000_000_000;
                "a program drew input later than 20 ms (its own drawing; BitsOS's paths keep the budget)");
        }
    }
}

/// The open program windows (after closing the due ones) and how many summaries were logged since the reset.
/// Room is reserved before the lock, as in `report`.
pub fn programs() -> (Vec<Window>, u64) {
    flush_programs(time::now_ns());
    let want = PROGRAMS.lock().windows().len();
    let mut out: Vec<Window> = Vec::new();
    if out.try_reserve_exact(want).is_ok() {
        let p = PROGRAMS.lock();
        out.extend(p.windows().iter().take(want).cloned());
    }
    (out, SUMMARIES.load(Ordering::Relaxed))
}

/// Copy of the numbers: per path, and the recent frames (oldest first). The input thread (KernelRt) records under
/// the same spin lock, so the lock covers only fixed-size copies: the room for the recent frames is reserved
/// before it (a frame recorded in between may be left out), and no allocation happens under it.
pub fn report() -> (Vec<(Path, PathStats)>, Vec<Sample>) {
    let want = { STORE.lock().recent.len() };
    let mut recent: Vec<Sample> = Vec::new();
    let room = recent.try_reserve_exact(want).is_ok();
    let stats = {
        let st = STORE.lock();
        if room {
            let (a, b) = st.recent.as_slices();
            let skip = (a.len() + b.len()).saturating_sub(want);
            let (a_skip, b_skip) = (skip.min(a.len()), skip.saturating_sub(a.len()));
            recent.extend_from_slice(&a[a_skip..]);
            recent.extend_from_slice(&b[b_skip..]);
        }
        st.paths.clone()
    };
    let paths = PATHS.iter().map(|&p| (p, stats[p as usize].clone())).collect();
    (paths, recent)
}

/// Forgets every number (a measurement starts).
pub fn reset() {
    {
        let mut st = STORE.lock();
        st.paths = [const { PathStats::new() }; 4];
        st.recent.clear();
    }
    SLOW.store(0, Ordering::Relaxed);
    *PROGRAM.lock() = ProgramPending::NONE;
    let old = PROGRAMS.lock().clear();
    drop(old);
    SUMMARIES.store(0, Ordering::Relaxed);
    klog::event!(Info, "input", "latency.reset");
}

/// The warning's count of late frames on BitsOS's own paths.
#[cfg(test)]
pub(crate) fn late_owned() -> u64 {
    SLOW.load(Ordering::Relaxed)
}

/// How many recent frames are kept (at least 1).
pub fn set_keep(n: usize) {
    let n = n.max(1);
    let old = core::mem::replace(&mut STORE.lock().keep, n);
    if old != n {
        klog::event!(Info, "input", "latency.keep", frames = n);
    }
}

pub fn keep() -> usize {
    STORE.lock().keep
}
