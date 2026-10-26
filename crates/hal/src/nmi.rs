//! NMI for diagnosis (owner: RB, items RB-2/RB-3; spec/core.md §10.2): the register state and a frame-pointer
//! backtrace of the other CPUs, taken by an NMI (Linux `arch/x86/kernel/apic/hw_nmi.c`,
//! `trigger_all_cpu_backtrace`; `native_stop_other_cpus` for the panic stop).
//!
//! Two uses:
//! - **crash** (`stop_others`, the panic path): every other online CPU gets an NMI, writes its registers and
//!   backtrace into its own slot and stops for good (interrupts and NMIs blocked, `hlt`). The reporting CPU then
//!   prints the slots. A CPU that never answers (NMIs blocked: already inside an NMI handler, parked by Reload's
//!   NMI hook, or dead) is reported as silent. A CPU that is not online yet (an AP between INIT-SIPI and
//!   `ONLINE`) gets no NMI: it may still run the real-mode trampoline without an IDT, where an NMI would reset the
//!   machine; it is reported as not online and keeps running (a short window during SMP bring-up).
//! - **sample** (`sample_others`, ktests and later RB-3's `cpus` tool): the same, but the CPU returns to what it
//!   did. One sample at a time. A CPU that still owes an earlier sample's answer is asked again: its NMI may be
//!   lost for good (taken by another hook, e.g. Reload's park hook while the CPU sat in the park loop; desk
//!   fix/nmi, Mini-PC 20.10.: 12 of 15 CPUs silent until reboot). Each slot counts our NMIs in flight (`owed`),
//!   so a spare one (two of ours collapsed or crossed) is swallowed, never left to the "kernel NMI" panic.
//!
//! The handler is an `idt` exception hook on vector 2 (chained: an NMI that is none of ours goes to the hook that
//! was there before, e.g. none -> the generic "kernel NMI" panic). It runs on the NMI IST stack, uses no lock, no
//! allocation and no klog: the slots are atomics created for every possible CPU by `prepare` (thread context,
//! before the first NMI). A late NMI of a sample that already timed out is consumed (its slot says it was asked),
//! never passed on to the panic path.

use crate::apic::{self, IpiDest, IpiKind};
use crate::idt::{self, ExceptionHook, TrapFrame};
use crate::panic::Step;
use crate::percpu::{self, PerCpu};
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering};

/// Backtrace steps kept per CPU (a bound on one report, not on a count of things).
pub const DEPTH: usize = 16;

// Phases.
const NONE: u8 = 0;
const SAMPLE: u8 = 1;
const CRASH: u8 = 2;

// Slot states.
const IDLE: u8 = 0;
const ASKED: u8 = 1;
const SAVING: u8 = 2;
const SAVED: u8 = 3;
const STOPPED: u8 = 4;

/// Registers kept per CPU: the trap frame's 22 words plus CR2, CR3, the TSC and the APIC ID.
const NREGS: usize = 26;

struct Slot {
    state: AtomicU8,
    regs: [AtomicU64; NREGS],
    /// Two words per step: kind (0 none, 1 call, 2 kernel trap, 3 user trap, 4 guess) | vector << 8, then the
    /// address.
    steps: [AtomicU64; 2 * DEPTH],
    nsteps: AtomicUsize,
    /// Times this CPU answered (sample or crash): a sampler sees a fresh answer.
    answers: AtomicU64,
    /// NMIs of ours sent to this CPU that have not arrived yet; the hook takes one per NMI. NMIs that arrive
    /// while one is latched collapse into one, so the count can stay above the truth: then a foreign NMI later
    /// is swallowed (counted in `spare_nmis`) instead of reaching the panic path.
    owed: AtomicU32,
}

static SLOTS: PerCpu<Slot> = PerCpu::new(|_| Slot {
    state: AtomicU8::new(IDLE),
    regs: [const { AtomicU64::new(0) }; NREGS],
    steps: [const { AtomicU64::new(0) }; 2 * DEPTH],
    nsteps: AtomicUsize::new(0),
    answers: AtomicU64::new(0),
    owed: AtomicU32::new(0),
});
/// Set once `SLOTS` has a record for every possible CPU (the handler reads only after this).
static READY: AtomicBool = AtomicBool::new(false);
static PHASE: AtomicU8 = AtomicU8::new(NONE);
/// The CPU that reports a crash (its own NMIs are not ours).
static CRASH_BY: AtomicUsize = AtomicUsize::new(usize::MAX);
static SAMPLING: AtomicBool = AtomicBool::new(false);
static PREV_HOOK: AtomicUsize = AtomicUsize::new(0);
static INSTALLED: AtomicBool = AtomicBool::new(false);
/// NMIs this hook consumed outside a sample or crash (late answers).
static LATE: AtomicU64 = AtomicU64::new(0);
/// Spare NMIs of ours swallowed (a CPU asked again whose earlier NMI came after all).
static SPARE: AtomicU64 = AtomicU64::new(0);

/// A CPU's state as its NMI answer left it.
#[derive(Clone, Copy, Debug, Default)]
pub struct CpuRegs {
    pub cpu: usize,
    pub apic_id: u32,
    pub frame: TrapFrame,
    pub cr2: u64,
    pub cr3: u64,
    pub tsc: u64,
    /// The interrupted code ran in user mode (no kernel backtrace then).
    pub user: bool,
    pub steps: [Step; DEPTH],
    pub nsteps: usize,
}

fn words(f: &TrapFrame) -> [u64; 22] {
    [
        f.r15, f.r14, f.r13, f.r12, f.r11, f.r10, f.r9, f.r8, f.rbp, f.rdi, f.rsi, f.rdx, f.rcx, f.rbx, f.rax, f.vector,
        f.error, f.rip, f.cs, f.rflags, f.rsp, f.ss,
    ]
}

fn from_words(w: &[u64]) -> TrapFrame {
    TrapFrame {
        r15: w[0],
        r14: w[1],
        r13: w[2],
        r12: w[3],
        r11: w[4],
        r10: w[5],
        r9: w[6],
        r8: w[7],
        rbp: w[8],
        rdi: w[9],
        rsi: w[10],
        rdx: w[11],
        rcx: w[12],
        rbx: w[13],
        rax: w[14],
        vector: w[15],
        error: w[16],
        rip: w[17],
        cs: w[18],
        rflags: w[19],
        rsp: w[20],
        ss: w[21],
    }
}

fn encode(s: Step) -> (u64, u64) {
    match s {
        Step::None => (0, 0),
        Step::Call(a) => (1, a),
        Step::Guess(a) => (4, a),
        Step::Trap { vector, rip, user, .. } => (if user { 3 } else { 2 } | (vector & 0xFF) << 8, rip),
    }
}

fn decode(k: u64, a: u64) -> Step {
    match k & 0xFF {
        1 => Step::Call(a),
        2 | 3 => Step::Trap { vector: (k >> 8) & 0xFF, error: 0, rip: a, user: k & 0xFF == 3 },
        4 => Step::Guess(a),
        _ => Step::None,
    }
}

/// How a CPU answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answer {
    /// Not asked (this CPU, or not online).
    NotAsked,
    /// Asked, no answer (yet).
    Silent,
    /// Answered a sample and went on.
    Sampled,
    /// Answered the crash stop and halted.
    Stopped,
}

/// Counts of one stop or sample.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Round {
    pub asked: usize,
    pub answered: usize,
    /// NMIs that could not be sent (NMI IPIs paused by `apic::pause_nmi_ipis`, no LAPIC).
    pub send_failed: usize,
    /// CPUs that were not online (not asked, not stopped).
    pub not_online: usize,
    pub ns: u64,
}

/// Longest wait of one sample (a diagnostic must not hold its caller long; a CPU that needs longer is silent).
pub const MAX_SAMPLE_WAIT_NS: u64 = 1_000_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NmiError {
    /// `prepare` has not run.
    NotPrepared,
    /// Another sample runs, or a crash.
    Busy,
}

/// Creates a slot for every possible CPU and installs the NMI hook (once; thread context; allocates). Call it
/// after the possible-CPU count is known (`acpi::init`) and before NMIs are used.
pub fn prepare() {
    SLOTS.cover_possible();
    READY.store(true, Ordering::Release);
    if !INSTALLED.swap(true, Ordering::AcqRel) {
        PREV_HOOK.store(idt::exception_hook(idt::NMI).map_or(0, |h| h as usize), Ordering::Release);
        idt::set_exception_hook(idt::NMI, nmi_hook);
    }
}

pub fn prepared() -> bool {
    READY.load(Ordering::Acquire)
}

/// What the hook does with an NMI (pure; host-tested). `crash_by_other`: a crash runs and another CPU reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    /// Save, then halt for good (crash).
    SaveAndStop,
    /// Save, then return (sample).
    SaveAndReturn,
    /// A late answer to a finished sample: swallow it.
    Consume,
    /// One of ours that found nothing to answer (the CPU was asked again and its earlier NMI came after all):
    /// swallow it.
    Spare,
    /// Not ours.
    Chain,
}

/// `owed`: this NMI may be one of ours (the slot's count was above 0 and took one).
fn action(phase: u8, crash_by_other: bool, state: u8, owed: bool) -> Action {
    match (phase, state) {
        (CRASH, _) if crash_by_other => Action::SaveAndStop,
        (SAMPLE, ASKED) => Action::SaveAndReturn,
        (_, ASKED) => Action::Consume,
        _ if owed => Action::Spare,
        _ => Action::Chain,
    }
}

/// Takes one owed NMI if there is one (saturating: never below 0).
fn take_owed(owed: &AtomicU32) -> bool {
    owed.try_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1)).is_ok()
}

fn nmi_hook(frame: &mut TrapFrame) -> bool {
    if READY.load(Ordering::Acquire) {
        let me = percpu::this_cpu();
        if let Some(s) = SLOTS.find_for(me) {
            let phase = PHASE.load(Ordering::Acquire);
            let other = CRASH_BY.load(Ordering::Acquire) != me;
            let owed = take_owed(&s.owed);
            match action(phase, other, s.state.load(Ordering::Acquire), owed) {
                Action::SaveAndStop => {
                    s.state.store(SAVING, Ordering::Release);
                    save(s, frame);
                    s.state.store(STOPPED, Ordering::Release);
                    s.answers.fetch_add(1, Ordering::AcqRel);
                    // NMIs stay blocked (no iret ever comes), interrupts off: this CPU is done.
                    crate::panic::halt_forever()
                }
                Action::SaveAndReturn => {
                    if s.state.compare_exchange(ASKED, SAVING, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                        save(s, frame);
                        // A crash stop that began meanwhile set this slot to ASKED again: leave it so (its NMI is
                        // latched and stops this CPU right after the iret), so only STOPPED counts for the crash.
                        if s.state.compare_exchange(SAVING, SAVED, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                            s.answers.fetch_add(1, Ordering::AcqRel);
                        }
                    }
                    return true;
                }
                Action::Consume => {
                    s.state.store(IDLE, Ordering::Release);
                    LATE.fetch_add(1, Ordering::Relaxed);
                    return true;
                }
                Action::Spare => {
                    SPARE.fetch_add(1, Ordering::Relaxed);
                    return true;
                }
                Action::Chain => {}
            }
        }
    }
    let prev = PREV_HOOK.load(Ordering::Acquire);
    if prev == 0 {
        return false;
    }
    // SAFETY: PREV_HOOK holds 0 or the hook `idt::exception_hook` returned at install, an `ExceptionHook`.
    let f: ExceptionHook = unsafe { core::mem::transmute::<usize, ExceptionHook>(prev) };
    f(frame)
}

fn save(s: &Slot, f: &TrapFrame) {
    for (i, w) in words(f).iter().enumerate() {
        s.regs[i].store(*w, Ordering::Relaxed);
    }
    s.regs[22].store(crate::cpu::read_cr2(), Ordering::Relaxed);
    s.regs[23].store(crate::cpu::read_cr3(), Ordering::Relaxed);
    s.regs[24].store(crate::cpu::rdtsc(), Ordering::Relaxed);
    s.regs[25].store(crate::smp::apic_id(percpu::this_cpu()).unwrap_or(u32::MAX) as u64, Ordering::Relaxed);
    let mut steps = [Step::None; DEPTH];
    let user = f.cs & 3 != 0;
    let n = if user {
        0
    } else {
        // The interrupted instruction first (it is not a return address), then its callers. The walk reads only
        // the interrupted stack from its rsp up (`panic::walk_from`): in the entry stubs rbp may still be the
        // program's, and then it points nowhere the walk follows.
        steps[0] = Step::Trap { vector: idt::NMI as u64, error: 0, rip: f.rip, user: false };
        let mut k = 1;
        // Caught at a function's entry or `ret`: its caller is not in the rbp chain (see `entry_return`).
        if let Some(st) = crate::panic::entry_return(f.rip, f.rsp) {
            steps[1] = st;
            k = 2;
        }
        k + crate::panic::walk_from(f.rsp, f.rbp, &mut steps[k..])
    };
    for (i, st) in steps.iter().enumerate() {
        let (k, a) = encode(*st);
        s.steps[2 * i].store(k, Ordering::Relaxed);
        s.steps[2 * i + 1].store(a, Ordering::Relaxed);
    }
    s.nsteps.store(n, Ordering::Relaxed);
}

fn others(me: usize) -> impl Iterator<Item = usize> {
    (0..percpu::cpu_count()).filter(move |&c| c != me)
}

/// Asks every other online CPU by NMI and waits until each answered or `wait_ns` passed on `clock`. `crash`:
/// every slot is asked anew and only STOPPED counts as an answer; a sample asks IDLE slots and those still ASKED
/// by an earlier sample (that NMI may never come), not one SAVING right now, and counts SAVED.
fn ask(clock: fn() -> u64, wait_ns: u64, crash: bool) -> Round {
    let t0 = clock();
    let mut r = Round::default();
    // The NMIs go out with interrupts off: no migration between taking `me` and the sends. The wait below
    // only reads slots, so a move to another CPU then does no harm (that CPU answers its own NMI, if asked).
    let me = {
        let _irq = crate::cpu::IrqGuard::new();
        let me = percpu::this_cpu();
        for c in others(me) {
            let Some(s) = SLOTS.find_for(c) else { continue };
            let Some(id) = crate::smp::apic_id(c) else {
                r.not_online += 1;
                s.state.store(IDLE, Ordering::Release);
                continue;
            };
            r.asked += 1;
            if crash {
                s.state.store(ASKED, Ordering::Release);
            } else if s.state.compare_exchange(IDLE, ASKED, Ordering::AcqRel, Ordering::Acquire).is_err()
                && s.state.load(Ordering::Acquire) != ASKED
            {
                // SAVING: it answers right now.
                continue;
            }
            // Counted before the send: the NMI can arrive before `send_ipi` returns.
            s.owed.fetch_add(1, Ordering::AcqRel);
            if apic::send_ipi(IpiDest::Physical(id), IpiKind::Nmi).is_err() {
                take_owed(&s.owed);
                r.send_failed += 1;
                // No NMI is on its way: a sample gives the slot back (IDLE), so it does not wait for this CPU
                // and the next sample asks it again (review INT-A1d core 1). A crash keeps ASKED: any NMI from
                // elsewhere then stops the CPU as well.
                if !crash {
                    let _ = s.state.compare_exchange(ASKED, IDLE, Ordering::AcqRel, Ordering::Acquire);
                    r.asked -= 1;
                }
            }
        }
        me
    };
    let done = |c: usize| {
        SLOTS.find_for(c).is_some_and(|s| match s.state.load(Ordering::Acquire) {
            STOPPED => true,
            SAVED => !crash,
            _ => false,
        })
    };
    // A sample does not wait for a CPU whose NMI could not be sent (its slot is IDLE again).
    let not_asked = |c: usize| !crash && SLOTS.find_for(c).is_some_and(|s| s.state.load(Ordering::Acquire) == IDLE);
    let online = |c: usize| crate::smp::apic_id(c).is_some();
    let deadline = t0.saturating_add(wait_ns);
    let mut spins = 0u32;
    loop {
        let all = others(me).all(|c| SLOTS.find_for(c).is_none() || !online(c) || done(c) || not_asked(c));
        spins = spins.wrapping_add(1);
        if all || (spins.is_multiple_of(64) && clock() >= deadline) {
            break;
        }
        core::hint::spin_loop();
    }
    r.answered = others(me).filter(|&c| done(c)).count();
    r.ns = clock().saturating_sub(t0);
    r
}

/// The panic path: stops every other online CPU by NMI; each saves its registers and backtrace first
/// (`regs`). Waits at most `wait_ns` on `clock`. Lock- and allocation-free; any CPU may call it, once.
pub fn stop_others(clock: fn() -> u64, wait_ns: u64) -> Result<Round, NmiError> {
    if !READY.load(Ordering::Acquire) {
        return Err(NmiError::NotPrepared);
    }
    let me = percpu::this_cpu();
    if CRASH_BY.compare_exchange(usize::MAX, me, Ordering::AcqRel, Ordering::Acquire).is_err() {
        return Err(NmiError::Busy);
    }
    // From here every NMI on another CPU stops it (whatever its slot says, a sample's late answer included).
    PHASE.store(CRASH, Ordering::SeqCst);
    // Interrupts are off on the panic path, so `ask` sees the same CPU as `me`.
    Ok(ask(clock, wait_ns, true))
}

/// Samples every other online CPU by NMI: each saves its registers and backtrace and goes on. One sample at a
/// time; never while a crash stop runs. Thread or interrupt context; no allocation. Waits at most `wait_ns`
/// (clamped to `MAX_SAMPLE_WAIT_NS`), with interrupts as the caller has them (off only while the NMIs go out).
pub fn sample_others(clock: fn() -> u64, wait_ns: u64) -> Result<Round, NmiError> {
    if !READY.load(Ordering::Acquire) {
        return Err(NmiError::NotPrepared);
    }
    if PHASE.load(Ordering::Acquire) == CRASH || SAMPLING.swap(true, Ordering::AcqRel) {
        return Err(NmiError::Busy);
    }
    // The phase first: a late answer that comes in from now on answers this sample (ASKED -> SAVED) instead
    // of being consumed.
    if PHASE.compare_exchange(NONE, SAMPLE, Ordering::AcqRel, Ordering::Acquire).is_err() {
        SAMPLING.store(false, Ordering::Release);
        return Err(NmiError::Busy);
    }
    for c in 0..percpu::cpu_count() {
        if let Some(s) = SLOTS.find_for(c) {
            // A CPU that still owes an earlier sample's answer keeps ASKED (`ask` sends it another NMI) or SAVING.
            let _ = s.state.compare_exchange(SAVED, IDLE, Ordering::AcqRel, Ordering::Acquire);
        }
    }
    let r = ask(clock, wait_ns.min(MAX_SAMPLE_WAIT_NS), false);
    let _ = PHASE.compare_exchange(SAMPLE, NONE, Ordering::AcqRel, Ordering::Acquire);
    SAMPLING.store(false, Ordering::Release);
    Ok(r)
}

/// How `cpu` answered the last stop or sample.
pub fn answer(cpu: usize) -> Answer {
    match SLOTS.find_for(cpu).map(|s| s.state.load(Ordering::Acquire)) {
        Some(ASKED) | Some(SAVING) => Answer::Silent,
        Some(SAVED) => Answer::Sampled,
        Some(STOPPED) => Answer::Stopped,
        _ => Answer::NotAsked,
    }
}

/// The registers and backtrace `cpu` saved in its last answer (`None` if it never answered).
pub fn regs(cpu: usize) -> Option<CpuRegs> {
    let s = SLOTS.find_for(cpu)?;
    if s.answers.load(Ordering::Acquire) == 0 || !matches!(s.state.load(Ordering::Acquire), SAVED | STOPPED) {
        return None;
    }
    let mut w = [0u64; NREGS];
    for (i, x) in w.iter_mut().enumerate() {
        *x = s.regs[i].load(Ordering::Relaxed);
    }
    let frame = from_words(&w[..22]);
    let n = s.nsteps.load(Ordering::Relaxed).min(DEPTH);
    let mut steps = [Step::None; DEPTH];
    for (i, st) in steps.iter_mut().enumerate().take(n) {
        *st = decode(s.steps[2 * i].load(Ordering::Relaxed), s.steps[2 * i + 1].load(Ordering::Relaxed));
    }
    Some(CpuRegs {
        cpu,
        apic_id: w[25] as u32,
        user: frame.cs & 3 != 0,
        frame,
        cr2: w[22],
        cr3: w[23],
        tsc: w[24],
        steps,
        nsteps: n,
    })
}

/// How many times `cpu` answered (stop or sample) since boot.
pub fn answers(cpu: usize) -> u64 {
    SLOTS.find_for(cpu).map_or(0, |s| s.answers.load(Ordering::Acquire))
}

/// Late NMIs consumed (answers to a sample that had given up waiting).
pub fn late_answers() -> u64 {
    LATE.load(Ordering::Relaxed)
}

/// Spare NMIs of ours swallowed (a CPU asked again whose earlier NMI arrived after all).
pub fn spare_nmis() -> u64 {
    SPARE.load(Ordering::Relaxed)
}

/// NMIs of ours sent to `cpu` that have not arrived (an upper bound: collapsed NMIs keep it high). A count that
/// keeps growing over samples means NMIs do not reach this CPU's hook at all.
pub fn owed(cpu: usize) -> u32 {
    SLOTS.find_for(cpu).map_or(0, |s| s.owed.load(Ordering::Acquire))
}

/// True once a crash stop ran (the other CPUs are halted or silent).
pub fn crashed() -> bool {
    PHASE.load(Ordering::Acquire) == CRASH
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decisions() {
        for owed in [false, true] {
            // A crash stops every CPU but the reporter, whatever its slot says.
            for st in [IDLE, ASKED, SAVED] {
                assert_eq!(action(CRASH, true, st, owed), Action::SaveAndStop);
            }
            // A sample answers what it asked (also when the NMI that brings the answer collapsed with another).
            assert_eq!(action(SAMPLE, true, ASKED, owed), Action::SaveAndReturn);
            // A late answer after the sample ended is swallowed, never a "kernel NMI" panic.
            assert_eq!(action(NONE, true, ASKED, owed), Action::Consume);
        }
        // The reporter's own NMI (none is sent, but one may come from elsewhere) is not ours.
        assert_eq!(action(CRASH, false, IDLE, false), Action::Chain);
        // Nothing asked and none of ours in flight: someone else's NMI (Reload's park NMI) goes on.
        for (ph, st) in [(SAMPLE, IDLE), (NONE, IDLE), (NONE, SAVED), (SAMPLE, SAVED), (SAMPLE, SAVING)] {
            assert_eq!(action(ph, true, st, false), Action::Chain, "phase {ph} state {st}");
            // One of ours still in flight (a CPU asked again whose first NMI came after all): swallowed.
            assert_eq!(action(ph, true, st, true), Action::Spare, "phase {ph} state {st}");
        }
    }

    #[test]
    fn asked_again_after_a_lost_nmi() {
        // desk fix/nmi: sample 1 asks, its NMI never reaches this hook (another hook took it). Sample 2 asks the
        // same CPU again; both NMIs then arrive: the first answers, the second is a spare, nothing panics.
        let owed = AtomicU32::new(0);
        owed.fetch_add(1, Ordering::AcqRel);
        owed.fetch_add(1, Ordering::AcqRel);
        assert_eq!(action(SAMPLE, true, ASKED, take_owed(&owed)), Action::SaveAndReturn);
        assert_eq!(action(SAMPLE, true, SAVED, take_owed(&owed)), Action::Spare);
        assert_eq!(action(NONE, true, IDLE, take_owed(&owed)), Action::Chain);
        assert_eq!(owed.load(Ordering::Acquire), 0);
    }

    #[test]
    fn steps_survive_the_slot_encoding() {
        for s in [
            Step::None,
            Step::Call(0xFFFF_FFFF_8000_1234),
            Step::Guess(0xFFFF_FFFF_8000_5678),
            Step::Trap { vector: 2, error: 0, rip: 0xFFFF_FFFF_8000_0010, user: false },
            Step::Trap { vector: 14, error: 0, rip: 0x40_0000, user: true },
        ] {
            let (k, a) = encode(s);
            assert_eq!(decode(k, a), s);
        }
    }

    #[test]
    fn frame_words_round_trip() {
        let f = TrapFrame { rip: 1, rax: 2, ss: 3, r15: 4, ..TrapFrame::default() };
        assert_eq!(words(&from_words(&words(&f))), words(&f));
    }

    #[test]
    fn nothing_before_prepare() {
        // Host: prepare is never called in this test binary.
        assert_eq!(stop_others(|| 0, 0), Err(NmiError::NotPrepared));
        assert_eq!(sample_others(|| 0, 0), Err(NmiError::NotPrepared));
        assert!(regs(0).is_none());
        assert_eq!(answer(0), Answer::NotAsked);
    }
}
