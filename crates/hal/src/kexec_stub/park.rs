//! Stopping the other CPUs before Reload's jump (owner: RB, item RB-5b; spec/core.md §2.4, §6.1, §10.2).
//!
//! The boot CPU stops every other online CPU in three steps, each one only after the one before succeeded:
//! 1. **park** (`stop_others`): a fixed IPI on `PARK_VECTOR` brings each AP into `park_here`, a known loop with
//!    interrupts off. An AP takes the IPI only with interrupts on, so it holds no `IrqSpinLock` then (R5), and the
//!    I/O APIC masking and device quiesce that follow cannot wait for a lock a parked AP holds. The IPI is sent
//!    again to an AP that has not answered (a park guard may have deferred it, see `set_park_guard`). An AP that
//!    does not answer within the deadline gets one NMI (Linux `native_stop_other_cpus`: REBOOT_VECTOR first, NMI
//!    after a timeout). One that answers neither makes `stop_others` refuse and release the others, so the Reload
//!    is refused before any device is touched and this kernel goes on.
//! 2. **quiet** (`Stopped::quiet`): each parked AP masks its own LAPIC timer and LVTs and halts with interrupts
//!    off. From here there is no way back.
//! 3. **INIT** (`Quiet::send_init`, boot CPU with interrupts off only): INIT to every AP, and to the APIC IDs the
//!    caller names besides (the MADT's CPUs that never came online); each AP then waits for a SIPI, which only the new kernel's AP
//!    bring-up sends (INIT, 10 ms, SIPI, spec §6.1). A halted CPU is no safe place across the jump: an NMI, SMI or
//!    machine check resumes it in code the new kernel overwrites; a CPU waiting for SIPI runs nothing (Linux 6.5
//!    `smp_park_other_cpus_in_init`, "x86/smp: Put CPUs into INIT on shutdown if possible").
//!
//! `jump` refuses unless step 3 reached every AP (or this is the only CPU).
//!
//! Nothing here allocates or takes a lock on the AP side; the per-CPU records are created by the boot CPU before
//! the first IPI goes out.

use crate::apic::{self, IpiDest, IpiKind};
use crate::idt::{self, ExceptionHook, TrapFrame};
use crate::percpu::{self, PerCpu};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering};

/// The system vector of the park IPI (0xF0 LAPIC timer and 0xF1 `SMP_CALL` are CP's; RB takes 0xFC).
pub const PARK_VECTOR: u8 = 0xFC;

/// How often an AP that has not parked gets the IPI again.
const RESEND_NS: u64 = 10_000_000;

// Phases of a stop (one at a time).
const IDLE: u8 = 0;
const STOP: u8 = 1;
const QUIET: u8 = 2;
const RELEASE: u8 = 3;

// Per-CPU states.
const RUNNING: u8 = 0;
const PARKED_IRQ: u8 = 1;
const PARKED_NMI: u8 = 2;
const HALTED: u8 = 3;

struct CpuPark {
    state: AtomicU8,
    /// NMIs a stop sent to this CPU that have not arrived yet: the NMI hook takes one each (and parks the CPU if
    /// a stop runs). Never reset by a new stop: an NMI sent while the CPU had NMIs blocked (inside an NMI handler)
    /// arrives late, maybe during the next stop, and must still be taken, not reach the panic path. NMIs sent
    /// while one is latched collapse into one, so the count can stay above the truth; the cost is that an
    /// unrelated NMI later is consumed instead of chained (see `take_owed`).
    nmi_owed: AtomicU32,
}

static PARK: PerCpu<CpuPark> = PerCpu::new(|_| CpuPark { state: AtomicU8::new(RUNNING), nmi_owed: AtomicU32::new(0) });
/// Set once `PARK` has a record for every possible CPU (built by the boot CPU before any handler reads it).
static TABLE_READY: AtomicBool = AtomicBool::new(false);
static PHASE: AtomicU8 = AtomicU8::new(IDLE);
static BUSY: AtomicBool = AtomicBool::new(false);
/// CPUs inside `park_here` now.
static INSIDE: AtomicUsize = AtomicUsize::new(0);
/// Every AP was sent INIT (the jump's condition with more than one CPU). Never cleared: the APs are gone.
static ALL_INIT: AtomicBool = AtomicBool::new(false);
static INSTALLED: AtomicBool = AtomicBool::new(false);
static PREV_NMI_HOOK: AtomicUsize = AtomicUsize::new(0);
static GUARD: AtomicUsize = AtomicUsize::new(0);
/// Park IPIs and owed NMIs that arrived outside a stop (late ones after a release), and IPIs a guard deferred.
static STRAY: AtomicU64 = AtomicU64::new(0);
static DEFERRED: AtomicU64 = AtomicU64::new(0);

/// Why a stop was refused. Every refusal left the other CPUs running as before.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopError {
    /// The checks of `check_cpus` (not the boot CPU, no LAPIC, an AP without an APIC ID).
    Cpus(super::StubError),
    /// Another stop runs, or CPUs of a released one have not left the park loop yet.
    Busy,
    /// `PARK_VECTOR` has another owner.
    VectorTaken,
    /// These CPUs answered neither the park IPI nor the NMI (`nmi_sent` of them got one).
    NotStopped { cpus: Vec<usize>, nmi_sent: usize },
}

/// How long to wait; `clock` in ns (the kernel passes `time::now_ns`; hal has no clock of its own).
#[derive(Clone, Copy)]
pub struct StopOptions {
    pub clock: fn() -> u64,
    /// For the park IPI.
    pub irq_wait_ns: u64,
    /// For the NMI to the CPUs that did not answer the IPI; `None` = no NMI (refuse right away).
    pub nmi_wait_ns: Option<u64>,
}

/// What a stop did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StopReport {
    /// Other CPUs online when the stop began.
    pub others: usize,
    pub by_irq: usize,
    pub by_nmi: usize,
    /// Park IPIs sent (first round and re-sends).
    pub ipis: u32,
    pub ns: u64,
    /// The lowest-numbered CPU that had to be parked by NMI (it had interrupts off: a kernel bug).
    pub first_nmi_cpu: Option<usize>,
}

/// What the last two steps did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InitReport {
    pub others: usize,
    /// APs that masked their LAPIC and halted before the deadline.
    pub halted: usize,
    pub init_sent: usize,
    pub init_failed: usize,
    /// INIT to APIC IDs that are not online (CPUs that failed to come up; CP sent them INIT at boot already).
    pub unused_sent: usize,
    pub unused_failed: usize,
    /// Nothing was sent: not the boot CPU, or interrupts on (`init_failed` = `others`).
    pub refused: bool,
}

/// The other CPUs are parked; dropping this releases them (they return to what they did).
pub struct Stopped {
    report: StopReport,
    clock: fn() -> u64,
    done: bool,
}

/// The other CPUs are halted with their LAPICs quiet; only INIT follows.
pub struct Quiet {
    report: StopReport,
    halted: usize,
}

/// A hook a lock-aware layer (SC) can set: called on the AP inside the park IPI's handler; `false` = this CPU is
/// in no state to park (e.g. it holds a `SpinLock` taken with interrupts on), the IPI is ignored and comes again
/// `RESEND_NS` later. Must not allocate or take a lock.
pub fn set_park_guard(f: fn() -> bool) {
    GUARD.store(f as usize, Ordering::Release);
}

/// The guard set now (tests put the previous one back).
pub fn park_guard() -> Option<fn() -> bool> {
    let g = GUARD.load(Ordering::Acquire);
    // SAFETY: only `set_park_guard` stores into GUARD, always a `fn() -> bool`.
    (g != 0).then(|| unsafe { core::mem::transmute::<usize, fn() -> bool>(g) })
}

/// Park IPIs that came outside a stop, and those a guard deferred (for `reload_info` and tests).
pub fn stray_and_deferred() -> (u64, u64) {
    (STRAY.load(Ordering::Relaxed), DEFERRED.load(Ordering::Relaxed))
}

/// Whether every other CPU was sent INIT (or there is no other CPU): what `jump` needs.
pub fn others_stopped() -> bool {
    percpu::cpu_count() <= 1 || ALL_INIT.load(Ordering::Acquire)
}

fn slot(cpu: usize) -> Option<&'static CpuPark> {
    // Without the table a handler must not touch `PARK`: `get_for` would allocate in interrupt context.
    TABLE_READY.load(Ordering::Acquire).then(|| PARK.get_for(cpu)).flatten()
}

/// What the NMI hook does (pure; host-tested).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NmiAction {
    Park,
    /// Taken by the stop (owed).
    Consume,
    /// Not the stop's, but this CPU is in the park loop: the previous hook may take it (an NMI sample of
    /// `hal::nmi` is answered from here), and if none does it is swallowed, never a panic there. Swallowing it
    /// unseen (as before desk fix/nmi) left the sampler's slot waiting for good: 12 of 15 CPUs on the Mini-PC
    /// never answered a sample again after one landed while they sat here.
    Offer,
    /// Not the stop's: the previous hook (or the default path) decides.
    Chain,
}

/// Takes one owed NMI if there is one (saturating: never below 0).
fn take_owed(owed: &AtomicU32) -> bool {
    owed.try_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1)).is_ok()
}

fn nmi_action(phase: u8, state: u8, owed: bool) -> NmiAction {
    match (state, owed) {
        (s, true) if s != RUNNING => NmiAction::Consume,
        (s, false) if s != RUNNING => NmiAction::Offer,
        (_, true) if phase == STOP => NmiAction::Park,
        (_, true) => NmiAction::Consume,
        _ => NmiAction::Chain,
    }
}

/// The loop a stopped CPU waits in (interrupts off: inside an interrupt or NMI handler).
fn park_here(me: &CpuPark, via: u8) {
    INSIDE.fetch_add(1, Ordering::AcqRel);
    me.state.store(via, Ordering::Release);
    let mut spins = 0u32;
    loop {
        match PHASE.load(Ordering::Acquire) {
            STOP => {
                // RB-quiesce: a reboot's quiesce and reset run for a while beside the parked CPUs; a kernel TLB
                // shootdown meanwhile (a driver unmapping its registers) is answered from here instead of giving
                // up on this CPU after its bound (`tlb::poll` is made for loops with interrupts off). Not from the
                // NMI park: the NMI may have come in the middle of this CPU's own TLB service.
                spins = spins.wrapping_add(1);
                if via == PARKED_IRQ && spins.is_multiple_of(256) {
                    crate::paging::tlb::poll();
                }
                core::hint::spin_loop();
            }
            QUIET => {
                super::quiet_local_apic();
                me.state.store(HALTED, Ordering::Release);
                // The boot CPU's INIT ends this; an NMI meanwhile is consumed by the hook and lands here again.
                loop {
                    crate::cpu::disable_interrupts();
                    crate::cpu::halt();
                }
            }
            _ => break,
        }
    }
    me.state.store(RUNNING, Ordering::Release);
    INSIDE.fetch_sub(1, Ordering::AcqRel);
}

fn park_irq(_v: u8) {
    let Some(me) = slot(percpu::this_cpu()) else {
        STRAY.fetch_add(1, Ordering::Relaxed);
        return;
    };
    if PHASE.load(Ordering::Acquire) != STOP || me.state.load(Ordering::Acquire) != RUNNING {
        STRAY.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let g = GUARD.load(Ordering::Acquire);
    if g != 0 {
        // SAFETY: only `set_park_guard` stores into GUARD, always a `fn() -> bool`.
        let f: fn() -> bool = unsafe { core::mem::transmute::<usize, fn() -> bool>(g) };
        if !f() {
            DEFERRED.fetch_add(1, Ordering::Relaxed);
            return;
        }
    }
    park_here(me, PARKED_IRQ);
}

fn park_nmi(frame: &mut TrapFrame) -> bool {
    if let Some(me) = slot(percpu::this_cpu()) {
        let owed = take_owed(&me.nmi_owed);
        match nmi_action(PHASE.load(Ordering::Acquire), me.state.load(Ordering::Acquire), owed) {
            NmiAction::Park => {
                park_here(me, PARKED_NMI);
                return true;
            }
            NmiAction::Consume => return true,
            NmiAction::Offer => {
                let _ = chain_nmi(frame);
                return true;
            }
            NmiAction::Chain => {}
        }
    }
    chain_nmi(frame)
}

/// The NMI hook that was there before `install` (false: none, the default path decides).
fn chain_nmi(frame: &mut TrapFrame) -> bool {
    let prev = PREV_NMI_HOOK.load(Ordering::Acquire);
    if prev == 0 {
        return false;
    }
    // SAFETY: PREV_NMI_HOOK holds 0 or the hook `idt::exception_hook` returned at install, an `ExceptionHook`.
    let f: ExceptionHook = unsafe { core::mem::transmute::<usize, ExceptionHook>(prev) };
    f(frame)
}

/// The park vector and the NMI hook, once (the boot CPU, thread context).
fn install() -> Result<(), StopError> {
    if INSTALLED.load(Ordering::Acquire) {
        return Ok(());
    }
    crate::vectors::register_system(PARK_VECTOR, park_irq).map_err(|_| StopError::VectorTaken)?;
    PREV_NMI_HOOK.store(idt::exception_hook(idt::NMI).map_or(0, |h| h as usize), Ordering::Release);
    idt::set_exception_hook(idt::NMI, park_nmi);
    INSTALLED.store(true, Ordering::Release);
    Ok(())
}

fn wait_until(clock: fn() -> u64, deadline: u64, mut done: impl FnMut() -> bool) -> bool {
    let mut spins = 0u32;
    loop {
        if done() {
            return true;
        }
        spins = spins.wrapping_add(1);
        if spins.is_multiple_of(64) && clock() >= deadline {
            return done();
        }
        core::hint::spin_loop();
    }
}

fn state_of(cpu: usize) -> u8 {
    slot(cpu).map_or(RUNNING, |s| s.state.load(Ordering::Acquire))
}

fn send_park(cpu: usize) -> bool {
    crate::smp::apic_id(cpu).is_some_and(|id| apic::send_ipi(IpiDest::Physical(id), IpiKind::Fixed(PARK_VECTOR)).is_ok())
}

/// Step 1: parks every other online CPU, or refuses (and releases those it parked). Runs on the boot CPU in
/// thread context; allocates only before the first IPI.
pub fn stop_others(o: &StopOptions) -> Result<Stopped, StopError> {
    let t0 = (o.clock)();
    super::check_cpus().map_err(StopError::Cpus)?;
    let count = percpu::cpu_count();
    if count <= 1 {
        return Ok(Stopped { report: StopReport::default(), clock: o.clock, done: true });
    }
    if INSIDE.load(Ordering::Acquire) != 0 || BUSY.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_err() {
        return Err(StopError::Busy);
    }
    if let Err(e) = install() {
        BUSY.store(false, Ordering::Release);
        return Err(e);
    }
    for c in 0..percpu::possible_cpus().max(count) {
        // `nmi_owed` stays: see its doc.
        if let Some(s) = PARK.get_for(c) {
            s.state.store(RUNNING, Ordering::Relaxed);
        }
    }
    TABLE_READY.store(true, Ordering::Release);
    // From here `Stopped`'s drop releases whatever parked, on every path.
    let mut st = Stopped { report: StopReport { others: count - 1, ..StopReport::default() }, clock: o.clock, done: false };
    PHASE.store(STOP, Ordering::SeqCst);
    let pending = |cpu: &usize| state_of(*cpu) == RUNNING;
    let deadline = t0.saturating_add(o.irq_wait_ns);
    let mut last_send = 0u64;
    loop {
        let now = (o.clock)();
        if last_send == 0 || now.saturating_sub(last_send) >= RESEND_NS {
            for c in (1..count).filter(pending) {
                send_park(c);
                st.report.ipis += 1;
            }
            last_send = now.max(1);
        }
        let slice_end = now.saturating_add(RESEND_NS).min(deadline);
        if wait_until(o.clock, slice_end, || (1..count).all(|c| !pending(&c))) || now >= deadline {
            break;
        }
    }
    let mut left: Vec<usize> = (1..count).filter(pending).collect();
    let mut nmi_sent = 0;
    if !left.is_empty()
        && let Some(nmi_ns) = o.nmi_wait_ns
    {
        for &c in &left {
            let (Some(s), Some(id)) = (slot(c), crate::smp::apic_id(c)) else { continue };
            // Counted before the send: the NMI can arrive before `send_ipi` returns.
            s.nmi_owed.fetch_add(1, Ordering::AcqRel);
            if apic::send_ipi(IpiDest::Physical(id), IpiKind::Nmi).is_ok() {
                nmi_sent += 1;
            } else {
                take_owed(&s.nmi_owed);
            }
        }
        let d = (o.clock)().saturating_add(nmi_ns);
        wait_until(o.clock, d, || left.iter().all(|c| !pending(c)));
        left.retain(pending);
    }
    if !left.is_empty() {
        return Err(StopError::NotStopped { cpus: left, nmi_sent });
    }
    st.report.by_irq = (1..count).filter(|&c| state_of(c) == PARKED_IRQ).count();
    st.report.by_nmi = (1..count).filter(|&c| state_of(c) == PARKED_NMI).count();
    st.report.first_nmi_cpu = (1..count).find(|&c| state_of(c) == PARKED_NMI);
    st.report.ns = (o.clock)().saturating_sub(t0);
    Ok(st)
}

impl Stopped {
    pub fn report(&self) -> StopReport {
        self.report
    }

    /// Step 2: every parked CPU masks its LAPIC timer and LVTs and halts (waits up to `wait_ns` for them to say
    /// so; INIT follows either way). No way back from here.
    pub fn quiet(mut self, wait_ns: u64) -> Quiet {
        self.done = true;
        let count = self.report.others + 1;
        if count <= 1 {
            return Quiet { report: self.report, halted: 0 };
        }
        PHASE.store(QUIET, Ordering::SeqCst);
        let d = (self.clock)().saturating_add(wait_ns);
        wait_until(self.clock, d, || (1..count).all(|c| state_of(c) == HALTED));
        let halted = (1..count).filter(|&c| state_of(c) == HALTED).count();
        Quiet { report: self.report, halted }
    }
}

impl Drop for Stopped {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        PHASE.store(RELEASE, Ordering::SeqCst);
        // Bounded: a parked CPU leaves its loop as soon as it sees the phase (a few hundred ns).
        let d = (self.clock)().saturating_add(1_000_000_000);
        wait_until(self.clock, d, || INSIDE.load(Ordering::Acquire) == 0);
        PHASE.store(IDLE, Ordering::SeqCst);
        BUSY.store(false, Ordering::Release);
    }
}

/// Whether this CPU may send the Reload's INIT now: the boot CPU (`check_cpus`) with interrupts off. Read only
/// (tests ask it on every CPU).
pub fn init_allowed() -> bool {
    super::check_cpus().is_ok() && !crate::cpu::interrupts_enabled()
}

impl Quiet {
    pub fn report(&self) -> StopReport {
        self.report
    }

    /// Step 3, the Reload's point of no return: INIT to every other online CPU and to `unused` (APIC IDs of CPUs
    /// that never came online, as Linux INITs every present CPU), then `settle_ns` for the IPIs to land (x2APIC
    /// gives no delivery status). Only the boot CPU with interrupts off sends anything: elsewhere INIT would hit
    /// a CPU this one still needs, so the call refuses (`refused`, every AP counted in `init_failed`).
    /// `others_stopped` becomes true only if every online AP was sent INIT; `unused` IDs do not count for it
    /// (CP's bring-up sent them INIT already).
    pub fn send_init(self, clock: fn() -> u64, settle_ns: u64, unused: &[u32]) -> InitReport {
        let others = self.report.others;
        let mut r = InitReport { others, halted: self.halted, ..InitReport::default() };
        if others == 0 {
            return r;
        }
        if !init_allowed() {
            r.init_failed = others;
            r.refused = true;
            return r;
        }
        for c in 1..=others {
            let sent = crate::smp::apic_id(c).is_some_and(|id| apic::send_init_sipi(id, IpiKind::Init).is_ok());
            if sent {
                r.init_sent += 1;
            } else {
                r.init_failed += 1;
            }
        }
        let me = apic::id();
        for &id in unused.iter().filter(|&&id| Some(id) != me && crate::smp::cpu_of_apic(id).is_none()) {
            if apic::send_init_sipi(id, IpiKind::Init).is_ok() {
                r.unused_sent += 1;
            } else {
                r.unused_failed += 1;
            }
        }
        let d = clock().saturating_add(settle_ns);
        wait_until(clock, d, || false);
        if r.init_failed == 0 {
            ALL_INIT.store(true, Ordering::Release);
        }
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nmi_hook_decisions() {
        // A CPU in the park loop never lets an NMI through to the panic path: the stop's own is taken, any
        // other is offered to the previous hook (an NMI sample is answered from the park loop) and then taken.
        for phase in [IDLE, STOP, QUIET, RELEASE] {
            for st in [PARKED_IRQ, PARKED_NMI, HALTED] {
                assert_eq!(nmi_action(phase, st, true), NmiAction::Consume, "phase {phase} state {st}");
                assert_eq!(nmi_action(phase, st, false), NmiAction::Offer, "phase {phase} state {st}");
            }
        }
        assert_eq!(nmi_action(STOP, RUNNING, true), NmiAction::Park);
        // An owed NMI that lands after the stop ended is swallowed, not a panic.
        assert_eq!(nmi_action(IDLE, RUNNING, true), NmiAction::Consume);
        assert_eq!(nmi_action(RELEASE, RUNNING, true), NmiAction::Consume);
        // Not the stop's NMI: the previous hook decides.
        assert_eq!(nmi_action(STOP, RUNNING, false), NmiAction::Chain);
        assert_eq!(nmi_action(IDLE, RUNNING, false), NmiAction::Chain);
    }

    #[test]
    fn one_cpu_needs_nothing() {
        // Host: one CPU, so every step is a no-op and the jump's CPU condition holds.
        assert!(others_stopped());
        fn clock() -> u64 {
            0
        }
        let st = stop_others(&StopOptions { clock, irq_wait_ns: 0, nmi_wait_ns: None }).expect("one CPU");
        assert_eq!(st.report(), StopReport::default());
        let q = st.quiet(0);
        let r = q.send_init(clock, 0, &[2, 3]);
        assert_eq!(r, InitReport::default());
    }

    #[test]
    fn owed_nmis_survive_a_new_stop() {
        // Stop 1 sent an NMI the CPU could not take yet (it was inside an NMI handler) and was refused; stop 2
        // starts (it no longer resets the count) and sends another one, which collapses with the latched one.
        let owed = AtomicU32::new(0);
        owed.fetch_add(1, Ordering::AcqRel);
        owed.fetch_add(1, Ordering::AcqRel);
        // The latched NMI lands during stop 2: taken, and it parks the CPU.
        assert_eq!(nmi_action(STOP, RUNNING, take_owed(&owed)), NmiAction::Park);
        // Released before the second (collapsed) one: the next NMI is still taken, never a panic.
        assert_eq!(nmi_action(IDLE, RUNNING, take_owed(&owed)), NmiAction::Consume);
        // Count back at 0: an unrelated NMI goes to the previous hook again; the count never wraps.
        assert_eq!(nmi_action(IDLE, RUNNING, take_owed(&owed)), NmiAction::Chain);
        assert_eq!(owed.load(Ordering::Acquire), 0);
        // A late owed NMI that lands while an unrelated one is handled first: both are taken.
        owed.fetch_add(1, Ordering::AcqRel);
        assert_eq!(nmi_action(RELEASE, RUNNING, take_owed(&owed)), NmiAction::Consume);
        assert!(!take_owed(&owed));
    }
}
