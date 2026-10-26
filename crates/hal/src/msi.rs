//! MSI/MSI-X, the hal part (owner: PC, item PC-3): the message a device writes to reach one CPU's LAPIC, and
//! the per-vector dispatch from `hal::vectors` to a `Line` the PCI core owns.
//!
//! **Message** (Intel SDM Vol. 3A 11.11.1, AMD APM Vol. 2 16.7, Linux `__irq_msi_compose_msg`): address
//! `0xFEE0_0000 | dest << 12` (physical destination mode, no redirection hint), data = the vector (fixed
//! delivery, edge). The address carries 8 destination bits: an APIC ID above 255 needs interrupt remapping or
//! the extended destination ID, neither of which BitsOS uses (both machines have IDs below 16), so such a CPU is
//! refused (`DestinationTooWide`) and the PCI core picks another one.
//!
//! **Dispatch:** every MSI vector is allocated from `hal::vectors` with one shared handler (`trampoline`). The
//! handler finds the vector's `Line` in this CPU's slot table (lock-free, no heap) and calls `Line::fire`:
//! count, storm guard, then the owner's action (in practice: wake a thread). A `Line` is reference counted;
//! the slot holds one reference (`Arc::into_raw`). `free` takes the slot's reference back only after the CPU
//! that owns the vector is no longer inside that vector's handler (`in_flight`), like Linux's
//! `synchronize_irq`; if that CPU never leaves (a stopped CPU) the reference is leaked rather than freed under
//! it. The PCI core keeps its own reference for as long as the set lives, so dropping the slot's reference on
//! the quiesce path never frees memory there.
//!
//! **Storm guard in hard-IRQ context:** a line counts its deliveries per window of TSC ticks; when one window
//! holds more than the line's limit it is marked stormed, an MSI-X entry is masked at once through its vector
//! control dword (an MMIO write, no lock), `on_storm` wakes the PCI core's guard thread (which masks plain MSI
//! in configuration space, logs, and unmasks later), and further deliveries are only counted until the guard
//! clears the mark. V1 `apic/msiirq.asm` judged storms on the tick; here the CPU that suffers the storm judges
//! it, so a storm aimed at the only CPU that could run the guard still stops at the device (MSI-X).
//!
//! Handlers run with interrupts off: nothing here allocates, frees, logs or takes a lock.
//!
//! **No nesting:** the `in_flight` protocol keeps one marker per CPU, so it relies on hard-IRQ handlers never
//! nesting on a CPU (`vectors::dispatch` runs with IF=0, the interrupt rule in `switch.rs`) and on a line's
//! action never turning interrupts back on. The trampoline also refuses a nested delivery outright (counted in
//! `nested_count`) instead of overwriting the outer marker.

use crate::mmio::IoMem;
use crate::percpu::PerCpu;
use crate::vectors::{self, Vector, VectorError};
use alloc::boxed::Box;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, Ordering};

/// The LAPIC's MSI address window (xAPIC and x2APIC alike for MSI).
pub const ADDRESS_BASE: u32 = 0xFEE0_0000;
/// MSI-X vector control bit 0: the entry is masked.
pub const MSIX_CTRL_MASKED: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsiError {
    /// The CPU's APIC ID does not fit the 8 destination bits of the message address.
    DestinationTooWide(u32),
    /// The CPU is not online (no APIC ID known).
    NoSuchCpu(u32),
}

impl core::fmt::Display for MsiError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            MsiError::DestinationTooWide(id) => write!(f, "APIC ID {id} does not fit an MSI address (no interrupt remapping)"),
            MsiError::NoSuchCpu(c) => write!(f, "CPU {c} is not online"),
        }
    }
}

/// What a device writes: `address` (64 bits; the upper half is 0 for the LAPIC window) and `data`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Message {
    pub address: u64,
    pub data: u32,
}

impl Message {
    pub fn address_lo(&self) -> u32 {
        self.address as u32
    }
    pub fn address_hi(&self) -> u32 {
        (self.address >> 32) as u32
    }
    /// The APIC ID and vector this message reaches, if it is a LAPIC message in the form `compose` writes.
    pub fn decode(&self) -> Option<(u32, u8)> {
        (self.address >> 20 == (ADDRESS_BASE >> 20) as u64 && self.address & 0xFFC == 0 && self.data & !0xFF == 0)
            .then_some((((self.address >> 12) & 0xFF) as u32, self.data as u8))
    }
}

/// The message for `vector` on the CPU with APIC ID `dest`: physical destination, fixed delivery, edge.
pub fn compose(dest: u32, vector: u8) -> Result<Message, MsiError> {
    if dest > 0xFF {
        return Err(MsiError::DestinationTooWide(dest));
    }
    Ok(Message { address: (ADDRESS_BASE | dest << 12) as u64, data: vector as u32 })
}

/// The APIC ID an MSI for `cpu` must name: `smp::apic_id` for an online CPU; for CPU 0 before CP-4 recorded the
/// BSP, the LAPIC's own ID.
pub fn destination(cpu: u32) -> Option<u32> {
    crate::smp::apic_id(cpu as usize).or_else(|| if cpu == 0 { crate::apic::id() } else { None })
}

/// The message reaching `v`.
pub fn message(v: Vector) -> Result<Message, MsiError> {
    compose(destination(v.cpu).ok_or(MsiError::NoSuchCpu(v.cpu))?, v.vector)
}

/// Whether `cpu` can be named by an MSI (online, APIC ID below 256).
pub fn reachable(cpu: u32) -> bool {
    destination(cpu).is_some_and(|d| d <= 0xFF)
}

// ---------------------------------------------------------------------------------------------------------
// Line

/// One interrupt source (an MSI-X entry or one message of an MSI block) as hard-IRQ code sees it. The PCI core
/// builds it, keeps a reference, and binds it to its current vector with `alloc`; the line and its counts stay
/// the same when the vector moves to another CPU (affinity).
pub struct Line {
    action: Box<dyn Fn() + Send + Sync>,
    on_storm: Box<dyn Fn() + Send + Sync>,
    /// MSI-X: the mapped table and the byte offset of this entry's vector control dword.
    mask_entry: Option<(Arc<IoMem>, usize)>,
    deliveries: AtomicU64,
    /// Deliveries that arrived while the line was masked by its owner or stormed (action not called).
    suppressed: AtomicU64,
    soft_masked: AtomicBool,
    stormed: AtomicBool,
    storms: AtomicU64,
    /// Storm guard: more than `limit` deliveries within `window` TSC ticks (limit 0 = guard off).
    limit: AtomicU32,
    window: AtomicU64,
    win_start: AtomicU64,
    win_count: AtomicU32,
}

impl Line {
    /// `action` runs on every delivery that is not suppressed (hard-IRQ context: no heap, no klog, no plain
    /// `SpinLock`, never enable interrupts; waking a `WaitQueue` is fine). `on_storm` runs once when the guard
    /// marks a storm (the same rules).
    /// `mask_entry`: an MSI-X entry's vector control dword, masked in IRQ context on a storm.
    pub fn new(action: Box<dyn Fn() + Send + Sync>, on_storm: Box<dyn Fn() + Send + Sync>, mask_entry: Option<(Arc<IoMem>, usize)>) -> Line {
        Line {
            action,
            on_storm,
            mask_entry,
            deliveries: AtomicU64::new(0),
            suppressed: AtomicU64::new(0),
            soft_masked: AtomicBool::new(false),
            stormed: AtomicBool::new(false),
            storms: AtomicU64::new(0),
            limit: AtomicU32::new(0),
            window: AtomicU64::new(1),
            win_start: AtomicU64::new(0),
            win_count: AtomicU32::new(0),
        }
    }

    /// Storm guard: more than `limit` deliveries within `window_ticks` TSC ticks is a storm (0 = off).
    pub fn set_storm_limit(&self, limit: u32, window_ticks: u64) {
        self.window.store(window_ticks.max(1), Ordering::Relaxed);
        self.limit.store(limit, Ordering::Relaxed);
        self.reset_window();
    }

    fn reset_window(&self) {
        self.win_start.store(0, Ordering::Relaxed);
        self.win_count.store(0, Ordering::Relaxed);
    }

    /// One delivery at TSC `now`: true when it makes the window pass the limit. A line fires on one CPU at a
    /// time (two only for the moment an affinity change overlaps), so relaxed counting is enough.
    fn storm_hit(&self, now: u64) -> bool {
        let limit = self.limit.load(Ordering::Relaxed);
        if limit == 0 {
            return false;
        }
        let start = self.win_start.load(Ordering::Relaxed);
        if now < start || now - start >= self.window.load(Ordering::Relaxed) {
            self.win_start.store(now, Ordering::Relaxed);
            self.win_count.store(1, Ordering::Relaxed);
            return 1 > limit;
        }
        self.win_count.fetch_add(1, Ordering::Relaxed) + 1 > limit
    }

    fn hw_mask(&self) {
        if let Some((io, off)) = &self.mask_entry {
            // Vector control: bit 0 masks, the other bits are reserved and read 0 (PCIe 6.0 §7.7.2.5); the read
            // flushes the posted write.
            let _ = io.write32(*off, MSIX_CTRL_MASKED);
            let _ = io.read32(*off);
        }
    }

    /// A delivery (hard-IRQ context, interrupts off). What `vectors` calls through the trampoline.
    pub fn fire(&self) {
        self.fire_at(crate::cpu::rdtsc());
    }

    fn fire_at(&self, now: u64) {
        self.deliveries.fetch_add(1, Ordering::Relaxed);
        if self.stormed.load(Ordering::Acquire) {
            // An owner's unmask may have raced with the mask: mask again until the guard ends the storm.
            self.hw_mask();
            self.suppressed.fetch_add(1, Ordering::Relaxed);
            return;
        }
        if self.soft_masked.load(Ordering::Acquire) {
            self.suppressed.fetch_add(1, Ordering::Relaxed);
            return;
        }
        if self.storm_hit(now) {
            self.stormed.store(true, Ordering::Release);
            self.storms.fetch_add(1, Ordering::Relaxed);
            self.hw_mask();
            (self.on_storm)();
        }
        // The delivery that tipped the window is a real event: the owner still hears of it.
        (self.action)();
    }

    /// Runs the action once from thread context with interrupts off, as an interrupt would (after an unmask,
    /// the end of a storm, or a message change that may have dropped one).
    pub fn retrigger(&self) {
        let _irq = crate::cpu::IrqGuard::new();
        (self.action)();
    }

    pub fn deliveries(&self) -> u64 {
        self.deliveries.load(Ordering::Relaxed)
    }
    pub fn suppressed(&self) -> u64 {
        self.suppressed.load(Ordering::Relaxed)
    }
    pub fn storms(&self) -> u64 {
        self.storms.load(Ordering::Relaxed)
    }
    pub fn is_stormed(&self) -> bool {
        self.stormed.load(Ordering::Acquire)
    }
    /// The guard's end of a storm: deliveries reach the action again, the window starts afresh.
    pub fn clear_storm(&self) {
        self.reset_window();
        self.stormed.store(false, Ordering::Release);
    }
    /// The owner's mask: deliveries are counted as suppressed and the action is not called. Returns the
    /// previous state.
    pub fn set_soft_mask(&self, masked: bool) -> bool {
        self.soft_masked.swap(masked, Ordering::AcqRel)
    }
    pub fn is_soft_masked(&self) -> bool {
        self.soft_masked.load(Ordering::Acquire)
    }
}

// ---------------------------------------------------------------------------------------------------------
// Per-CPU slots

struct CpuSlots {
    slots: [AtomicPtr<Line>; 256],
    /// vector + 1 while this CPU runs that vector's line; 0 otherwise.
    in_flight: AtomicU32,
}

impl CpuSlots {
    fn new(_cpu: usize) -> CpuSlots {
        CpuSlots { slots: [const { AtomicPtr::new(core::ptr::null_mut()) }; 256], in_flight: AtomicU32::new(0) }
    }
}

static SLOTS: PerCpu<CpuSlots> = PerCpu::new(CpuSlots::new);
/// Deliveries through the trampoline that found no line (the moment between `vectors::alloc` and the bind,
/// or after `free` began).
static UNBOUND: AtomicU64 = AtomicU64::new(0);
/// Lines whose reference was leaked because their CPU stayed inside the handler past the bound.
static LEAKED: AtomicU64 = AtomicU64::new(0);
/// Deliveries refused because the CPU was already inside an MSI handler (an action turned interrupts on).
static NESTED: AtomicU64 = AtomicU64::new(0);

/// How long `free` waits for the owning CPU to leave the vector's handler (spin iterations, a few ms at most:
/// a handler takes microseconds, so only a stopped CPU, e.g. on the panic path, reaches the bound).
const UNBIND_SPINS: u64 = 200_000;

fn trampoline(vector: u8) {
    let Some(t) = SLOTS.find_for(crate::percpu::this_cpu()) else {
        UNBOUND.fetch_add(1, Ordering::Relaxed);
        return;
    };
    // SeqCst pairs with `release`: either it sees this CPU in flight, or this load sees the slot's new value.
    // A CPU already inside an MSI handler (only if an action broke the no-nesting rule) keeps the outer
    // marker: overwriting it would let `release` drop the outer line while it still runs.
    if t.in_flight.compare_exchange(0, vector as u32 + 1, Ordering::SeqCst, Ordering::SeqCst).is_err() {
        NESTED.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let p = t.slots[vector as usize].load(Ordering::SeqCst);
    if p.is_null() {
        UNBOUND.fetch_add(1, Ordering::Relaxed);
    } else {
        // SAFETY: only `alloc` and `rebind` store non-null slot values, each a reference from `Arc::into_raw`.
        // `release` (via `free`/`unbind`/`rebind`) takes a slot's old reference back only after it swapped the
        // slot and saw this CPU's `in_flight` not naming this vector; this CPU set `in_flight` before loading
        // the slot (SeqCst on both sides), and it stays set until `fire` returns because MSI handlers never
        // nest on a CPU (IF=0 in `vectors::dispatch`, actions never enable interrupts; a nested delivery is
        // refused above). So the line outlives this call.
        unsafe { (*p).fire() };
    }
    t.in_flight.store(0, Ordering::SeqCst);
}

/// Allocates a vector on `cpu` (None: the CPU with the most free vectors) and binds `line` to it. Thread
/// context (may allocate the CPU's slot table). The caller programs the device afterwards; an interrupt that
/// arrives earlier (a stale message) only counts as unbound.
pub fn alloc(line: &Arc<Line>, cpu: Option<u32>) -> Result<Vector, VectorError> {
    if let Some(c) = cpu {
        SLOTS.get_for(c as usize).ok_or(VectorError::NoSuchCpu)?;
    }
    let v = vectors::alloc(trampoline, cpu)?;
    let Some(t) = SLOTS.get_for(v.cpu as usize) else {
        vectors::free(v);
        return Err(VectorError::NoSuchCpu);
    };
    let raw = Arc::into_raw(line.clone()) as *mut Line;
    let old = t.slots[v.vector as usize].swap(raw, Ordering::SeqCst);
    if !old.is_null() {
        // Only a leaked slot (see `unbind`) can still hold a line here; it stays leaked.
        LEAKED.fetch_add(1, Ordering::Relaxed);
    }
    Ok(v)
}

/// Unbinds `v` (a later delivery counts as unbound) and gives the vector back. Mask the source first. Never
/// allocates; frees the line only if nobody else holds it (the PCI core always does while its set lives).
/// False if the owning CPU did not leave the handler in time (the line's reference was leaked instead).
pub fn free(v: Vector) -> bool {
    let ok = unbind(v);
    vectors::free(v);
    ok
}

/// Binds `line` instead of whatever `v` carried (an MSI block's vectors are first taken for one line, then
/// each given its own). False if `v` is not an allocated MSI vector here or the old line could not be let
/// go in time (it is leaked then, `line` is bound all the same).
pub fn rebind(v: Vector, line: &Arc<Line>) -> bool {
    if !vectors::is_registered(v) {
        return false;
    }
    let Some(t) = SLOTS.find_for(v.cpu as usize) else { return false };
    let raw = Arc::into_raw(line.clone()) as *mut Line;
    let old = t.slots[v.vector as usize].swap(raw, Ordering::SeqCst);
    release(t, v, old)
}

fn unbind(v: Vector) -> bool {
    let Some(t) = SLOTS.find_for(v.cpu as usize) else { return true };
    let p = t.slots[v.vector as usize].swap(core::ptr::null_mut(), Ordering::SeqCst);
    release(t, v, p)
}

/// Drops the slot's old reference `p` of `v` once the owning CPU is not inside that vector's handler.
fn release(t: &CpuSlots, v: Vector, p: *mut Line) -> bool {
    if p.is_null() {
        return true;
    }
    let mine = v.vector as u32 + 1;
    let mut spins = 0u64;
    while t.in_flight.load(Ordering::SeqCst) == mine {
        spins += 1;
        if spins > UNBIND_SPINS {
            LEAKED.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        core::hint::spin_loop();
    }
    // SAFETY: `p` is a slot's old value, which only `alloc` or `rebind` stored, each from `Arc::into_raw`; the
    // caller swapped it out of the slot, so no later trampoline can load it. A trampoline that loaded it
    // earlier set `in_flight` to this vector first (SeqCst) and keeps it until `fire` returns: handlers never
    // nest on a CPU and a nested delivery never overwrites the marker (see `trampoline`). `in_flight` no
    // longer naming this vector therefore means nobody is using `p`, and this is the only use of that
    // reference.
    drop(unsafe { Arc::from_raw(p) });
    true
}

/// Whether `v` has a line bound (tests, hw_info).
pub fn is_bound(v: Vector) -> bool {
    SLOTS.find_for(v.cpu as usize).is_some_and(|t| !t.slots[v.vector as usize].load(Ordering::Acquire).is_null())
}

/// Deliveries that found no line.
pub fn unbound_count() -> u64 {
    UNBOUND.load(Ordering::Relaxed)
}

/// Line references leaked because a CPU did not leave its handler in time.
pub fn leaked_count() -> u64 {
    LEAKED.load(Ordering::Relaxed)
}

/// Deliveries refused because they arrived inside another MSI handler (should stay 0).
pub fn nested_count() -> u64 {
    NESTED.load(Ordering::Relaxed)
}

#[cfg(test)]
pub(crate) mod tests {
    extern crate std;
    use super::*;
    use std::sync::atomic::{AtomicU8, AtomicUsize};
    use std::sync::{Mutex, MutexGuard};

    /// The tests below share `SLOTS`, `UNBOUND` and the BSP's vector table; cargo runs tests on parallel
    /// threads, so they take turns (a vector one test frees would otherwise be the next one another gets).
    /// `vectors`' own host tests on the global table can take it too (change request to CP).
    static SERIAL: Mutex<()> = Mutex::new(());

    pub(crate) fn serial() -> MutexGuard<'static, ()> {
        SERIAL.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn compose_and_decode() {
        let m = compose(3, 0x41).unwrap();
        assert_eq!(m.address, 0xFEE0_3000);
        assert_eq!(m.data, 0x41);
        assert_eq!((m.address_lo(), m.address_hi()), (0xFEE0_3000, 0));
        assert_eq!(m.decode(), Some((3, 0x41)));
        assert_eq!(compose(255, 0xEF).unwrap().address, 0xFEEF_F000);
        assert_eq!(compose(256, 0x30), Err(MsiError::DestinationTooWide(256)));
        assert_eq!(Message { address: 0xFED0_0000, data: 0x30 }.decode(), None, "not the LAPIC window");
        assert_eq!(Message { address: 0xFEE0_0004, data: 0x30 }.decode(), None, "logical destination mode");
        assert_eq!(Message { address: 0xFEE0_0000, data: 0x4130 }.decode(), None, "level-triggered");
    }

    fn counting() -> (Arc<AtomicUsize>, Box<dyn Fn() + Send + Sync>) {
        let n = Arc::new(AtomicUsize::new(0));
        let m = n.clone();
        (
            n,
            Box::new(move || {
                m.fetch_add(1, Ordering::SeqCst);
            }),
        )
    }

    #[test]
    fn storm_guard_counts_per_window() {
        let (acts, a) = counting();
        let (storms, s) = counting();
        let l = Line::new(a, s, None);
        l.set_storm_limit(3, 1000);
        for t in [10, 20, 30] {
            l.fire_at(t);
        }
        assert!(!l.is_stormed());
        l.fire_at(40);
        assert!(l.is_stormed(), "the fourth within one window is a storm");
        assert_eq!(storms.load(Ordering::SeqCst), 1);
        assert_eq!(acts.load(Ordering::SeqCst), 4, "the tipping delivery still reaches the owner");
        l.fire_at(50);
        l.fire_at(60);
        assert_eq!(acts.load(Ordering::SeqCst), 4, "stormed: only counted");
        assert_eq!((l.deliveries(), l.suppressed(), l.storms()), (6, 2, 1));
        l.clear_storm();
        for t in [5000, 5100, 5200] {
            l.fire_at(t);
        }
        assert!(!l.is_stormed());
        l.fire_at(7000);
        l.fire_at(7100);
        assert!(!l.is_stormed(), "a new window starts after `window` ticks");
        assert_eq!(acts.load(Ordering::SeqCst), 9);
    }

    #[test]
    fn guard_off_and_soft_mask() {
        let (acts, a) = counting();
        let (_, s) = counting();
        let l = Line::new(a, s, None);
        for t in 0..1000 {
            l.fire_at(t);
        }
        assert!(!l.is_stormed(), "limit 0: no guard");
        assert!(!l.set_soft_mask(true));
        l.fire_at(2000);
        assert_eq!((acts.load(Ordering::SeqCst), l.suppressed()), (1000, 1));
        assert!(l.set_soft_mask(false));
        l.retrigger();
        assert_eq!(acts.load(Ordering::SeqCst), 1001);
    }

    #[test]
    fn bind_fire_free_on_the_host() {
        // Host build: one CPU (0), `this_cpu` = 0.
        let _s = serial();
        let (acts, a) = counting();
        let (_, s) = counting();
        let l = Arc::new(Line::new(a, s, None));
        let v = alloc(&l, Some(0)).unwrap();
        assert!(is_bound(v) && vectors::is_registered(v));
        assert_eq!(Arc::strong_count(&l), 2, "the slot holds one reference");
        trampoline(v.vector);
        trampoline(v.vector);
        assert_eq!(acts.load(Ordering::SeqCst), 2);
        assert_eq!(l.deliveries(), 2);
        assert!(free(v));
        // Not `!vectors::is_registered(v)`: `vectors`' own tests may take the freed vector at once.
        assert!(!is_bound(v));
        assert_eq!(Arc::strong_count(&l), 1, "free gave the slot's reference back");
        let before = unbound_count();
        trampoline(v.vector);
        assert_eq!(unbound_count(), before + 1);
        assert_eq!(acts.load(Ordering::SeqCst), 2);
        // Not a second `free(v)`: the freed number may belong to a `vectors` test by now.
        assert!(unbind(v), "a second unbind is harmless");
        assert_eq!(alloc(&l, Some(7)), Err(VectorError::NoSuchCpu));
        assert_eq!(Arc::strong_count(&l), 1);
    }

    #[test]
    fn rebind_moves_the_reference() {
        let _s = serial();
        let (a_acts, a) = counting();
        let (b_acts, b) = counting();
        let la = Arc::new(Line::new(a, Box::new(|| {}), None));
        let lb = Arc::new(Line::new(b, Box::new(|| {}), None));
        let v = alloc(&la, Some(0)).unwrap();
        assert!(rebind(v, &lb));
        assert_eq!((Arc::strong_count(&la), Arc::strong_count(&lb)), (1, 2));
        trampoline(v.vector);
        assert_eq!((a_acts.load(Ordering::SeqCst), b_acts.load(Ordering::SeqCst)), (0, 1));
        assert!(free(v));
        assert_eq!(Arc::strong_count(&lb), 1);
        // A reserved vector is never allocated (a freed dynamic one might be, by `vectors`' own tests).
        assert!(!rebind(Vector { cpu: 0, vector: 0x20 }, &la), "not an allocated vector");
        assert_eq!(Arc::strong_count(&la), 1);
    }

    #[test]
    fn nested_delivery_is_refused() {
        let _s = serial();
        static INNER: AtomicU8 = AtomicU8::new(0);
        let (outer_acts, o) = counting();
        let (inner_acts, i) = counting();
        let outer = Arc::new(Line::new(
            Box::new(move || {
                o();
                // An action that broke the rule and let a second MSI in on this CPU.
                trampoline(INNER.load(Ordering::SeqCst));
            }),
            Box::new(|| {}),
            None,
        ));
        let inner = Arc::new(Line::new(i, Box::new(|| {}), None));
        let vo = alloc(&outer, Some(0)).unwrap();
        let vi = alloc(&inner, Some(0)).unwrap();
        INNER.store(vi.vector, Ordering::SeqCst);
        let before = nested_count();
        trampoline(vo.vector);
        assert_eq!((outer_acts.load(Ordering::SeqCst), inner_acts.load(Ordering::SeqCst)), (1, 0));
        assert_eq!(nested_count(), before + 1, "the nested delivery was refused, the outer marker kept");
        trampoline(vi.vector);
        assert_eq!(inner_acts.load(Ordering::SeqCst), 1, "after the outer handler left, the marker is free again");
        assert!(free(vo) && free(vi));
        assert_eq!((Arc::strong_count(&outer), Arc::strong_count(&inner)), (1, 1));
    }
}
