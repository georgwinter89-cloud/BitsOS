//! PC-3, kernel side: interrupt vectors for a PCI function (`PciFunction::alloc_irqs`, Linux
//! `pci_alloc_irq_vectors` + `request_irq` in one), on CP's vector allocator (`C: IrqVector`) through
//! `hal::msi`.
//!
//! - **Modes**, tried in this order as the request allows: MSI-X (`min..=max` entries, table and PBA mapped
//!   uncached from their BAR), MSI (a power-of-two block; more than one message only as an aligned run of
//!   vectors on one CPU, which the device fills in by the low data bits), polled (no vector: `wait` returns
//!   `Poll` every `poll_ns`). The routed I/O APIC pins carry only the SCI and PS/2 (CP-3), so PCI INTx stays off
//!   in every mode (command bit 10, also set again here); a function never has MSI and MSI-X on together.
//! - **IRQ -> thread wake:** each index has a `Line` (hal) whose hard-IRQ action calls the driver's optional
//!   hook, counts an event and wakes the index's `WaitQueue` (SC-3 allows wakes from IRQ handlers). The
//!   driver's thread waits with `IrqHandle::wait(timeout)` (B4: always with a deadline).
//! - **Affinity:** `Affinity::Spread` puts index i on the i-th reachable CPU from a rotating start, `Cpu(c)` all
//!   on one CPU, `Any` lets the allocator choose. `set_affinity` moves an index (MSI-X) or the whole block (MSI)
//!   with the entry masked while address and data change and while the old vector is freed; a plain MSI
//!   without mask bits is switched off for the change. Every moved index's owner is woken once afterwards: a
//!   message the device posted to the old vector before the change finds no line there (Linux keeps the old
//!   vector until the first delivery on the new one, `irq_complete_move`; here one spurious wake is cheaper).
//! - **Device mask = record:** the soft mask and the storm mark of each line are the record; every mask write
//!   (`mask`, `unmask`, the guard, the end of a move or restore) writes what the record says now and re-checks
//!   it afterwards, so the last write always carries the newest record. While a move or restore rewrites the
//!   message (`moving`), `mask`/`unmask` change only the record; the move waits for writers already inside
//!   and writes the record when it ends.
//! - **Storm guard:** more than `StormLimit::per_window` deliveries in one window (default V1's and CP-3's 100
//!   per ms) mark the line stormed in hard-IRQ context (MSI-X entries are masked right there); the guard thread
//!   `pci.msi.guard` masks plain MSI, logs `msi.storm`, holds it 100 ms (doubling for repeated storms within
//!   30 s, at most 10 s), then unmasks, wakes the owner once and logs `msi.storm_end`. Meanwhile `wait`
//!   returns `Poll` every `poll_ns`, so the driver keeps working by polling.
//! - **Teardown:** dropping the `IrqSet` masks and switches MSI/MSI-X off and frees the vectors.
//!   `disable_device` (Quiesce, remove) switches the enables off itself and releases the vectors here without
//!   heap, klog or unbounded locks (`release_quiet`); the set's methods then report `Released`.
//!   `restore_config` writes the MSI/MSI-X state back after a reset (`restore_locked`).
//! - **R8:** a function left untouched by its key and mass storage not bound to `nvme` are refused before
//!   any access (R8a, R8d); raw MCP writes never reach these capabilities (PC-5's `raw` refusals).
//!
//! Locks: `pci.enable` (sleeping, bounded) around alloc, free, affinity, restore and the guard's device
//! changes; `pci.cfg` per configuration access under it; `pci.irq` (the live sets) and `pci.irq.maps` are taken
//! with no other lock held; under them only `release_quiet` does more: it frees vectors (`hal::msi::free`:
//! atomics and a bounded spin, no lock) and wakes the waiters (ksync's wait-queue lock, inner to every
//! SUBSYSTEM lock). Masking an index takes no sleeping lock.

use crate::device::{self, Locked};
use crate::msi::{self, MsiLayout, MsixLayout, Plan, Table};
use crate::node::{Bar, Problem};
use crate::{Loc, PciFunction, NODES};
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use hal::mmio::IoMem;
use hal::msi::{Line, Message};
use hal::vectors::{Vector, VectorError};
use ksync::{Once, SpinLock, WaitQueue};

klog::counter!(STORMS, "pci.msi.storms");
klog::counter!(SETS_MADE, "pci.msi.sets");

/// How a set delivers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IrqMode {
    MsiX,
    Msi,
    /// No vector: the owner polls (`wait` returns `Poll` every `poll_ns`).
    Polled,
}

impl IrqMode {
    pub fn name(self) -> &'static str {
        match self {
            IrqMode::MsiX => "msix",
            IrqMode::Msi => "msi",
            IrqMode::Polled => "polled",
        }
    }
}

/// Where the vectors go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Affinity {
    /// The allocator's choice (the CPU with the most free vectors).
    Any,
    /// Index i on the i-th CPU from a rotating start (queues per CPU).
    Spread,
    /// Every index on this CPU (falls back to `Any` if it cannot be reached).
    Cpu(u32),
}

/// The storm rule: more than `per_window` deliveries within `window_ns`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StormLimit {
    pub per_window: u32,
    pub window_ns: u64,
}

/// V1's and CP-3's rule: more than 100 interrupts in 1 ms. A driver that legitimately goes faster (NVMe
/// without coalescing) sets its own.
pub const DEFAULT_STORM: StormLimit = StormLimit { per_window: 100, window_ns: 1_000_000 };
/// The first hold after a storm, the most, and how long after a storm's end another one counts as repeated.
pub const STORM_HOLD_NS: u64 = 100_000_000;
pub const STORM_HOLD_MAX_NS: u64 = 10_000_000_000;
pub const STORM_REPEAT_NS: u64 = 30_000_000_000;

/// A driver's hard-IRQ hook: runs with interrupts off before the wake (no heap, no klog, no plain `SpinLock`,
/// and it must never enable interrupts: `hal::msi` relies on MSI handlers not nesting), with the index that
/// fired. Also runs, interrupts off, from thread context when the core wakes the owner
/// once (after an unmask, a storm, a message change).
pub type IrqHook = Arc<dyn Fn(u16) + Send + Sync>;

/// What a driver asks for. `IrqRequest::new(name, min, max)` allows every mode, spreads, guards with
/// `DEFAULT_STORM` and polls every 1 ms when it falls back.
#[derive(Clone)]
pub struct IrqRequest {
    pub name: &'static str,
    pub min: u16,
    pub max: u16,
    pub msix: bool,
    pub msi: bool,
    pub polled: bool,
    pub affinity: Affinity,
    pub storm: Option<StormLimit>,
    pub poll_ns: u64,
    pub hook: Option<IrqHook>,
}

impl IrqRequest {
    pub fn new(name: &'static str, min: u16, max: u16) -> IrqRequest {
        IrqRequest { name, min, max, msix: true, msi: true, polled: true, affinity: Affinity::Spread, storm: Some(DEFAULT_STORM), poll_ns: 1_000_000, hook: None }
    }
    pub fn modes(mut self, msix: bool, msi: bool, polled: bool) -> Self {
        (self.msix, self.msi, self.polled) = (msix, msi, polled);
        self
    }
    pub fn affinity(mut self, a: Affinity) -> Self {
        self.affinity = a;
        self
    }
    pub fn storm(mut self, s: Option<StormLimit>) -> Self {
        self.storm = s;
        self
    }
    pub fn poll_ns(mut self, ns: u64) -> Self {
        self.poll_ns = ns.max(1_000);
        self
    }
    pub fn hook(mut self, h: IrqHook) -> Self {
        self.hook = Some(h);
        self
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IrqError {
    /// R8d: untouched until this key is on.
    Untouched(&'static str),
    /// R8a: mass storage only through the NVMe driver.
    Storage,
    /// `enable_device(true)` first: an interrupt message is a memory write and needs bus master.
    NotEnabled,
    NotScanned,
    /// `pci.enable` stayed busy past its bound, or the function already has a live set.
    Busy,
    /// Neither MSI-X nor MSI could give `min` vectors and polling was not allowed.
    NoVectors,
    NoSuchIndex,
    /// The CPU cannot be named by an MSI (offline, or an APIC ID above 255).
    Unreachable(u32),
    /// `disable_device` released the set (Quiesce, remove); allocate a new one after `enable_device`.
    Released,
    /// The operation needs a vector and the set is polled.
    Polled,
    /// A vector could not be had on the target CPU.
    Vector(VectorError),
}

impl core::fmt::Display for IrqError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            IrqError::Untouched(k) => write!(f, "untouched {} (R8d)", crate::registry::Until(k)),
            IrqError::Storage => write!(f, "mass storage interrupts only for the NVMe driver (R8a)"),
            IrqError::NotEnabled => write!(f, "enable_device(true) first"),
            IrqError::NotScanned => write!(f, "function not in the scanned tree"),
            IrqError::Busy => write!(f, "busy (pci.enable timeout, or a set is already allocated)"),
            IrqError::NoVectors => write!(f, "no MSI-X/MSI vectors and polling not allowed"),
            IrqError::NoSuchIndex => write!(f, "no such index"),
            IrqError::Unreachable(c) => write!(f, "CPU {c} cannot receive MSIs"),
            IrqError::Released => write!(f, "released by disable_device"),
            IrqError::Polled => write!(f, "the set is polled"),
            IrqError::Vector(e) => write!(f, "vector: {e:?}"),
        }
    }
}

/// What a `wait` ended with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wake {
    /// At least one interrupt (or a wake by the core) since the last wait.
    Irq,
    /// Polled set, stormed line or released set: time to look at the device.
    Poll,
    Timeout,
}

/// Per-index numbers (hw_info, tests).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IrqStats {
    pub vector: Option<Vector>,
    pub deliveries: u64,
    pub suppressed: u64,
    pub storms: u64,
    pub masked: bool,
    pub stormed: bool,
}

struct Queue {
    wq: WaitQueue,
    events: AtomicU64,
    seen: AtomicU64,
}

/// Vector packed for lock-free reads: 0 = none, else VALID | cpu << 8 | vector.
const VALID: u64 = 1 << 63;

fn pack(v: Vector) -> u64 {
    VALID | (v.cpu as u64) << 8 | v.vector as u64
}

fn unpack(p: u64) -> Option<Vector> {
    (p & VALID != 0).then_some(Vector { cpu: ((p & !VALID) >> 8) as u32, vector: p as u8 })
}

struct Slot {
    line: Arc<Line>,
    queue: Arc<Queue>,
    vector: AtomicU64,
    /// Guard: when the hold ends (0 = no storm being held).
    storm_until: AtomicU64,
    storm_run: AtomicU32,
    last_storm_end: AtomicU64,
}

impl Slot {
    fn vector(&self) -> Option<Vector> {
        unpack(self.vector.load(Ordering::Acquire))
    }
}

pub(crate) struct SetInner {
    loc: Loc,
    name: &'static str,
    mode: IrqMode,
    msi: Option<MsiLayout>,
    msix: Option<MsixLayout>,
    table: Option<Arc<IoMem>>,
    pba: Option<Arc<IoMem>>,
    block_log2: AtomicU8,
    slots: Vec<Slot>,
    released: AtomicBool,
    poll_ns: u64,
    /// The guard switched a plain MSI (no mask bits) off.
    guard_msi_off: AtomicBool,
    /// A move or restore is rewriting the message: mask writers leave the device alone (the mover writes the
    /// record when it ends).
    moving: AtomicBool,
    /// `mask`/`unmask`/guard writes to the device in progress (a move waits for them before it starts).
    mask_writers: AtomicU32,
}

/// The live sets, one per function.
static SETS: SpinLock<BTreeMap<Loc, Arc<SetInner>>> = SpinLock::named("pci.irq", BTreeMap::new());
/// MSI-X tables and PBAs mapped so far (a mapping is never undone, so it is kept for the next set).
static MAPS: SpinLock<Vec<(u64, usize, Arc<IoMem>)>> = SpinLock::named("pci.irq.maps", Vec::new());
static SPREAD: AtomicU32 = AtomicU32::new(0);
static GUARD: Once<bool> = Once::new();
static GUARD_WQ: WaitQueue = WaitQueue::new();
static GUARD_KICK: AtomicBool = AtomicBool::new(false);

fn guard_kick() {
    GUARD_KICK.store(true, Ordering::Release);
    GUARD_WQ.wake_one();
}

fn fname(at: Loc) -> String {
    format!("{at}")
}

pub(crate) fn map_cached(phys: u64, len: usize) -> Option<Arc<IoMem>> {
    if let Some(m) = MAPS.lock().iter().find(|m| m.0 == phys && m.1 == len) {
        return Some(m.2.clone());
    }
    let io = Arc::new(hal::mmio::map_device(phys, len, hal::paging::CacheType::Uncached).ok()?);
    MAPS.lock().push((phys, len, io.clone()));
    Some(io)
}

/// How long a move waits for `mask`/`unmask` calls already writing the device (thread context, yielding).
const MOVE_WAIT_NS: u64 = 100_000_000;
/// Mask writes per call before giving up on a record that keeps changing (the next writer writes it again).
const MASK_SYNC_TRIES: u32 = 8;

/// TSC ticks for `ns` (the storm window is judged in hard-IRQ context by the TSC). Without a calibrated TSC
/// the ticks are taken as nanoseconds: every x86-64-v3 TSC runs at 1 GHz or more, so the window only gets
/// shorter and the guard more lenient (CP-3's `storm_clock` reasoning).
fn ticks(ns: u64) -> u64 {
    match time::tsc_hz() {
        0 => ns,
        hz => ((ns as u128 * hz as u128) / 1_000_000_000) as u64,
    }
}

/// CPUs an MSI can name now, in number order.
fn reachable_cpus() -> Vec<u32> {
    (0..hal::percpu::cpu_count() as u32).filter(|&c| hal::msi::reachable(c)).collect()
}

/// The table and PBA of `l` mapped from their BARs, if they lie inside memory BARs as the scan checked.
fn map_msix(f: &PciFunction, l: &MsixLayout, problems: &[Problem]) -> Result<(Arc<IoMem>, Arc<IoMem>), &'static str> {
    if problems.iter().any(|p| matches!(p, Problem::MsixOutside { .. })) {
        return Err("table_outside_bar");
    }
    let place = |bir: u8, off: u32, len: usize| -> Result<Arc<IoMem>, &'static str> {
        match f.bar(bir) {
            Some(Bar::Mem { phys, size, .. }) if off as u64 + len as u64 <= size => map_cached(phys + off as u64, len).ok_or("map_failed"),
            _ => Err("table_outside_bar"),
        }
    };
    Ok((place(l.table_bir, l.table_offset, l.table_bytes())?, place(l.pba_bir, l.pba_offset, l.pba_bytes())?))
}

/// Vectors for an MSI block of `n` (a power of two) lines on one CPU: an aligned run (the device writes
/// `data | message number`). Candidates are allocated one by one on the target CPU (the allocator hands out
/// the lowest free vector, so a run turns up quickly), the run's vectors are rebound to their lines, the rest
/// given back. None if the CPU has no such run.
fn alloc_block(lines: &[Arc<Line>], cpu: Option<u32>) -> Option<Vec<Vector>> {
    let n = lines.len() as u16;
    let first = hal::msi::alloc(&lines[0], cpu).ok()?;
    if !hal::msi::reachable(first.cpu) {
        hal::msi::free(first);
        return None;
    }
    if n == 1 {
        return Some(alloc::vec![first]);
    }
    let target = first.cpu;
    let mut got = alloc::vec![first];
    let run = loop {
        let nums: Vec<u8> = got.iter().map(|v| v.vector).collect();
        if let Some(base) = msi::aligned_run(&nums, n) {
            break Some(base);
        }
        match hal::msi::alloc(&lines[0], Some(target)) {
            Ok(v) => got.push(v),
            Err(_) => break None,
        }
    };
    let Some(base) = run else {
        for v in got {
            hal::msi::free(v);
        }
        return None;
    };
    let mut out = Vec::new();
    for k in 0..n {
        let want = Vector { cpu: target, vector: base + k as u8 };
        hal::msi::rebind(want, &lines[k as usize]);
        out.push(want);
    }
    for v in got.into_iter().filter(|v| !out.contains(v)) {
        hal::msi::free(v);
    }
    Some(out)
}

/// The CPU for index `i` of `n` under `a` (`start`: the set's rotating start for `Spread`).
fn pick(a: Affinity, i: usize, cpus: &[u32], start: usize) -> Option<u32> {
    match a {
        Affinity::Any => None,
        Affinity::Cpu(c) => cpus.contains(&c).then_some(c),
        Affinity::Spread if cpus.is_empty() => None,
        Affinity::Spread => Some(cpus[(start + i) % cpus.len()]),
    }
}

/// Where to look for a vector: the wanted CPU, then the allocator's choice, then each reachable CPU.
fn candidates(cpu: Option<u32>, cpus: &[u32]) -> impl Iterator<Item = Option<u32>> + '_ {
    cpu.into_iter().map(Some).chain(core::iter::once(None)).chain(cpus.iter().filter(move |&&c| Some(c) != cpu).map(|&c| Some(c)))
}

/// One vector on a CPU an MSI can name (the allocator's choice may be one that cannot: an APIC ID above 255).
fn alloc_one(line: &Arc<Line>, cpu: Option<u32>, cpus: &[u32]) -> Option<Vector> {
    candidates(cpu, cpus).find_map(|c| {
        let v = hal::msi::alloc(line, c).ok()?;
        if hal::msi::reachable(v.cpu) {
            Some(v)
        } else {
            hal::msi::free(v);
            None
        }
    })
}

fn free_vectors(slots: &[Slot]) {
    for s in slots {
        if let Some(v) = unpack(s.vector.swap(0, Ordering::AcqRel)) {
            hal::msi::free(v);
        }
    }
}

impl SetInner {
    fn msg(&self, i: usize) -> Option<Message> {
        hal::msi::message(self.slots.get(i)?.vector()?).ok()
    }

    fn masked_now(&self, i: usize) -> bool {
        let l = &self.slots[i].line;
        l.is_soft_masked() || l.is_stormed()
    }

    /// The MSI mask bits the record says (soft-masked or stormed lines).
    fn msi_mask_bits(&self) -> u32 {
        (0..self.slots.len()).filter(|&i| self.masked_now(i)).fold(0, |m, i| m | 1 << (i & 31))
    }

    /// Writes the whole record to the device (alloc and restore), as a move: mask writers wait.
    fn program(&self) -> bool {
        self.begin_move();
        let ok = self.program_raw();
        self.end_move();
        ok
    }

    fn program_raw(&self) -> bool {
        let at = self.loc;
        match self.mode {
            IrqMode::MsiX => {
                let (Some(l), Some(t)) = (self.msix, self.table.as_ref()) else { return false };
                if let Some(m) = self.msi {
                    msi::msi_disable(&Locked, at, &m);
                }
                let entries: Vec<(u16, Message, bool)> =
                    (0..self.slots.len()).filter_map(|i| self.msg(i).map(|m| (i as u16, m, self.masked_now(i)))).collect();
                let ok = entries.len() == self.slots.len() && msi::msix_program(&Locked, at, &l, &**t, &entries);
                msi::intx_off(&Locked, at);
                ok
            }
            IrqMode::Msi => {
                let (Some(l), Some(m)) = (self.msi, self.msg(0)) else { return false };
                if let Some(x) = self.msix {
                    msi::msix_control(&Locked, at, &x, false, true);
                }
                let mask = if l.maskbit { self.msi_mask_bits() } else { 0 };
                msi::msi_program(&Locked, at, &l, m, self.block_log2.load(Ordering::Relaxed), mask);
                if self.guard_msi_off.load(Ordering::Acquire) {
                    // A storm hold without mask bits: MSI stays off until the guard ends it.
                    msi::msi_set_enable(&Locked, at, &l, false);
                }
                msi::intx_off(&Locked, at);
                true
            }
            IrqMode::Polled => {
                msi::intx_off(&Locked, at);
                true
            }
        }
    }

    /// Whether the device holds what the record says (restore skips a function that kept its state).
    fn device_matches(&self) -> bool {
        let at = self.loc;
        match self.mode {
            IrqMode::MsiX => {
                let (Some(l), Some(t)) = (self.msix, self.table.as_ref()) else { return true };
                let (on, fmask) = msi::msix_state(&Locked, at, &l);
                on && !fmask
                    && (0..self.slots.len()).all(|i| match (msi::msix_read_entry(&**t, i as u16), self.msg(i)) {
                        (Some((m, masked)), Some(want)) => m == want && masked == self.masked_now(i),
                        _ => false,
                    })
            }
            IrqMode::Msi => {
                let Some(l) = self.msi else { return true };
                let (on, mme) = msi::msi_state(&Locked, at, &l);
                (on || self.guard_msi_off.load(Ordering::Acquire))
                    && mme == self.block_log2.load(Ordering::Relaxed)
                    && Some(msi::msi_read_message(&Locked, at, &l)) == self.msg(0)
            }
            IrqMode::Polled => true,
        }
    }

    /// Writes index `i`'s device mask as the record says (MSI: the whole mask register), then re-checks the
    /// record: if a concurrent `mask`/`unmask` changed it meanwhile, writes again, so the last write to the
    /// device always carries the newest record (a writer that changes the record writes after its change).
    fn sync_mask(&self, i: usize) {
        match self.mode {
            IrqMode::MsiX => {
                let Some(t) = &self.table else { return };
                for _ in 0..MASK_SYNC_TRIES {
                    let want = self.masked_now(i);
                    msi::msix_mask_entry(&**t, i as u16, want);
                    if self.masked_now(i) == want {
                        return;
                    }
                }
            }
            IrqMode::Msi => {
                let Some(l) = self.msi.filter(|l| l.maskbit) else { return };
                for _ in 0..MASK_SYNC_TRIES {
                    let want = self.msi_mask_bits();
                    msi::msi_write_mask(&Locked, self.loc, &l, want);
                    if self.msi_mask_bits() == want {
                        return;
                    }
                }
            }
            IrqMode::Polled => {}
        }
    }

    /// Brings index `i`'s device mask in line with the record (the caller changed the soft mask or the storm
    /// mark first). Leaves the device alone while a move rewrites the message: the move writes the record when
    /// it ends.
    fn hw_mask(&self, i: usize) {
        self.mask_writers.fetch_add(1, Ordering::SeqCst);
        if !self.moving.load(Ordering::SeqCst) {
            self.sync_mask(i);
        }
        self.mask_writers.fetch_sub(1, Ordering::SeqCst);
    }

    /// Starts a message change: from now on mask writers only change the record; waits (bounded, yielding)
    /// for the ones already writing. SeqCst on both sides: a writer either sees `moving` or is counted here.
    /// False if a writer stayed inside past the bound (logged; the move goes on).
    fn begin_move(&self) -> bool {
        self.moving.store(true, Ordering::SeqCst);
        let end = time::now_ns().saturating_add(MOVE_WAIT_NS);
        while self.mask_writers.load(Ordering::SeqCst) != 0 {
            if time::now_ns() >= end {
                let f = fname(self.loc);
                klog::event!(Warn, "pci", "msi.mask_busy", function = f.as_str(), driver = self.name;
                    "a mask/unmask stayed inside its device write: the message changes anyway");
                return false;
            }
            sched::yield_now();
        }
        true
    }

    /// Ends a message change: every index's device mask is written from the record (with the re-check).
    fn end_move(&self) {
        self.moving.store(false, Ordering::SeqCst);
        match self.mode {
            IrqMode::MsiX => (0..self.slots.len()).for_each(|i| self.sync_mask(i)),
            IrqMode::Msi => self.sync_mask(0),
            IrqMode::Polled => {}
        }
    }

    /// The device side of a teardown: everything masked, MSI/MSI-X off.
    fn device_off(&self) {
        let at = self.loc;
        match self.mode {
            IrqMode::MsiX => {
                if let Some(l) = self.msix {
                    let used: Vec<u16> = (0..self.slots.len() as u16).collect();
                    msi::msix_disable(&Locked, at, &l, self.table.as_deref().map(|t| t as &dyn Table), &used);
                }
            }
            IrqMode::Msi => {
                if let Some(l) = self.msi {
                    msi::msi_write_mask(&Locked, at, &l, u32::MAX);
                    msi::msi_disable(&Locked, at, &l);
                }
            }
            IrqMode::Polled => {}
        }
    }

    fn stats(&self, i: usize) -> Option<IrqStats> {
        let s = self.slots.get(i)?;
        Some(IrqStats {
            vector: s.vector(),
            deliveries: s.line.deliveries(),
            suppressed: s.line.suppressed(),
            storms: s.line.storms(),
            masked: s.line.is_soft_masked(),
            stormed: s.line.is_stormed(),
        })
    }

    fn wait(&self, i: usize, timeout_ns: u64) -> Wake {
        let s = &self.slots[i];
        let q = &s.queue;
        let seen = q.seen.load(Ordering::Acquire);
        let polled = self.mode == IrqMode::Polled || self.released.load(Ordering::Acquire) || s.line.is_stormed();
        let t = if polled { timeout_ns.min(self.poll_ns) } else { timeout_ns };
        q.wq.wait_until_timeout(|| q.events.load(Ordering::Acquire) != seen, t);
        let now = q.events.load(Ordering::Acquire);
        if now != seen {
            q.seen.store(now, Ordering::Release);
            Wake::Irq
        } else if polled {
            Wake::Poll
        } else {
            Wake::Timeout
        }
    }

    /// The guard's pass over this set at `now`: hold new storms, end holds that are due.
    fn guard_step(&self, now: u64) {
        if self.released.load(Ordering::Acquire) {
            return;
        }
        let f = fname(self.loc);
        for (i, s) in self.slots.iter().enumerate() {
            let until = s.storm_until.load(Ordering::Acquire);
            if s.line.is_stormed() && until == 0 {
                let run = if now.saturating_sub(s.last_storm_end.load(Ordering::Relaxed)) < STORM_REPEAT_NS && s.last_storm_end.load(Ordering::Relaxed) != 0 {
                    s.storm_run.fetch_add(1, Ordering::Relaxed) + 1
                } else {
                    s.storm_run.store(1, Ordering::Relaxed);
                    1
                };
                let hold = STORM_HOLD_NS.saturating_mul(1 << (run - 1).min(7)).min(STORM_HOLD_MAX_NS);
                let how = match (self.mode, self.msi.is_some_and(|l| l.maskbit)) {
                    (IrqMode::MsiX, _) => {
                        self.hw_mask(i);
                        "entry_masked"
                    }
                    (IrqMode::Msi, true) => {
                        self.hw_mask(i);
                        "mask_bit"
                    }
                    (IrqMode::Msi, false) => {
                        if let Some(l) = self.msi {
                            msi::msi_set_enable(&Locked, self.loc, &l, false);
                        }
                        self.guard_msi_off.store(true, Ordering::Release);
                        "msi_off"
                    }
                    (IrqMode::Polled, _) => "none",
                };
                s.storm_until.store(now.saturating_add(hold).max(1), Ordering::Release);
                STORMS.inc();
                let v = s.vector();
                klog::event!(Warn, "pci", "msi.storm", function = f.as_str(), driver = self.name, index = i, cpu = v.map_or(0, |v| v.cpu),
                    vector = klog::Hex(v.map_or(0, |v| v.vector as u64)), deliveries = s.line.deliveries(), storms = s.line.storms(),
                    hold_ms = hold / 1_000_000, masked = how; "interrupt storm: masked for a while, the driver polls meanwhile");
            } else if until != 0 && now >= until {
                s.storm_until.store(0, Ordering::Release);
                s.last_storm_end.store(now, Ordering::Relaxed);
                s.line.clear_storm();
                if self.mode == IrqMode::Msi && self.guard_msi_off.load(Ordering::Acquire) {
                    if !self.slots.iter().any(|o| o.line.is_stormed())
                        && let Some(l) = self.msi
                    {
                        msi::msi_set_enable(&Locked, self.loc, &l, true);
                        self.guard_msi_off.store(false, Ordering::Release);
                    }
                } else {
                    // The record decides: still masked if the owner masked it meanwhile.
                    self.hw_mask(i);
                }
                s.line.retrigger();
                klog::event!(Info, "pci", "msi.storm_end", function = f.as_str(), driver = self.name, index = i, suppressed = s.line.suppressed(),
                    deliveries = s.line.deliveries(); "storm hold over: unmasked");
            }
        }
    }

    fn next_due(&self) -> Option<u64> {
        self.slots.iter().map(|s| s.storm_until.load(Ordering::Acquire)).filter(|&u| u != 0).min()
    }
}

fn guard_loop() {
    loop {
        let sets: Vec<Arc<SetInner>> = SETS.lock().values().cloned().collect();
        let now = time::now_ns();
        let next = sets.iter().filter_map(|s| s.next_due()).min();
        let timeout = next.map_or(1_000_000_000, |d| d.saturating_sub(now).clamp(1_000_000, 1_000_000_000));
        drop(sets);
        GUARD_WQ.wait_until_timeout(|| GUARD_KICK.load(Ordering::Acquire), timeout);
        GUARD_KICK.store(false, Ordering::Release);
        let sets: Vec<Arc<SetInner>> = SETS.lock().values().cloned().collect();
        let now = time::now_ns();
        for s in sets.iter().filter(|s| s.slots.iter().any(|x| x.line.is_stormed() || x.storm_until.load(Ordering::Acquire) != 0)) {
            match device::lock_enable(&fname(s.loc)) {
                Ok(_g) => s.guard_step(now),
                // Retried on the next pass (at most 1 s later).
                Err(_) => GUARD_KICK.store(true, Ordering::Release),
            }
        }
    }
}

fn ensure_guard() -> bool {
    *GUARD.get_or_init(|| match sched::spawn("pci.msi.guard", sched::Class::KernelRt, guard_loop) {
        Ok(_) => true,
        Err(e) => {
            klog::event!(Warn, "pci", "msi.guard_missing", reason = format!("{e:?}");
                "no storm guard thread: MSI-X storms stay masked, MSI storms only suppressed");
            false
        }
    })
}

/// A function's interrupt vectors. Dropping it masks and switches them off and frees the vectors.
pub struct IrqSet {
    inner: Arc<SetInner>,
}

/// One index of a set, for the thread that waits on it (clone it into the thread).
#[derive(Clone)]
pub struct IrqHandle {
    inner: Arc<SetInner>,
    index: usize,
}

impl IrqHandle {
    /// Waits (B4: always with a deadline) until an interrupt on this index or `timeout_ns`. One waiting thread
    /// per index. `Poll` comes every `poll_ns` while the set is polled, the line stormed or the set released.
    pub fn wait(&self, timeout_ns: u64) -> Wake {
        self.inner.wait(self.index, timeout_ns)
    }
    /// Interrupts (and core wakes) seen on this index so far.
    pub fn events(&self) -> u64 {
        self.inner.slots[self.index].queue.events.load(Ordering::Acquire)
    }
    pub fn index(&self) -> u16 {
        self.index as u16
    }
}

impl IrqSet {
    pub fn mode(&self) -> IrqMode {
        self.inner.mode
    }
    pub fn len(&self) -> usize {
        self.inner.slots.len()
    }
    pub fn is_empty(&self) -> bool {
        self.inner.slots.is_empty()
    }
    pub fn function(&self) -> Loc {
        self.inner.loc
    }
    /// The vector of index `i` now (None: polled or released).
    pub fn vector(&self, i: usize) -> Option<Vector> {
        self.inner.slots.get(i)?.vector()
    }
    pub fn handle(&self, i: usize) -> Option<IrqHandle> {
        (i < self.inner.slots.len()).then(|| IrqHandle { inner: self.inner.clone(), index: i })
    }
    pub fn wait(&self, i: usize, timeout_ns: u64) -> Result<Wake, IrqError> {
        if i >= self.inner.slots.len() {
            return Err(IrqError::NoSuchIndex);
        }
        Ok(self.inner.wait(i, timeout_ns))
    }
    pub fn stats(&self, i: usize) -> Option<IrqStats> {
        self.inner.stats(i)
    }
    /// Deliveries on index `i` (every vector it had).
    pub fn count(&self, i: usize) -> u64 {
        self.inner.slots.get(i).map_or(0, |s| s.line.deliveries())
    }
    pub fn is_released(&self) -> bool {
        self.inner.released.load(Ordering::Acquire)
    }
    /// The MSI-X pending bit of index `i` (None: not MSI-X).
    pub fn pending(&self, i: usize) -> Option<bool> {
        msi::msix_pending(&**self.inner.pba.as_ref()?, i as u16)
    }

    fn check(&self, i: usize) -> Result<&Slot, IrqError> {
        if self.inner.released.load(Ordering::Acquire) {
            return Err(IrqError::Released);
        }
        self.inner.slots.get(i).ok_or(IrqError::NoSuchIndex)
    }

    /// Masks index `i`: its deliveries are counted, not passed on (MSI-X entry and MSI mask bit at the device
    /// too; a plain MSI without mask bits is masked in software only). Thread context only (an MSI mask bit
    /// goes through `pci.cfg`); in the hard-IRQ hook a driver masks at its own device. No sleeping lock.
    pub fn mask(&self, i: usize) -> Result<(), IrqError> {
        let s = self.check(i)?;
        if !s.line.set_soft_mask(true) {
            self.inner.hw_mask(i);
        }
        Ok(())
    }

    /// Unmasks index `i` (not while the guard holds a storm) and wakes the owner once, so an event that came
    /// while masked is not lost.
    pub fn unmask(&self, i: usize) -> Result<(), IrqError> {
        let s = self.check(i)?;
        if s.line.set_soft_mask(false) {
            // A stormed line stays masked: the record says so.
            self.inner.hw_mask(i);
            s.line.retrigger();
        }
        Ok(())
    }

    /// Moves index `i` to `cpu` (MSI-X), or the whole block (MSI: one address for all its messages). The entry
    /// stays masked while address and data change and the old vector is freed (a plain MSI without mask bits
    /// is off instead); then the record's mask is written back and every moved index woken once, since a
    /// message posted to the old vector just before the change found no line. Returns the new vector of `i`.
    pub fn set_affinity(&self, i: usize, cpu: u32) -> Result<Vector, IrqError> {
        self.check(i)?;
        let inner = &*self.inner;
        let f = fname(inner.loc);
        let _g = device::lock_enable(&f).map_err(|_| IrqError::Busy)?;
        if inner.released.load(Ordering::Acquire) {
            return Err(IrqError::Released);
        }
        if !hal::msi::reachable(cpu) {
            return Err(IrqError::Unreachable(cpu));
        }
        let at = inner.loc;
        let moved: Vec<(usize, Option<Vector>, Vector)> = match inner.mode {
            IrqMode::Polled => return Err(IrqError::Polled),
            IrqMode::MsiX => {
                let (Some(t), s) = (inner.table.as_ref(), &inner.slots[i]) else { return Err(IrqError::Polled) };
                let new = hal::msi::alloc(&s.line, Some(cpu)).map_err(IrqError::Vector)?;
                let Ok(m) = hal::msi::message(new) else {
                    hal::msi::free(new);
                    return Err(IrqError::Unreachable(cpu));
                };
                inner.begin_move();
                // Written masked and left masked until the old vector is gone.
                if !msi::msix_write_entry(&**t, i as u16, m, true) {
                    klog::event!(Error, "pci", "msi.entry_write_failed", function = f.as_str(), driver = inner.name, index = i);
                }
                let old = unpack(s.vector.swap(pack(new), Ordering::AcqRel));
                if let Some(o) = old {
                    hal::msi::free(o);
                }
                inner.end_move();
                alloc::vec![(i, old, new)]
            }
            IrqMode::Msi => {
                let Some(l) = inner.msi else { return Err(IrqError::Polled) };
                let lines: Vec<Arc<Line>> = inner.slots.iter().map(|s| s.line.clone()).collect();
                let new = alloc_block(&lines, Some(cpu)).ok_or(IrqError::Vector(VectorError::Exhausted))?;
                let Ok(m) = hal::msi::message(new[0]) else {
                    for v in new {
                        hal::msi::free(v);
                    }
                    return Err(IrqError::Unreachable(cpu));
                };
                inner.begin_move();
                let was_on = if l.maskbit {
                    msi::msi_write_mask(&Locked, at, &l, u32::MAX);
                    None
                } else {
                    Some(msi::msi_set_enable(&Locked, at, &l, false))
                };
                msi::msi_write_message(&Locked, at, &l, m);
                let mut out = Vec::new();
                for (k, s) in inner.slots.iter().enumerate() {
                    let old = unpack(s.vector.swap(pack(new[k]), Ordering::AcqRel));
                    if let Some(o) = old {
                        hal::msi::free(o);
                    }
                    out.push((k, old, new[k]));
                }
                if was_on == Some(true) {
                    msi::msi_set_enable(&Locked, at, &l, true);
                }
                // Mask bits: written from the record, not from a copy read before the change (a concurrent
                // unmask would be lost).
                inner.end_move();
                out
            }
        };
        if inner.released.load(Ordering::Acquire) {
            // `disable_device` released the set while the entry moved: the new vectors go back too.
            free_vectors(&inner.slots);
            return Err(IrqError::Released);
        }
        // A masked or stormed index hears of it at its unmask or the storm's end.
        for (k, _, _) in moved.iter().filter(|m| !inner.masked_now(m.0)) {
            inner.slots[*k].line.retrigger();
        }
        for (k, old, new) in &moved {
            klog::event!(Info, "pci", "msi.affinity", function = f.as_str(), driver = inner.name, index = *k, old_cpu = old.map_or(0, |o| o.cpu),
                old_vector = klog::Hex(old.map_or(0, |o| o.vector as u64)), cpu = new.cpu, vector = klog::Hex(new.vector as u64));
        }
        Ok(moved.iter().find(|m| m.0 == i).map(|m| m.2).unwrap_or(moved[0].2))
    }

    /// Masks, switches off and frees everything (the same as dropping the set).
    pub fn free(self) {}
}

/// Dropping takes `pci.enable` (bounded, may sleep): never drop a set with a spin lock held.
impl Drop for IrqSet {
    fn drop(&mut self) {
        let inner = &*self.inner;
        let f = fname(inner.loc);
        let guard = device::lock_enable(&f);
        let was_released = inner.released.swap(true, Ordering::AcqRel);
        if !was_released {
            inner.device_off();
            free_vectors(&inner.slots);
        }
        drop(guard);
        {
            let mut sets = SETS.lock();
            if sets.get(&inner.loc).is_some_and(|s| Arc::ptr_eq(s, &self.inner)) {
                sets.remove(&inner.loc);
            }
        }
        // Threads waiting on a handle look at the device once more and then see `Poll`.
        for s in &inner.slots {
            s.queue.wq.wake_all();
        }
        klog::event!(Info, "pci", "msi.free", function = f.as_str(), driver = inner.name, mode = inner.mode.name(), vectors = inner.slots.len(),
            released_before = was_released);
    }
}

impl PciFunction {
    /// Interrupt vectors for this function (see the module text). Call `enable_device(true)` first. Tries
    /// MSI-X, then MSI, then polling, as `req` allows; INTx stays off. Refused without any access for an
    /// untouched function (R8d) and for mass storage unless the NVMe driver is bound or probing (R8a). One set
    /// per function at a time. May sleep (`pci.enable`, at most 2 s).
    pub fn alloc_irqs(&self, req: &IrqRequest) -> Result<IrqSet, IrqError> {
        let at = self.loc();
        let f = fname(at);
        let refuse = |e: IrqError| {
            klog::event!(Warn, "pci", "msi.refused", function = f.as_str(), driver = req.name, reason = format!("{e}"));
            Err(e)
        };
        if let Some(k) = self.untouched {
            return refuse(IrqError::Untouched(k));
        }
        if self.class >> 16 == 0x01 && device::driver_of_loc(at) != Some(crate::optin::STORAGE_DRIVER) {
            return refuse(IrqError::Storage);
        }
        let Some(node) = NODES.lock().iter().find(|n| n.loc == at).cloned() else { return refuse(IrqError::NotScanned) };
        if !device::dma_on(at) {
            return refuse(IrqError::NotEnabled);
        }
        let _g = device::lock_enable(&f).map_err(|_| IrqError::Busy)?;
        {
            let mut sets = SETS.lock();
            match sets.get(&at) {
                Some(s) if !s.released.load(Ordering::Acquire) => {
                    drop(sets);
                    return refuse(IrqError::Busy);
                }
                Some(_) => {
                    sets.remove(&at);
                }
                None => {}
            }
        }
        let msi_l = (self.msi_cap != 0).then(|| MsiLayout::read(&Locked, at, self.msi_cap)).flatten();
        let msix_l = (self.msix_cap != 0).then(|| MsixLayout::read(&Locked, at, self.msix_cap)).flatten();
        let mut maps = None;
        if let (true, Some(l)) = (req.msix, msix_l) {
            match map_msix(self, &l, &node.problems) {
                Ok(m) => maps = Some(m),
                Err(why) => klog::event!(Warn, "pci", "msi.fallback", function = f.as_str(), driver = req.name, from = "msix", reason = why),
            }
        }
        let cpus = reachable_cpus();
        let storm = req.storm.map(|s| (s.per_window, ticks(s.window_ns)));
        let make_slots = |n: u16, table: Option<&Arc<IoMem>>| -> Vec<Slot> {
            (0..n)
                .map(|i| {
                    let queue = Arc::new(Queue { wq: WaitQueue::new(), events: AtomicU64::new(0), seen: AtomicU64::new(0) });
                    let q = queue.clone();
                    let hook = req.hook.clone();
                    let action: Box<dyn Fn() + Send + Sync> = Box::new(move || {
                        if let Some(h) = &hook {
                            h(i);
                        }
                        q.events.fetch_add(1, Ordering::AcqRel);
                        q.wq.wake_all();
                    });
                    let line = Line::new(action, Box::new(guard_kick), table.map(|t| (t.clone(), msi::ctrl_off(i))));
                    if let Some((limit, window)) = storm {
                        line.set_storm_limit(limit, window);
                    }
                    Slot {
                        line: Arc::new(line),
                        queue,
                        vector: AtomicU64::new(0),
                        storm_until: AtomicU64::new(0),
                        storm_run: AtomicU32::new(0),
                        last_storm_end: AtomicU64::new(0),
                    }
                })
                .collect()
        };
        let start = SPREAD.fetch_add(req.max.max(1) as u32, Ordering::Relaxed) as usize;
        let mut msix_ok = maps.is_some();
        let mut msi_ok = req.msi && msi_l.is_some();
        let inner = loop {
            let p = msi::plan(
                req.min,
                req.max,
                msix_l.map(|l| (l.size, msix_ok)),
                if msi_ok { msi_l.map(|l| l.capable_log2) } else { None },
                (req.msix && msix_ok, req.msi && msi_ok, req.polled),
            );
            let Some(p) = p else {
                return refuse(IrqError::NoVectors);
            };
            let base = |mode, slots, table: Option<Arc<IoMem>>, pba: Option<Arc<IoMem>>, b: u8| SetInner {
                loc: at,
                name: req.name,
                mode,
                msi: msi_l,
                msix: msix_l,
                table,
                pba,
                block_log2: AtomicU8::new(b),
                slots,
                released: AtomicBool::new(false),
                poll_ns: req.poll_ns.max(1_000),
                guard_msi_off: AtomicBool::new(false),
                moving: AtomicBool::new(false),
                mask_writers: AtomicU32::new(0),
            };
            let set = match p {
                Plan::MsiX { n } => {
                    let Some((t, pba)) = maps.clone() else {
                        msix_ok = false;
                        continue;
                    };
                    let slots = make_slots(n, Some(&t));
                    let mut got = 0u16;
                    for (i, s) in slots.iter().enumerate() {
                        match alloc_one(&s.line, pick(req.affinity, i, &cpus, start), &cpus) {
                            Some(v) => {
                                s.vector.store(pack(v), Ordering::Release);
                                got += 1;
                            }
                            None => break,
                        }
                    }
                    if got >= req.min.max(1) {
                        let mut slots = slots;
                        free_vectors(&slots[got as usize..]);
                        slots.truncate(got as usize);
                        base(IrqMode::MsiX, slots, Some(t), Some(pba), 0)
                    } else {
                        free_vectors(&slots);
                        klog::event!(Warn, "pci", "msi.fallback", function = f.as_str(), driver = req.name, from = "msix", reason = "vectors", got = got);
                        msix_ok = false;
                        continue;
                    }
                }
                Plan::Msi { block_log2 } => {
                    let mut b = block_log2;
                    let made = loop {
                        let slots = make_slots(1 << b, None);
                        let lines: Vec<Arc<Line>> = slots.iter().map(|s| s.line.clone()).collect();
                        let cpu = pick(req.affinity, 0, &cpus, start);
                        let vs = candidates(cpu, &cpus).find_map(|c| alloc_block(&lines, c));
                        if let Some(vs) = vs {
                            for (s, v) in slots.iter().zip(vs) {
                                s.vector.store(pack(v), Ordering::Release);
                            }
                            break Some(base(IrqMode::Msi, slots, None, None, b));
                        }
                        if b == 0 || (1u16 << (b - 1)) < req.min.max(1) {
                            klog::event!(Warn, "pci", "msi.fallback", function = f.as_str(), driver = req.name, from = "msi", reason = "vectors");
                            break None;
                        }
                        b -= 1;
                    };
                    match made {
                        Some(s) => s,
                        None => {
                            msi_ok = false;
                            continue;
                        }
                    }
                }
                Plan::Polled => base(IrqMode::Polled, make_slots(req.min.max(1), None), None, None, 0),
            };
            let inner = Arc::new(set);
            if inner.program() {
                break inner;
            }
            // The device did not take the record (an MSI-X table write that did not read back): the next mode.
            klog::event!(Error, "pci", "msi.program_failed", function = f.as_str(), driver = req.name, mode = inner.mode.name());
            inner.device_off();
            free_vectors(&inner.slots);
            match inner.mode {
                IrqMode::MsiX => msix_ok = false,
                IrqMode::Msi => msi_ok = false,
                IrqMode::Polled => return refuse(IrqError::NoVectors),
            }
        };
        if req.storm.is_some() && inner.mode != IrqMode::Polled {
            ensure_guard();
        }
        SETS.lock().insert(at, inner.clone());
        if !device::dma_on(at) {
            // `disable_device` ran while the vectors were set up (it found no set to release yet).
            SETS.lock().remove(&at);
            inner.released.store(true, Ordering::Release);
            inner.device_off();
            free_vectors(&inner.slots);
            return refuse(IrqError::NotEnabled);
        }
        SETS_MADE.inc();
        let vs: Vec<String> = inner.slots.iter().filter_map(|s| s.vector()).map(|v| format!("{}:{:#x}", v.cpu, v.vector)).collect();
        klog::event!(Info, "pci", "msi.alloc", function = f.as_str(), driver = req.name, mode = inner.mode.name(), vectors = inner.slots.len(),
            asked_min = req.min, asked_max = req.max, at = vs.join(",").as_str(), intx = "off");
        Ok(IrqSet { inner })
    }
}

/// For `disable_device` (Quiesce, remove; MSI/MSI-X enables are already off): the function's live set is
/// marked released and its vectors freed. No heap, no klog; `pci.irq` is tried for 1 ms. False if the set
/// could not be reached (the lock stayed busy) or a vector's CPU stayed in its handler.
pub(crate) fn release_quiet(at: Loc) -> bool {
    let Some(sets) = device::bounded(|| SETS.try_lock()) else { return false };
    // Worked on under the guard, not through a clone: a clone could become the last reference if the owner
    // dropped its `IrqSet` meanwhile, and the set would be freed here (heap on the quiesce path). Under
    // `pci.irq` only atomics and `hal::msi::free` (no lock, bounded spin, never the line's last reference:
    // the set in the map holds every line) run.
    let Some(s) = sets.get(&at) else { return true };
    if s.released.swap(true, Ordering::AcqRel) {
        return true;
    }
    let mut ok = true;
    for slot in &s.slots {
        if let Some(v) = unpack(slot.vector.swap(0, Ordering::AcqRel)) {
            ok &= hal::msi::free(v);
        }
    }
    for slot in &s.slots {
        slot.queue.wq.wake_all();
    }
    ok
}

/// For `restore_config` (`pci.enable` held): writes the live set's MSI/MSI-X state back when the device lost
/// it (a reset, a retrain). Returns whether something was written.
pub(crate) fn restore_locked(at: Loc) -> bool {
    let Some(s) = SETS.lock().get(&at).cloned() else { return false };
    if s.released.load(Ordering::Acquire) || s.device_matches() {
        return false;
    }
    let ok = s.program();
    let f = fname(at);
    klog::event!(Warn, "pci", "msi.restored", function = f.as_str(), driver = s.name, mode = s.mode.name(), ok = ok;
        "MSI/MSI-X state was lost (reset or retrain): written back");
    ok
}

/// The first vector of the function's live set (sysfs `irq`).
pub fn first_vector(at: Loc) -> Option<Vector> {
    SETS.lock().get(&at).and_then(|s| s.slots.first()).and_then(Slot::vector)
}

/// The live sets as (function, driver, mode, per-index stats) for hw_info and tests; sets `disable_device`
/// released are left out (`released_count`).
pub fn sets() -> Vec<(Loc, &'static str, IrqMode, Vec<IrqStats>)> {
    let sets: Vec<Arc<SetInner>> = SETS.lock().values().filter(|s| !s.released.load(Ordering::Acquire)).cloned().collect();
    sets.iter().map(|s| (s.loc, s.name, s.mode, (0..s.slots.len()).filter_map(|i| s.stats(i)).collect())).collect()
}

/// Sets released by `disable_device` whose owner has not dropped them yet.
pub fn released_count() -> usize {
    SETS.lock().values().filter(|s| s.released.load(Ordering::Acquire)).count()
}

/// hw_info's `msi` section: one line per index with its CPU, vector and delivery count.
pub fn summary() -> Vec<String> {
    let all = sets();
    let mut out = alloc::vec![format!(
        "msi: {} sets ({} released), guard {}, storms {}, unbound deliveries {}, leaked lines {}, nested deliveries {}",
        all.len(),
        released_count(),
        match GUARD.get() {
            Some(true) => "running",
            Some(false) => "missing",
            None => "not started",
        },
        STORMS.get(),
        hal::msi::unbound_count(),
        hal::msi::leaked_count(),
        hal::msi::nested_count()
    )];
    for (at, name, mode, stats) in all {
        for (i, s) in stats.iter().enumerate() {
            let place = s.vector.map_or(String::from("-"), |v| format!("cpu {} vector {:#x}", v.cpu, v.vector));
            let flags = match (s.masked, s.stormed) {
                (true, true) => " masked stormed",
                (true, false) => " masked",
                (false, true) => " stormed",
                (false, false) => "",
            };
            out.push(format!(
                "{at} {name} {} #{i}: {place} count {} suppressed {} storms {}{flags}",
                mode.name(),
                s.deliveries,
                s.suppressed,
                s.storms
            ));
        }
    }
    out
}
