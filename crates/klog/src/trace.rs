//! Per-CPU trace rings for high-rate events (RB-7a, B6): context switches, IRQ storms, syscalls slower than a
//! limit, lock waits over a limit, page-fault bursts, and `mark` for any subsystem.
//!
//! An entry is six words, written without a lock (Linux `kernel/trace/ring_buffer.c` in overwrite mode, much
//! simplified): the writer reserves a position with one `fetch_add` on the ring's head, marks the slot as being
//! written, stores the words and publishes the slot with its position (a per-slot sequence lock). Readers on
//! other CPUs check the slot's position before and after copying it, so a torn or overwritten entry is counted
//! as lost, never shown. Writers never wait and never allocate, so the hooks are safe in IRQ and NMI context,
//! under spin locks and before the heap (then they only count `dropped`). Each CPU writes its own ring; the
//! reservation is atomic, so a thread migrated mid-write or klogd writing for another CPU is still correct.
//!
//! Sizes follow memory (R1): `init` gives each possible CPU `bytes_for_memory` (CONFIG `trace_buf=` per CPU).

use crate::Level;
use alloc::boxed::Box;
use core::fmt::Write;
use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering, fence};
use hal::percpu::PerCpu;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    /// Context switch: `a` = previous thread, `b` = next thread.
    Switch = 0,
    /// IRQ storm (klogd's per-CPU sampler): `a` = vector, `b` = interrupts per second.
    Irq = 1,
    /// Syscall slower than `Limits::syscall_ns` without blocking: `a` = nr | ret << 32, `b` = ns.
    Syscall = 2,
    /// Lock wait over `Limits::lock_wait_ns`: name id = the lock, `a` = lock address, `b` = ns.
    LockWait = 3,
    /// Page-fault burst (`Limits::faults_per_window` in one window on one CPU): `a` = address, `b` = faults.
    Fault = 4,
    /// `mark`: name id = (subsystem, name), `a`/`b` free.
    Mark = 5,
}

const KINDS: [Kind; 6] = [Kind::Switch, Kind::Irq, Kind::Syscall, Kind::LockWait, Kind::Fault, Kind::Mark];

impl Kind {
    fn from_u8(v: u8) -> Option<Kind> {
        KINDS.get(v as usize).copied()
    }
    pub fn word(self) -> &'static str {
        ["switch", "irq", "syscall", "lock", "fault", "mark"][self as usize]
    }
    pub fn parse(s: &str) -> Option<Kind> {
        KINDS.iter().copied().find(|k| k.word() == s)
    }
    fn bit(self) -> u32 {
        1 << self as u32
    }
}

/// When the hooks record (CONFIG `trace_syscall_ms=`, `trace_lock_us=`, `trace_irq_rate=`, `trace_faults=`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub syscall_ns: u64,
    pub lock_wait_ns: u64,
    /// Interrupts per second on one CPU that make a storm.
    pub irq_per_s: u64,
    /// Faults in one `FAULT_WINDOW_NS` on one CPU that make a burst.
    pub faults_per_window: u64,
}

impl Limits {
    pub const DEFAULT: Limits = Limits { syscall_ns: 10_000_000, lock_wait_ns: 100_000, irq_per_s: 50_000, faults_per_window: 1000 };
}

pub const FAULT_WINDOW_NS: u64 = 100_000_000;

static SYSCALL_NS: AtomicU64 = AtomicU64::new(Limits::DEFAULT.syscall_ns);
static LOCK_NS: AtomicU64 = AtomicU64::new(Limits::DEFAULT.lock_wait_ns);
static IRQ_PER_S: AtomicU64 = AtomicU64::new(Limits::DEFAULT.irq_per_s);
static FAULTS: AtomicU64 = AtomicU64::new(Limits::DEFAULT.faults_per_window);
/// TSC ticks per second (klogd sets it from `time`), for lock waits measured in ticks.
static TSC_HZ: AtomicU64 = AtomicU64::new(0);
/// Kinds recorded by default: all but `switch`, which comes at 100 000s per second under load and would push
/// the rare entries (storms, lock waits, bursts) out of the ring; CONFIG `trace=all` or `trace=switch,...`.
pub const DEFAULT_KINDS: u32 = !(1 << Kind::Switch as u32);
static ENABLED: AtomicU32 = AtomicU32::new(DEFAULT_KINDS);
static DROPPED: AtomicU64 = AtomicU64::new(0);
/// Slots per CPU ring for `make` (0 until `init`).
static SLOTS: AtomicUsize = AtomicUsize::new(0);
static EXPLICIT: AtomicUsize = AtomicUsize::new(0);

pub fn limits() -> Limits {
    Limits {
        syscall_ns: SYSCALL_NS.load(Ordering::Relaxed),
        lock_wait_ns: LOCK_NS.load(Ordering::Relaxed),
        irq_per_s: IRQ_PER_S.load(Ordering::Relaxed),
        faults_per_window: FAULTS.load(Ordering::Relaxed),
    }
}

pub fn set_limits(l: Limits) {
    SYSCALL_NS.store(l.syscall_ns, Ordering::Relaxed);
    LOCK_NS.store(l.lock_wait_ns, Ordering::Relaxed);
    IRQ_PER_S.store(l.irq_per_s.max(1), Ordering::Relaxed);
    FAULTS.store(l.faults_per_window.max(1), Ordering::Relaxed);
}

pub fn set_tsc_hz(hz: u64) {
    TSC_HZ.store(hz, Ordering::Relaxed);
}

/// TSC ticks per second as `lock_wait` converts them (0 until klogd sets it).
pub fn tsc_hz() -> u64 {
    TSC_HZ.load(Ordering::Relaxed)
}

pub fn enabled(k: Kind) -> bool {
    ENABLED.load(Ordering::Relaxed) & k.bit() != 0
}

/// The kinds recorded (CONFIG `trace=`): `all`, `off`, or a list `switch,irq,syscall,lock,fault,mark`.
pub fn set_kinds(spec: &str) -> Result<(), alloc::string::String> {
    let mask = parse_kinds(spec)?;
    ENABLED.store(mask, Ordering::Relaxed);
    Ok(())
}

pub fn parse_kinds(spec: &str) -> Result<u32, alloc::string::String> {
    match spec.trim() {
        "all" | "on" => return Ok(u32::MAX),
        "off" | "none" | "" => return Ok(0),
        _ => {}
    }
    let mut mask = 0;
    for w in spec.split(',') {
        mask |= Kind::parse(w.trim()).ok_or_else(|| alloc::string::String::from(w.trim()))?.bit();
    }
    Ok(mask)
}

/// The kinds recorded, as `set_kinds` takes them.
pub fn kinds() -> alloc::string::String {
    let m = ENABLED.load(Ordering::Relaxed);
    let every = KINDS.iter().fold(0, |a, k| a | k.bit());
    if m & every == every {
        return "all".into();
    }
    let v: alloc::vec::Vec<&str> = KINDS.iter().filter(|k| m & k.bit() != 0).map(|k| k.word()).collect();
    if v.is_empty() { "off".into() } else { v.join(",") }
}

// ---- the ring ----

/// Marks a slot as being written for the entry with `stamp` (odd = in writing). Only a slot that is published
/// (even) with an older entry can be claimed: when a writer stalls between reserving and publishing while the
/// ring wraps past it, the later writer (or the stalled one, coming back to a newer stamp) drops its entry
/// instead of both writing the same slot and publishing a mix.
pub(crate) fn claim(stamp_word: &AtomicU64, stamp: u64) -> bool {
    let mut cur = stamp_word.load(Ordering::Relaxed);
    loop {
        if cur & 1 != 0 || cur >= stamp {
            return false;
        }
        match stamp_word.compare_exchange_weak(cur, stamp | 1, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return true,
            Err(c) => cur = c,
        }
    }
}

struct Slot([AtomicU64; 6]);

const STAMP: usize = 0;

impl Slot {
    fn new() -> Self {
        Slot([const { AtomicU64::new(0) }; 6])
    }
}

/// One CPU's ring (also used alone by tests).
pub struct Ring {
    slots: Box<[Slot]>,
    head: AtomicU64,
    cpu: u32,
    // Page-fault burst window of this CPU (approximate under nesting; it only decides when to record).
    fault_start: AtomicU64,
    faults: AtomicU64,
}

/// One entry as readers get it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    pub cpu: u32,
    /// Position in its ring (counts every entry ever written there).
    pub pos: u64,
    pub time_ns: u64,
    pub kind: Kind,
    pub thread: u64,
    /// Key of the lock or mark (`keys`), 0 when the kind has none.
    pub name_id: u32,
    pub a: u64,
    pub b: u64,
}

impl Ring {
    /// A ring of `slots` entries (rounded up to a power of two, at least 2).
    pub fn new(cpu: u32, slots: usize) -> Ring {
        let n = slots.max(2).next_power_of_two();
        Ring {
            slots: (0..n).map(|_| Slot::new()).collect(),
            head: AtomicU64::new(0),
            cpu,
            fault_start: AtomicU64::new(0),
            faults: AtomicU64::new(0),
        }
    }

    pub fn capacity(&self) -> u64 {
        self.slots.len() as u64
    }

    fn slot(&self, pos: u64) -> &Slot {
        &self.slots[(pos & (self.capacity() - 1)) as usize]
    }

    /// Writes one entry; never waits, never allocates. False if the slot was taken (see `claim`): the entry is
    /// dropped, never mixed with another.
    pub fn push(&self, time_ns: u64, kind: Kind, thread: u64, name_id: u32, a: u64, b: u64) -> bool {
        let pos = self.head.fetch_add(1, Ordering::Relaxed);
        let s = self.slot(pos);
        let stamp = (pos + 1) << 1;
        if !claim(&s.0[STAMP], stamp) {
            DROPPED.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        fence(Ordering::Release);
        s.0[1].store(time_ns, Ordering::Relaxed);
        s.0[2].store(thread, Ordering::Relaxed);
        s.0[3].store(kind as u64 | (name_id as u64) << 8, Ordering::Relaxed);
        s.0[4].store(a, Ordering::Relaxed);
        s.0[5].store(b, Ordering::Relaxed);
        // Nobody else can have claimed the slot while it is odd, so a plain store publishes it.
        s.0[STAMP].store(stamp, Ordering::Release);
        true
    }

    /// The entry at `pos` if it is whole and not overwritten.
    pub fn get(&self, pos: u64) -> Option<Entry> {
        let s = self.slot(pos);
        let stamp = (pos + 1) << 1;
        if s.0[STAMP].load(Ordering::Acquire) != stamp {
            return None;
        }
        let w: [u64; 5] = core::array::from_fn(|i| s.0[i + 1].load(Ordering::Relaxed));
        fence(Ordering::Acquire);
        if s.0[STAMP].load(Ordering::Relaxed) != stamp {
            return None;
        }
        Some(Entry {
            cpu: self.cpu,
            pos,
            time_ns: w[0],
            kind: Kind::from_u8(w[2] as u8)?,
            thread: w[1],
            name_id: (w[2] >> 8) as u32,
            a: w[3],
            b: w[4],
        })
    }

    /// The CPU this ring belongs to.
    pub fn cpu(&self) -> u32 {
        self.cpu
    }

    /// Positions that may still hold entries: `(oldest, next)`.
    pub fn span(&self) -> (u64, u64) {
        let next = self.head.load(Ordering::Acquire);
        (next.saturating_sub(self.capacity()), next)
    }

    /// Visits entries newest first (`newest_first`) or oldest first until `f` says stop. Returns the entries
    /// in the span that could not be read (overwritten or being written while read).
    pub fn scan(&self, newest_first: bool, mut f: impl FnMut(&Entry) -> bool) -> u64 {
        let (old, next) = self.span();
        let mut lost = 0;
        let mut visit = |p: u64| match self.get(p) {
            Some(e) => f(&e),
            None => {
                lost += 1;
                true
            }
        };
        if newest_first {
            for p in (old..next).rev() {
                if !visit(p) {
                    break;
                }
            }
        } else {
            for p in old..next {
                if !visit(p) {
                    break;
                }
            }
        }
        lost
    }

    /// Entries written over (lost to readers) since boot.
    pub fn overwritten(&self) -> u64 {
        self.span().0
    }
}

fn make(cpu: usize) -> Ring {
    Ring::new(cpu as u32, SLOTS.load(Ordering::Acquire))
}

static RINGS: PerCpu<Ring> = PerCpu::new(make);

/// Bytes of trace ring per CPU for a machine with `total_ram` bytes and `cpus` CPUs: RAM / 4096 shared by the
/// CPUs (the Mini-PC: 32 GB, 16 CPUs -> 512 KiB each, ~10 000 entries), at least 64 KiB, at most 4 MiB.
pub fn bytes_for_memory(total_ram: u64, cpus: usize) -> usize {
    (total_ram / 4096 / cpus.max(1) as u64).clamp(64 << 10, 4 << 20) as usize
}

pub const ENTRY_BYTES: usize = core::mem::size_of::<Slot>();

/// CONFIG `trace_buf=` (bytes per CPU) wins over the memory size.
pub fn set_explicit_bytes(bytes: usize) {
    EXPLICIT.store(bytes, Ordering::Relaxed);
}

/// klogd at init (thread context): creates the ring of every possible CPU. Later calls cover CPUs that
/// appeared since (sizes never change after the first call).
pub fn init(total_ram: u64) {
    let cpus = hal::percpu::possible_cpus();
    if SLOTS.load(Ordering::Acquire) == 0 {
        let bytes = match EXPLICIT.load(Ordering::Relaxed) {
            0 => bytes_for_memory(total_ram, cpus),
            b => b,
        };
        SLOTS.store((bytes / ENTRY_BYTES).max(2), Ordering::Release);
        RINGS.cover_possible();
        let cap = RINGS.find_for(0).map_or(0, |r| r.capacity());
        event!(Info, "klog", "trace.ready", cpus = cpus, entries_per_cpu = cap, bytes_per_cpu = cap as usize * ENTRY_BYTES, kinds = kinds());
    } else {
        RINGS.cover_possible();
    }
}

pub fn ring(cpu: usize) -> Option<&'static Ring> {
    if SLOTS.load(Ordering::Acquire) == 0 {
        return None;
    }
    RINGS.find_for(cpu)
}

/// The rings that exist (CPUs online).
pub fn rings() -> impl Iterator<Item = &'static Ring> {
    let ready = SLOTS.load(Ordering::Acquire) != 0;
    RINGS.iter_existing().filter(move |_| ready)
}

/// Entries not recorded: written before `init`, for a CPU without a ring, or into a slot another writer held
/// (a writer stalled while the ring wrapped past it).
pub fn dropped() -> u64 {
    DROPPED.load(Ordering::Relaxed)
}

fn push_on(cpu: usize, kind: Kind, thread: u64, name_id: u32, a: u64, b: u64) {
    match ring(cpu) {
        Some(r) => {
            r.push(crate::now(), kind, thread, name_id, a, b);
        }
        None => {
            DROPPED.fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn push(kind: Kind, name_id: u32, a: u64, b: u64) {
    push_on(hal::percpu::this_cpu(), kind, hal::percpu::current(), name_id, a, b);
}

// ---- hooks (the callers' lanes wire them in; see plan/notes/rb-RB-7a.md) ----

/// sched: a switch from thread `prev` to thread `next` (ids).
#[inline]
pub fn switch(prev: u64, next: u64) {
    if enabled(Kind::Switch) {
        push(Kind::Switch, 0, prev, next);
    }
}

/// syscall: the clock at entry (0 = not traced; pass it to `syscall_done`).
#[inline]
pub fn syscall_start() -> u64 {
    if enabled(Kind::Syscall) { crate::now() } else { 0 }
}

/// syscall: records the call if it ran longer than the limit without blocking (`blocked`: the thread slept in
/// it, e.g. a futex or epoll wait; such calls are slow by design).
#[inline]
pub fn syscall_done(nr: u64, start: u64, ret: i64, blocked: bool) {
    if start == 0 || blocked {
        return;
    }
    let ns = crate::now().saturating_sub(start);
    if ns >= SYSCALL_NS.load(Ordering::Relaxed) {
        push(Kind::Syscall, 0, (nr & 0xFFFF_FFFF) | (ret as u32 as u64) << 32, ns);
    }
}

/// ksync (installed as its slow-wait hook): a lock `name` at `addr` was waited for `ticks` TSC ticks.
pub fn lock_wait(name: &'static str, addr: usize, ticks: u64) {
    if !enabled(Kind::LockWait) {
        return;
    }
    let hz = TSC_HZ.load(Ordering::Relaxed);
    if hz == 0 {
        return;
    }
    let ns = ((ticks as u128 * 1_000_000_000) / hz as u128).min(u64::MAX as u128) as u64;
    if ns >= LOCK_NS.load(Ordering::Relaxed) {
        let (id, _) = crate::keys::lookup("lock", name);
        push(Kind::LockWait, id, addr as u64, ns);
    }
}

/// mm: a user page fault at `addr`. Records one entry per window in which this CPU took more than the limit.
#[inline]
pub fn page_fault(addr: u64) {
    if !enabled(Kind::Fault) {
        return;
    }
    let cpu = hal::percpu::this_cpu();
    let Some(r) = ring(cpu) else { return };
    let now = crate::now();
    if now.saturating_sub(r.fault_start.load(Ordering::Relaxed)) >= FAULT_WINDOW_NS {
        r.fault_start.store(now, Ordering::Relaxed);
        r.faults.store(0, Ordering::Relaxed);
    }
    let n = r.faults.fetch_add(1, Ordering::Relaxed) + 1;
    if n == FAULTS.load(Ordering::Relaxed) {
        let _ = r.push(now, Kind::Fault, hal::percpu::current(), 0, addr, n);
    }
}

/// klogd's IRQ sampler: `cpu` took `per_s` interrupts per second, most on `vector`.
pub fn irq_storm(cpu: usize, vector: u8, per_s: u64) {
    if enabled(Kind::Irq) {
        push_on(cpu, Kind::Irq, 0, 0, vector as u64, per_s);
    }
}

/// Any subsystem: one high-rate event that would flood the log (`a`, `b` are the caller's).
#[inline]
pub fn mark(subsystem: &'static str, name: &'static str, a: u64, b: u64) {
    if enabled(Kind::Mark) {
        let (id, _) = crate::keys::lookup(subsystem, name);
        push(Kind::Mark, id, a, b);
    }
}

// ---- entries as log lines ----

impl Entry {
    pub fn level(&self) -> Level {
        match self.kind {
            Kind::Irq => Level::Warn,
            Kind::Fault => Level::Info,
            Kind::Syscall | Kind::LockWait | Kind::Mark => Level::Debug,
            Kind::Switch => Level::Trace,
        }
    }

    fn names(&self) -> Option<(&'static str, &'static str)> {
        crate::keys::get(self.name_id).and_then(|k| k.names())
    }

    pub fn subsystem(&self) -> &'static str {
        match self.kind {
            Kind::Switch => "sched",
            Kind::Irq => "irq",
            Kind::Syscall => "syscall",
            Kind::LockWait => "lock",
            Kind::Fault => "mm",
            Kind::Mark => self.names().map_or("?", |n| n.0),
        }
    }

    pub fn name(&self) -> &'static str {
        match self.kind {
            Kind::Switch => "trace.switch",
            Kind::Irq => "trace.irq_storm",
            Kind::Syscall => "trace.slow_syscall",
            Kind::LockWait => "trace.lock_wait",
            Kind::Fault => "trace.fault_burst",
            Kind::Mark => self.names().map_or("?", |n| n.1),
        }
    }

    /// `key=value ...` like a klog record's text (numbers and static names only: nothing to escape).
    pub fn text(&self) -> alloc::string::String {
        let mut s = alloc::string::String::new();
        let _ = self.write_text(&mut s);
        s
    }

    /// `text` into any writer (the `events` tool matches field filters on a stack buffer).
    pub fn write_text(&self, s: &mut impl Write) -> core::fmt::Result {
        match self.kind {
            Kind::Switch => write!(s, "prev={} next={}", self.a, self.b),
            Kind::Irq => write!(s, "vector={} per_s={}", self.a, self.b),
            Kind::Syscall => write!(s, "nr={} ret={} us={}", self.a & 0xFFFF_FFFF, (self.a >> 32) as u32 as i32, self.b / 1000),
            Kind::LockWait => write!(s, "lock={} addr={:#x} us={}", self.names().map_or("?", |n| n.1), self.a, self.b / 1000),
            Kind::Fault => write!(s, "addr={:#x} faults={} window_ms={}", self.a, self.b, FAULT_WINDOW_NS / 1_000_000),
            Kind::Mark => write!(s, "a={} b={}", self.a, self.b),
        }?;
        write!(s, " cpu={} thread={}", self.cpu, self.thread)
    }
}
