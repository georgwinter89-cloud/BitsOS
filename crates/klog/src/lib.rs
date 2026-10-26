//! Structured events and the kernel log (owner: RB; contract `B6 event!`, V2-PLAN §1a, §4).
//!
//! Every log line is an event: subsystem, level, name, typed key/value fields and an optional message, stamped
//! with one clock, the CPU and the thread. The text for the screen, serial and the mirror is rendered from it
//! (`[  12.345678] I net link.up if=igc0 speed_mbps=1000`). Lanes emit events at state changes, errors,
//! retries, timeouts and fallbacks (B6 rule, checked by the checkers).
//!
//! RB-1:
//! - The ring stores records (header + rendered text) by sequence number. Before the heap a static early ring
//!   (`EARLY_BYTES`) takes them; `heap_ready` moves them into a heap ring of `RING_BYTES`, which can be resized
//!   later (`size_for_memory`, CONFIG `log_buf=`). Both are byte budgets, never a record count (R1).
//! - Emitting never allocates (the line is rendered on the stack) and never waits for long: the ring lock is
//!   tried for a bounded time with this CPU's interrupts masked, so an IRQ handler never meets its own CPU
//!   holding it; a nested emit from an NMI/#MC on that CPU gives up at once. A record that could not be stored
//!   still consumes its sequence number, so readers see the hole.
//! - RB-klog (CONTRACTS §4 R5): the ring lock covers only the copy of one record in (an emitter) or of a small
//!   batch out into memory reserved before (readers); rendering, allocation, serial and every sink run outside
//!   it, so a waiter waits microseconds (`lock_stats`). Serial is printed from the ring by one CPU at a time
//!   (`serial`), the deferred sinks before klogd as well (`sinks::catch_up`), so no emitter waits for another
//!   CPU's output.
//! - Text from outside is rendered safely: string fields are quoted and escaped when needed, control
//!   characters in messages are escaped, so one record is always one line.
//! - `try_event!` is the panic/NMI form: one try of the ring lock, no per-subsystem table, sinks only through
//!   `Sink::write_emergency` (desk decision, CONTRACTS.md §6a).
//! - Levels per subsystem from CONFIG `loglevel=warn,net=debug` (`levels`).
//! - Sinks: serial built in, inline sinks (G0 `add_sink`) and deferred sinks fed by the klogd thread (`sinks`);
//!   until klogd runs (`set_drainer`), deferred sinks are fed in order by the emitter, so the boot is on screen.
//! - Readers by sequence number with filters (`read`, `read_tail`); MCP `log` lives in the klogd crate because
//!   `mcp` depends on klog.
//!
//! Lock order (CONTRACTS.md §4): klog ring > frames/heap; klog never calls the scheduler.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

#[macro_use]
mod macros;
#[macro_use]
pub mod metrics;
pub mod ktests;
mod levels;
mod lock;
mod config;
pub mod fields;
pub mod keys;
pub mod journal;
pub mod ratelimit;
mod reader;
mod render;
pub mod ring;
mod serial;
mod sinks;
mod tail;
pub mod trace;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use ring::{Hdr, RingCore, RingOps};

pub use config::{configure, parse_size, ring_bytes_for_memory, size_for_memory};
pub use levels::{Spec as LevelSpec, parse as parse_levels};
use lock::{EMIT_WAIT_NS, Held, KLock, READ_WAIT_NS, Wait};
pub(crate) use lock::cpu_tag;
pub use lock::Stats as LockStats;
pub use serial::pending as serial_pending;
pub use fields::FieldFilter;
pub use reader::{Filter, Gap, Page, read, read_span, read_tail, read_tail_span};
pub use render::LINE_BYTES;
pub use tail::{try_tail_text, try_tail_text_before};
pub use sinks::{SinkId, add_deferred_sink, add_sink, drain_deferred, drainer, pending, remove_sink, set_drainer};
#[doc(hidden)]
pub use kreg as __kreg;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Level {
    Error = 0,
    Warn = 1,
    Info = 2,
    Debug = 3,
    Trace = 4,
}

impl Level {
    pub fn letter(self) -> char {
        match self {
            Level::Error => 'E',
            Level::Warn => 'W',
            Level::Info => 'I',
            Level::Debug => 'D',
            Level::Trace => 'T',
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Level::Error => "error",
            Level::Warn => "warn",
            Level::Info => "info",
            Level::Debug => "debug",
            Level::Trace => "trace",
        }
    }
    pub fn parse(s: &str) -> Option<Level> {
        Some(match s {
            "error" => Level::Error,
            "warn" => Level::Warn,
            "info" => Level::Info,
            "debug" => Level::Debug,
            "trace" => Level::Trace,
            _ => return None,
        })
    }
}

/// A field value. Strings are copied into the record; secrets must never be passed (R8i, `wlan_psk`).
#[derive(Clone, Copy, Debug)]
pub enum Value<'a> {
    U64(u64),
    I64(i64),
    Hex(u64),
    Bool(bool),
    Str(&'a str),
}

impl fmt::Display for Value<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::U64(v) => write!(f, "{v}"),
            Value::I64(v) => write!(f, "{v}"),
            Value::Hex(v) => write!(f, "{v:#x}"),
            Value::Bool(v) => write!(f, "{v}"),
            // Quoted (with control characters escaped) whenever the plain text could be misread: strings come
            // from devices, files and the network, and must never start a new line or fake a field.
            Value::Str(s) if needs_quotes(s) => write!(f, "{s:?}"),
            Value::Str(s) => f.write_str(s),
        }
    }
}

fn needs_quotes(s: &str) -> bool {
    s.is_empty() || s.chars().any(|c| c.is_whitespace() || c.is_control() || matches!(c, '"' | '=' | '\\'))
}

pub trait IntoValue {
    fn into_value(&self) -> Value<'_>;
}
macro_rules! iv {
    ($($t:ty => $v:ident),*) => { $(impl IntoValue for $t { fn into_value(&self) -> Value<'_> { Value::$v((*self).into()) } })* };
}
iv!(u8 => U64, u16 => U64, u32 => U64, u64 => U64, i8 => I64, i16 => I64, i32 => I64, i64 => I64, bool => Bool);
impl IntoValue for usize {
    fn into_value(&self) -> Value<'_> {
        Value::U64(*self as u64)
    }
}
impl IntoValue for isize {
    fn into_value(&self) -> Value<'_> {
        Value::I64(*self as i64)
    }
}
impl IntoValue for str {
    fn into_value(&self) -> Value<'_> {
        Value::Str(self)
    }
}
impl IntoValue for &str {
    fn into_value(&self) -> Value<'_> {
        Value::Str(self)
    }
}
impl IntoValue for String {
    fn into_value(&self) -> Value<'_> {
        Value::Str(self.as_str())
    }
}
/// Wrap a number to print it in hex: `addr = klog::Hex(x)`.
pub struct Hex(pub u64);
impl IntoValue for Hex {
    fn into_value(&self) -> Value<'_> {
        Value::Hex(self.0)
    }
}

pub struct Field<'a> {
    pub key: &'static str,
    pub value: Value<'a>,
}

pub struct Event<'a> {
    pub level: Level,
    pub subsystem: &'static str,
    pub name: &'static str,
    pub fields: &'a [Field<'a>],
    pub message: Option<fmt::Arguments<'a>>,
}

/// One stored record, as readers get it.
#[derive(Clone, Debug)]
pub struct Record {
    pub seq: u64,
    pub time_ns: u64,
    pub cpu: u32,
    /// Scheduler thread id (0 before the scheduler runs).
    pub thread: u64,
    pub level: Level,
    pub subsystem: &'static str,
    pub name: &'static str,
    /// Rendered `key=value ...` and message.
    pub text: String,
}

impl Record {
    pub fn render(&self) -> String {
        let mut s = String::new();
        render::prefix(&mut s, self.time_ns, self.level, self.subsystem, self.name);
        if !self.text.is_empty() {
            s.push(' ');
            s.push_str(&self.text);
        }
        s
    }
}

/// Where rendered lines go (serial, framebuffer console, mirror).
pub trait Sink: Sync {
    fn write_line(&self, level: Level, line: &str);
    /// The panic/NMI path (`try_event!`): no locks, no allocation, never wait. Default: nothing (the line is
    /// on serial and, if the ring was free, in the ring). A deferred sink may see such a line twice.
    fn write_emergency(&self, _level: Level, _line: &str) {}
    /// A deferred sink fell behind the ring: `records` records it will never get. Default: the line
    /// `[... n records lost]`; the network mirror (RB-4) overrides it with its own wire marker.
    fn lost(&self, records: u64) {
        let mut b = render::LineBuf::new();
        let _ = fmt::Write::write_fmt(&mut b, format_args!("[... {records} records lost]"));
        self.write_line(Level::Warn, b.as_str());
    }
    /// The panic path's last step (`flush_emergency`): make what `write_emergency` handed to a device leave
    /// it (the mirror waits a few ms for the network card). Same rules as `write_emergency`. Default: nothing.
    fn flush_emergency(&self) {}
}

counter!(RECORDS, "klog.records");
counter!(LOST, "klog.lost");
counter!(CUT, "klog.cut");

static HEAP_READY: AtomicBool = AtomicBool::new(false);
static SEQ: AtomicU64 = AtomicU64::new(0);
static EXPLICIT_SIZE: AtomicBool = AtomicBool::new(false);

/// Bytes of the static early ring (before the heap; a memory budget, R1).
pub const EARLY_BYTES: usize = 64 << 10;
/// Bytes of the heap ring at `heap_ready` (V2-PLAN §4: 4 MB) until `size_for_memory` or `log_buf=` resize it.
pub const RING_BYTES: usize = 4 << 20;
/// Smallest and largest ring `resize` accepts (bounds against a bad `log_buf=`).
pub const RING_MIN: usize = 64 << 10;
pub const RING_MAX: usize = 1 << 30;

const EARLY: (usize, usize) = ring::split(EARLY_BYTES);
type EarlyRing = RingCore<[Hdr; EARLY.0], [u8; EARLY.1]>;
type HeapRing = RingCore<Box<[Hdr]>, Box<[u8]>>;

pub(crate) struct Rings {
    early: EarlyRing,
    heap: Option<HeapRing>,
}

impl Rings {
    fn active(&mut self) -> &mut dyn RingOps {
        match &mut self.heap {
            Some(h) => h,
            None => &mut self.early,
        }
    }
    pub(crate) fn active_ref(&self) -> &dyn RingOps {
        match &self.heap {
            Some(h) => h,
            None => &self.early,
        }
    }
}

static RING: KLock<Rings> =
    KLock::named("klog.ring", Rings { early: RingCore::new([Hdr::EMPTY; EARLY.0], [0; EARLY.1]), heap: None });

/// The ring lock, held with this CPU's interrupts masked (an IRQ handler that logs never meets its own CPU
/// holding it; only NMI/#MC can, and they use `Wait::Never`).
pub(crate) fn lock_ring(wait: Wait) -> Option<Held<'static, Rings>> {
    RING.lock(wait)
}

/// Longest waits for and holds of the ring lock since boot or the last `reset_lock_stats` (the `log` tool shows
/// them; ktest `rb.klog.smp_storm` checks them).
pub fn lock_stats() -> LockStats {
    RING.stats()
}

pub fn reset_lock_stats() {
    RING.reset_stats();
}

/// Prints stored records serial has not had yet, at most `budget` lines, unless another CPU prints right now
/// (klogd after a storm; the lines are printed by the emitters otherwise). Returns the lines printed.
pub fn flush_serial(budget: usize) -> usize {
    serial::flush(None, budget)
}

/// The panic path's first step: serial gets the stored lines it has not had yet (at most `max`), before the
/// report, within `serial::EMERGENCY_BUDGET_NS`. No locks waited for, no allocation (`serial::flush_emergency`).
/// Returns the lines printed.
pub fn flush_serial_emergency(max: usize) -> usize {
    serial::flush_emergency(max)
}

static CLOCK_FN: ksync::Once<fn() -> u64> = ksync::Once::new();

/// Installs the monotonic clock (ns) used for time stamps (once; `time` does it at init).
pub fn install_clock(f: fn() -> u64) {
    CLOCK_FN.get_or_init(|| f);
}

/// The log's clock: ns since boot, the `[time]` of every line (0 before `install_clock`).
pub fn now() -> u64 {
    CLOCK_FN.get().map(|f| f()).unwrap_or(0)
}

/// Sets the default level (subsystem settings from `set_levels`/`configure` stay).
pub fn set_min_level(l: Level) {
    levels::set_default(l);
}

/// The most verbose level any setting lets through (the `event!` macro's first check).
pub fn min_level() -> Level {
    levels::most_verbose()
}

/// Applies a `loglevel=` value (`warn,net=debug`); the error names the bad part.
pub fn set_levels(spec: &str) -> Result<(), String> {
    levels::apply(levels::parse(spec)?);
    event!(Info, "klog", "levels.set", levels = levels::describe());
    Ok(())
}

/// The current level settings as a `loglevel=` value.
pub fn levels() -> String {
    levels::describe()
}

/// Serial output on or off (QEMU: on; the Mini-PC has no COM1, harmless either way).
pub fn set_serial(on: bool) {
    serial::set_on(on);
}

/// Records an event. Use the `event!` macro.
pub fn emit(ev: &Event<'_>) {
    let _ = record(ev, false);
}

/// The panic/NMI form (`try_event!`): never waits for a lock, never allocates. True if the record reached the
/// ring; it always reaches serial and every sink's `write_emergency`.
pub fn try_emit(ev: &Event<'_>) -> bool {
    record(ev, true)
}

fn record(ev: &Event<'_>, emergency: bool) -> bool {
    if ev.level > levels::effective(ev.subsystem, emergency) {
        return false;
    }
    let time_ns = now();
    // RB-7a: a storm of one event stops here, before anything is rendered or locked (the panic/NMI path is never
    // limited: the crash report must be whole).
    if !emergency && !ratelimit::admit(ev.subsystem, ev.name, time_ns) {
        return false;
    }
    let mut line = render::LineBuf::new();
    let text_start = render::line(&mut line, time_ns, ev);
    if line.was_cut() {
        CUT.inc();
    }
    let s = line.as_str();
    let h = Hdr {
        time_ns,
        thread: hal::percpu::current(),
        cpu: hal::percpu::this_cpu() as u32,
        level: ev.level,
        // The panic/NMI path writes its line itself (below); the serial printer must not repeat it.
        direct: emergency && serial::on(),
        subsystem: ev.subsystem,
        name: ev.name,
        ..Hdr::EMPTY
    };
    let wait = if emergency { Wait::Never } else { Wait::Bounded(EMIT_WAIT_NS) };
    let (stored, seq) = match lock_ring(wait) {
        Some(mut r) => {
            // Taken under the lock, so the ring stays sorted by sequence number. SeqCst: the serial printer and
            // the deferred-sink feeder check for new records after letting go of their flag (see `serial`).
            let seq = SEQ.fetch_add(1, Ordering::SeqCst);
            r.active().push(Hdr { seq, ..h }, &s.as_bytes()[text_start..]);
            (true, seq)
        }
        None => {
            let seq = SEQ.fetch_add(1, Ordering::SeqCst);
            LOST.inc();
            (false, seq)
        }
    };
    if stored {
        RECORDS.inc();
    }
    if emergency || !stored {
        // Straight out: the panic/NMI path never waits for the printer, and a record the ring did not take
        // exists only here. hal's line lock keeps it apart from other lines.
        serial::unstored(s);
    } else {
        // Printed by whichever CPU prints right now (maybe this one, maybe a moment later): never waited for.
        serial::stored(seq, s);
    }
    if emergency {
        sinks::emergency(ev.level, s);
    } else {
        sinks::inline(ev.level, s);
    }
    stored
}

/// Records that could not be stored since boot (`ring_info().lost` without taking the ring lock).
pub fn lost() -> u64 {
    LOST.get()
}

pub fn next_seq() -> u64 {
    SEQ.load(Ordering::SeqCst)
}

/// Records with `seq >= since`, at most `max`, oldest first (G0 API; `read` also reports gaps and filters).
pub fn read_since(since: u64, max: usize) -> Vec<Record> {
    read(since, max, &Filter::ALL).records
}

/// The last `n` records.
pub fn tail(n: usize) -> Vec<Record> {
    read_tail(n, &Filter::ALL).records
}

#[derive(Clone, Debug)]
pub struct RingInfo {
    /// Bytes of the ring in use (headers + text store).
    pub bytes: usize,
    /// Still the static early ring (no heap yet, or the heap ring could not be allocated).
    pub early: bool,
    pub records: u64,
    pub oldest_seq: u64,
    pub next_seq: u64,
    /// Records that could not be stored (ring busy on the panic/NMI path or held too long).
    pub lost: u64,
    /// The size came from CONFIG `log_buf=` (else from memory or the default).
    pub sized_by_config: bool,
    pub levels: String,
    /// The ring lock could not be had in time: the ring fields above are zero, not real.
    pub busy: bool,
    /// (name, deferred, cursor) of every sink.
    pub sinks: Vec<(&'static str, bool, u64)>,
    /// Longest waits for and holds of the ring lock (`lock_stats`).
    pub lock: LockStats,
}

pub fn ring_info() -> RingInfo {
    let (bytes, early, records, oldest, busy) = match lock_ring(Wait::Bounded(READ_WAIT_NS)) {
        Some(g) => {
            let r = g.active_ref();
            let (a, b) = r.span();
            let oldest = if a == b { next_seq() } else { r.hdr(a).seq };
            (r.bytes(), g.heap.is_none(), b - a, oldest, false)
        }
        None => (0, false, 0, 0, true),
    };
    RingInfo {
        bytes,
        early,
        records,
        oldest_seq: oldest,
        next_seq: next_seq(),
        lost: LOST.get(),
        sized_by_config: EXPLICIT_SIZE.load(Ordering::Acquire),
        levels: levels::describe(),
        busy,
        sinks: sinks::list(),
        lock: lock_stats(),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RingError {
    NoMemory,
    /// Called from inside klog on this CPU (e.g. a sink resizing the ring), or the ring lock was held too long.
    Busy,
}

fn alloc_slice<T: Clone>(n: usize, v: T) -> Result<Box<[T]>, RingError> {
    let mut s = Vec::new();
    s.try_reserve_exact(n).map_err(|_| RingError::NoMemory)?;
    s.resize(n, v);
    Ok(s.into_boxed_slice())
}

/// Records copied per ring-lock hold by `resize` (about 12 KiB of text: a few microseconds), and the rounds
/// beyond the ones the kept records need before it copies the rest at once.
const MIGRATE_CHUNK: u64 = 128;
const MIGRATE_ROUNDS: u64 = 64;

/// Bytes a ring of the requested size really uses (headers are whole).
pub fn ring_size(bytes: usize) -> usize {
    let (n, t) = ring::split(bytes.clamp(RING_MIN, RING_MAX));
    n * core::mem::size_of::<Hdr>() + t
}

/// Moves the log into a new heap ring of `bytes` (clamped to `RING_MIN..=RING_MAX`), keeping every record that
/// fits (the newest ones when it shrinks). Returns the bytes now in use. Needs the heap.
pub fn resize(bytes: usize) -> Result<usize, RingError> {
    let (n, t) = ring::split(bytes.clamp(RING_MIN, RING_MAX));
    let mut new: HeapRing = RingCore::new(alloc_slice(n, Hdr::EMPTY)?, alloc_slice(t, 0u8)?);
    let size = new.bytes();
    // Copied a chunk per lock hold (no allocation under the lock), so emitters on other CPUs never wait long;
    // after `MIGRATE_ROUNDS` the rest goes in one hold, so a busy log cannot keep the resize going forever.
    let kept = {
        let g = lock_ring(Wait::Bounded(READ_WAIT_NS)).ok_or(RingError::Busy)?;
        let (a, b) = g.active_ref().span();
        b - a
    };
    let round_limit = kept / MIGRATE_CHUNK + MIGRATE_ROUNDS;
    let mut since = 0;
    let mut rounds = 0u64;
    let old = loop {
        let mut g = lock_ring(Wait::Bounded(READ_WAIT_NS)).ok_or(RingError::Busy)?;
        rounds += 1;
        let max = if rounds > round_limit { u64::MAX } else { MIGRATE_CHUNK };
        let (next, done) = ring::migrate_chunk(g.active_ref(), &mut new, since, max);
        since = next;
        if done {
            g.early.clear();
            break g.heap.replace(new);
        }
    };
    // Freed after the ring lock is gone, so freeing never stretches the hold time.
    drop(old);
    Ok(size)
}

/// Kernel init: the heap exists. Moves the early records into the heap ring.
pub fn heap_ready() {
    if HEAP_READY.swap(true, Ordering::AcqRel) {
        return;
    }
    let early = ring_info();
    event!(Info, "klog", "ring.early", records = early.records + 1, bytes = EARLY_BYTES, lost = early.lost);
    let early = ring_info();
    match resize(RING_BYTES) {
        Ok(bytes) => {
            let after = ring_info();
            EARLY_MOVE.get_or_init(|| EarlyMove {
                records: early.records,
                oldest: early.oldest_seq,
                moved: after.records,
                oldest_after: after.oldest_seq,
            });
            event!(Info, "klog", "ring.ready", bytes = bytes, first_seq = after.oldest_seq, moved = after.records);
        }
        Err(e) => event!(Error, "klog", "ring.fallback", error = alloc::format!("{e:?}"); "keeping the early ring"),
    }
}

/// What `heap_ready` moved from the static early ring into the heap ring (proof that no boot record was lost
/// there, however much the boot logs afterwards).
#[derive(Clone, Copy, Debug)]
pub struct EarlyMove {
    /// Records in the early ring right before the move, and the oldest sequence number among them.
    pub records: u64,
    pub oldest: u64,
    /// Records in the heap ring right after the move, and its oldest sequence number.
    pub moved: u64,
    pub oldest_after: u64,
}

static EARLY_MOVE: ksync::Once<EarlyMove> = ksync::Once::new();

pub fn early_move() -> Option<EarlyMove> {
    EARLY_MOVE.get().copied()
}

/// The panic path's last step before halt, reset or QEMU exit: every sink gets `Sink::flush_emergency`, so
/// lines sent by `try_event!` leave the machine (RB-4: the mirror's frames leave the network card). No locks,
/// no allocation; bounded (the mirror waits at most `klogd::mirror::SETTLE_NS`).
pub fn flush_emergency() {
    sinks::flush_emergency();
}

/// Panic and NMI path: straight to serial, no locks, no allocation.
pub fn emergency(s: &str) {
    hal::serial::write_bytes_unlocked(s.as_bytes());
}

/// Holds the ring lock while `f` runs (tests of the busy-ring paths).
#[doc(hidden)]
pub fn __with_ring_held<R>(f: impl FnOnce() -> R) -> R {
    let _g = lock_ring(Wait::Bounded(READ_WAIT_NS));
    f()
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod trace_tests;
