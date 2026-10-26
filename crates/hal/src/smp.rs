//! SMP bring-up (owner: CP, item CP-4; spec/core.md §3, §5.1, §6.1-6.3, §11; V1 `cpu/apstart.asm`,
//! `cpu/apmtrr.asm`, `cpu/aptsc.asm`; Linux `arch/x86/kernel/smpboot.c`).
//!
//! The BSP starts the APs one after the other (`start_ap`, driven by CP's `platform` crate):
//! 1. per AP: a GDT/TSS with three guarded IST stacks (`gdt::new_ap_tables`), a guarded boot stack in the CPU
//!    area, a GS block (`percpu::prepare_ap`), a vector table (`vectors::prepare_cpu`), the parameters in the
//!    trampoline page;
//! 2. **INIT** (0x4500, assert, no de-assert), **10 ms**, **SIPI** with the trampoline page, a **second SIPI only
//!    if no sign of life after 200 µs** (the timing V1 proved on the Mini-PC, spec §11.11);
//! 3. on the AP (`hal_ap_main`): GS, its GDT/TSS, the IDT, the **relaxed check** (only what the BSP's set-up
//!    relies on refuses the AP: `cpu::ap_missing`, the XSAVE layout, x86-64-v3), `cpu::init_this_cpu` (CR0
//!    whole, CR4, EFER, XCR0, TSC_AUX like the BSP), AMD SYSCFG fix and **MTRR compare/copy** from the BSP
//!    (QEMU clears MTRRs on INIT), PAT, SYSCALL MSRs, XSAVE (`xsave::init_this_cpu`, IA32_XSS), the LAPIC in
//!    the BSP's mode, the CPU's flags and **topology from CPUID** (`decode_topology`). Nothing on this path
//!    allocates or takes a lock, so an AP that misses its deadline can be parked with INIT at any point
//!    without leaving a lock held;
//! 4. a TSC handshake (9 rounds; the BSP stamps before and after, the AP in between) for the offset check;
//! 5. the AP says ready (deadline from the caller: 100 ms on hardware), else it is sent INIT and stays unused.
//!
//! Then each AP idles (`sti; hlt`) until SC-3 hands it a scheduler with `hand_over_aps` (the hook). Meanwhile
//! `call_on` runs a short function on another CPU from its `SMP_CALL` interrupt (tests read an AP's registers
//! that way), and `wake` kicks an idle AP. `apic_id(cpu)` is the CPU -> APIC ID map for MSI (PC-3) and IPIs;
//! `topology(cpu)` the data SC-3 places threads by (R4: no "sibling = APIC ID ^ 1", no core roles).
//!
//! Hybrid Intel CPUs (CP-6; the Gaming-PC's Core Ultra 7 265K: 8 P + 12 E cores, no SMT): every CPU reads its own
//! core type from CPUID 0x1A EAX[31:24] (0x40 P, 0x20 E; linux-6.6/arch/x86/kernel/cpu/intel.c:1366-1379, only
//! with CPUID 7.0 EDX[15] `hybrid_cpu`) and, with CPUID 6 EAX[7] `hwp` and HWP switched on by the firmware
//! (IA32_PM_ENABLE bit 0; V2 never sets it), its HWP highest performance (IA32_HWP_CAPABILITIES[7:0]). `capacity(cpu)` turns that into the hint SC weighs placement by: highest
//! performance x 1024 / the maximum over the online CPUs (what Linux's `cpu_capacity` shows on the 265K: P 1012
//! or 1024 for the two favoured cores at 87/88, E 756 at 65; Linux 6.6 uses the same numbers as ITMT priorities,
//! drivers/cpufreq/intel_pstate.c:352-372). Without HWP numbers: P/uniform 1024, E `E_CORE_FALLBACK_CAPACITY`.
//! AMD and non-hybrid Intel CPUs never read the MSR and are uniform (1024).

use crate::apic::{self, IpiDest, IpiKind, NmiPin};
use crate::cpu::{self, CpuidView, Features, FlagBits, MtrrAction, MtrrState, SetupReport, XsaveSig};
use crate::percpu::{self, PerCpu};
use alloc::boxed::Box;
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};

// ---------------------------------------------------------------------------------------------------------
// Topology as data (R4)

/// `Topology::core_type` values (CPUID 0x1A on hybrid Intel CPUs).
pub const CORE_TYPE_UNIFORM: u8 = 0;
pub const CORE_TYPE_PERFORMANCE: u8 = 1;
pub const CORE_TYPE_EFFICIENCY: u8 = 2;

/// The capacity of the fastest CPU (Linux `SCHED_CAPACITY_SCALE`).
pub const CAPACITY_SCALE: u16 = 1024;
/// E-core capacity when the CPU gives no HWP numbers: the 265K's 756 (HWP highest 65 of 88), rounded to 3/4.
pub const E_CORE_FALLBACK_CAPACITY: u16 = 768;

/// Topology of one logical CPU, decoded from its own CPUID (0x1F, else 0x0B, else the legacy leaves; AMD
/// 0x8000001E; caches from 0x8000001D / 4; core type from 0x1A).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Topology {
    /// x2APIC ID (the full 32-bit one where CPUID gives it).
    pub apic_id: u32,
    pub package_id: u32,
    /// The die (Intel 0x1F die level, AMD node from 0x8000001E); the package where neither exists. Unique
    /// across packages.
    pub die_id: u32,
    /// The physical core, unique within its package.
    pub core_id: u32,
    /// The SMT thread inside the core (0 = first).
    pub thread_id: u32,
    pub threads_per_core: u32,
    /// APIC ID of thread 0 of the same core when this is another thread of it (an SMT sibling).
    pub smt_sibling_of: Option<u32>,
    /// The last-level-cache domain (a CCX on AMD, a die's L3 on Intel): CPUs with the same id share the LLC.
    pub cluster_id: u32,
    /// NUMA node (SRAT proximity domain; 0 without SRAT).
    pub node: u32,
    /// `CORE_TYPE_*`.
    pub core_type: u8,
    /// The leaf the shifts came from: 0x1F, 0x0B, or 1 (legacy).
    pub source_leaf: u32,
    /// HWP highest performance (IA32_HWP_CAPABILITIES[7:0]) read on this CPU; 0 = not read (`hwp_highest_perf`).
    pub perf_highest: u8,
}

impl Topology {
    /// The capacity hint for SC (CP-6): this CPU's speed relative to the fastest, `CAPACITY_SCALE` = fastest.
    /// `max_perf`: the highest `perf_highest` among the online CPUs (`max_perf()`).
    pub fn capacity(&self, max_perf: u8) -> u16 {
        if self.perf_highest != 0 && max_perf != 0 {
            let c = u32::from(self.perf_highest.min(max_perf)) * u32::from(CAPACITY_SCALE) / u32::from(max_perf);
            return c.max(1) as u16;
        }
        match self.core_type {
            CORE_TYPE_EFFICIENCY => E_CORE_FALLBACK_CAPACITY,
            _ => CAPACITY_SCALE,
        }
    }

    /// A key unique per physical core in the machine.
    pub fn core_key(&self) -> (u32, u32) {
        (self.package_id, self.core_id)
    }
}

fn ceil_log2(n: u32) -> u32 {
    if n <= 1 { 0 } else { 32 - (n - 1).leading_zeros() }
}

fn low_mask(bits: u32) -> u32 {
    if bits >= 32 { u32::MAX } else { (1u32 << bits) - 1 }
}

/// Bound for walking CPUID sub-leaves (topology levels, cache levels): a guard against a CPU or hypervisor
/// that never reports the terminating level, not a count of anything.
const SUBLEAF_WALK: u32 = 64;

/// What the extended topology leaf 0x1F or 0x0B says (walked without allocating: APs decode their topology
/// before they may take the heap's lock).
struct Levels {
    x2apic_id: u32,
    smt_shift: u32,
    /// Logical CPUs at the SMT level (threads per core), 0 if not reported.
    smt_count: u32,
    /// The shift of the level below a die level, if there is one (it numbers the dies).
    die_shift: Option<u32>,
    /// The highest level's shift (it numbers the packages).
    pkg_shift: u32,
}

fn topo_levels(v: &CpuidView, leaf: u32) -> Option<Levels> {
    if v.max_basic() < leaf {
        return None;
    }
    let r0 = v.get(leaf, 0);
    if (r0.ecx >> 8) & 0xFF == 0 || r0.ebx & 0xFFFF == 0 {
        return None;
    }
    let mut l = Levels { x2apic_id: r0.edx, smt_shift: 0, smt_count: 0, die_shift: None, pkg_shift: 0 };
    let mut prev = 0;
    for n in 0..SUBLEAF_WALK {
        let r = v.get(leaf, n);
        let (ty, shift) = ((r.ecx >> 8) & 0xFF, r.eax & 0x1F);
        if ty == 0 {
            break;
        }
        match ty {
            1 => {
                l.smt_shift = shift;
                l.smt_count = r.ebx & 0xFFFF;
            }
            5 => l.die_shift = Some(prev),
            _ => {}
        }
        prev = shift;
        l.pkg_shift = shift;
    }
    Some(l)
}

/// Sharing of the last cache level (CPUID 0x8000001D on AMD with TOPOEXT, else leaf 4): the number of
/// logical CPUs that share it, as the leaf reports it.
fn llc_sharing(v: &CpuidView, amd_topoext: bool) -> Option<u32> {
    let leaf = if amd_topoext { 0x8000_001D } else { 4 };
    if (leaf == 4 && v.max_basic() < 4) || (leaf != 4 && v.max_ext() < leaf) {
        return None;
    }
    let mut best: Option<(u32, u32)> = None;
    for n in 0..SUBLEAF_WALK {
        let r = v.get(leaf, n);
        if r.eax & 0x1F == 0 {
            break;
        }
        let level = (r.eax >> 5) & 7;
        let sharing = ((r.eax >> 14) & 0xFFF) + 1;
        if best.is_none_or(|(l, _)| level > l) {
            best = Some((level, sharing));
        }
    }
    best.map(|(_, s)| s)
}

/// Decodes the topology of the CPU whose CPUID `v` shows (pure; host-tested with recorded leaves).
pub fn decode_topology(v: &CpuidView, node: u32) -> Topology {
    let vendor = v.get(0, 0);
    let amd = vendor.ebx == u32::from_le_bytes(*b"Auth");
    let topoext = amd && v.has("topoext") && v.max_ext() >= 0x8000_001E;
    let legacy_id = v.get(1, 0).ebx >> 24;
    let levels = topo_levels(v, 0x1F).map(|l| (0x1F, l)).or_else(|| topo_levels(v, 0xB).map(|l| (0xB, l)));
    let (mut apic, smt_shift, pkg_shift, die_shift, mut tpc, source) = match levels {
        Some((leaf, l)) => (l.x2apic_id, l.smt_shift, l.pkg_shift, l.die_shift, l.smt_count, leaf),
        None => {
            // Legacy: leaf 1 EBX[23:16] logical CPUs per package (with HTT), threads per core from AMD's
            // 0x8000001E or Intel's leaf 4 core count.
            let logical = if v.has("ht") { (v.get(1, 0).ebx >> 16) & 0xFF } else { 1 }.max(1);
            let amd_size = if amd && v.max_ext() >= 0x8000_0008 { (v.get(0x8000_0008, 0).ecx >> 12) & 0xF } else { 0 };
            let pkg_shift = if amd_size != 0 { amd_size } else { ceil_log2(logical) };
            let threads = if topoext {
                ((v.get(0x8000_001E, 0).ebx >> 8) & 0xFF) + 1
            } else if v.max_basic() >= 4 && v.get(4, 0).eax & 0x1F != 0 {
                (logical / (((v.get(4, 0).eax >> 26) & 0x3F) + 1)).max(1)
            } else {
                1
            };
            (legacy_id, ceil_log2(threads).min(pkg_shift), pkg_shift, None, threads, 1)
        }
    };
    if topoext {
        let e = v.get(0x8000_001E, 0);
        if source == 1 {
            apic = e.eax;
        }
        let amd_tpc = ((e.ebx >> 8) & 0xFF) + 1;
        if tpc == 0 {
            tpc = amd_tpc;
        }
    }
    if tpc == 0 {
        tpc = 1 << smt_shift;
    }
    let package_id = if pkg_shift >= 32 { 0 } else { apic >> pkg_shift };
    let thread_id = apic & low_mask(smt_shift);
    let core_id = (apic & low_mask(pkg_shift)) >> smt_shift.min(31);
    let die_id = match die_shift {
        Some(s) if s < 32 => apic >> s,
        // AMD: the node of 0x8000001E is the die (one per package on the Mini-PC).
        _ if topoext => (package_id << 8) | (v.get(0x8000_001E, 0).ecx & 0xFF),
        _ => package_id,
    };
    let cluster_id = match llc_sharing(v, topoext) {
        Some(s) => {
            let shift = ceil_log2(s);
            if shift >= 32 { 0 } else { apic >> shift }
        }
        None => die_id,
    };
    let core_type = if v.has("hybrid_cpu") && v.max_basic() >= 0x1A {
        match v.get(0x1A, 0).eax >> 24 {
            0x40 => CORE_TYPE_PERFORMANCE,
            0x20 => CORE_TYPE_EFFICIENCY,
            _ => CORE_TYPE_UNIFORM,
        }
    } else {
        CORE_TYPE_UNIFORM
    };
    Topology {
        apic_id: apic,
        package_id,
        die_id,
        core_id,
        thread_id,
        threads_per_core: tpc,
        smt_sibling_of: (thread_id != 0).then_some(apic & !low_mask(smt_shift)),
        cluster_id,
        node,
        core_type,
        source_leaf: source,
        // Read by `this_cpu_topology` on the CPU itself (an MSR; this function is pure).
        perf_highest: 0,
    }
}

/// HWP highest performance of the CPU `v` describes, read with `read` (pure; host-tested). Only hybrid Intel CPUs
/// with CPUID `hwp` and HWP switched on (IA32_PM_ENABLE bit 0) are asked: the MSR #GPs elsewhere and `msr::read`
/// cannot recover; uniform CPUs need no hint. V2 never switches HWP on itself (that is a power-management choice,
/// not a topology read), so with the firmware's PM_ENABLE clear the core-type fallback applies (0 here).
pub fn hwp_highest_perf(v: &CpuidView, read: &mut dyn FnMut(u32) -> u64) -> u8 {
    if v.has("hybrid_cpu") && crate::msr::hwp_enabled(v, read) {
        (read(crate::msr::IA32_HWP_CAPABILITIES) & 0xFF) as u8
    } else {
        0
    }
}

/// The topology of the CPU running this, with its HWP number (each CPU reads its own; no lock, no allocation).
fn this_cpu_topology(v: &CpuidView, node: u32) -> Topology {
    Topology { perf_highest: hwp_highest_perf(v, &mut crate::msr::read), ..decode_topology(v, node) }
}

// ---------------------------------------------------------------------------------------------------------
// Per-CPU slots: what other lanes read (APIC ID, topology, boot stack) and the call mailbox.

const OFFLINE: u8 = 0;
const ONLINE: u8 = 1;

// The call mailbox. `call_owner` belongs to one caller from its claim until it has taken the result (or, after
// `Late`, to the target, which frees it when the abandoned call ends), so no caller can reclaim, collect or
// withdraw another caller's request.
const CALL_IDLE: u8 = 0;
const CALL_REQUESTED: u8 = 1;
const CALL_RUNNING: u8 = 2;
const CALL_DONE: u8 = 3;
/// The caller gave up while the target ran the call (`Late`): the target frees the mailbox when it ends.
const CALL_ABANDONED: u8 = 4;

struct Slot {
    state: AtomicU8,
    apic_id: AtomicU32,
    topo_set: AtomicBool,
    topo: UnsafeCell<Topology>,
    stack: [AtomicU64; 3],
    call_owner: AtomicBool,
    call_state: AtomicU8,
    call_fn: AtomicUsize,
    call_arg: AtomicU64,
    call_result: AtomicU64,
    calls: AtomicU64,
    wakes: AtomicU64,
}

// SAFETY: `topo` is written once, under the start lock, before `topo_set` is published (Release) and only read
// after it (Acquire); everything else is atomic.
unsafe impl Sync for Slot {}

static SLOTS: PerCpu<Slot> = PerCpu::new(|_| Slot {
    state: AtomicU8::new(OFFLINE),
    apic_id: AtomicU32::new(u32::MAX),
    topo_set: AtomicBool::new(false),
    topo: UnsafeCell::new(Topology { apic_id: 0, package_id: 0, die_id: 0, core_id: 0, thread_id: 0, threads_per_core: 0, smt_sibling_of: None, cluster_id: 0, node: 0, core_type: 0, source_leaf: 0, perf_highest: 0 }),
    stack: [const { AtomicU64::new(0) }; 3],
    call_owner: AtomicBool::new(false),
    call_state: AtomicU8::new(CALL_IDLE),
    call_fn: AtomicUsize::new(0),
    call_arg: AtomicU64::new(0),
    call_result: AtomicU64::new(0),
    calls: AtomicU64::new(0),
    wakes: AtomicU64::new(0),
});

fn slot(cpu: usize) -> Option<&'static Slot> {
    SLOTS.get_for(cpu)
}

fn set_topology(s: &Slot, t: Topology) {
    if s.topo_set.load(Ordering::Acquire) {
        return;
    }
    // SAFETY: callers hold the start lock (`prepare`, `record_bsp`, `start_ap`), so there is one writer, once,
    // before `topo_set` is published (see Slot's Sync impl).
    unsafe { *s.topo.get() = t };
    MAX_PERF.fetch_max(t.perf_highest, Ordering::AcqRel);
    s.topo_set.store(true, Ordering::Release);
}

/// The highest `perf_highest` recorded so far (the scale `capacity` divides by).
static MAX_PERF: AtomicU8 = AtomicU8::new(0);

/// The highest HWP performance among the CPUs brought up so far (0: none read one).
pub fn max_perf() -> u8 {
    MAX_PERF.load(Ordering::Acquire)
}

/// The capacity hint of online `cpu` (`Topology::capacity`; allocation-free, also from interrupt handlers). It
/// can drop for earlier CPUs while later, faster ones come up; after the bring-up (the `smp.shape` event) it
/// stays. Consumers read it per decision instead of caching it during the bring-up.
pub fn capacity(cpu: usize) -> Option<u16> {
    topology(cpu).map(|t| t.capacity(max_perf()))
}

/// The APIC ID of `cpu` (online CPUs only): the destination for its IPIs and MSIs.
pub fn apic_id(cpu: usize) -> Option<u32> {
    let s = slot(cpu)?;
    (s.state.load(Ordering::Acquire) == ONLINE).then(|| s.apic_id.load(Ordering::Relaxed))
}

/// The CPU number of an APIC ID (online CPUs only).
pub fn cpu_of_apic(apic: u32) -> Option<usize> {
    (0..percpu::cpu_count()).find(|&c| apic_id(c) == Some(apic))
}

/// This CPU's number found from its APIC ID, without GS (for `percpu::paranoid_gs_enter`): 0 before `prepare`
/// (only the BSP runs then), `None` for a CPU that is not online yet. No lock, no allocation (`prepare` sized
/// the slots for every possible CPU).
pub(crate) fn cpu_by_apic_id() -> Option<usize> {
    if !PREPARED.load(Ordering::Acquire) {
        return Some(0);
    }
    cpu_of_apic(apic::id()?)
}

/// Whether `cpu` runs kernel code.
pub fn is_online(cpu: usize) -> bool {
    slot(cpu).is_some_and(|s| s.state.load(Ordering::Acquire) == ONLINE)
}

/// The topology of `cpu` as its own CPUID reported it (online CPUs, after `prepare`).
pub fn topology(cpu: usize) -> Option<Topology> {
    let s = slot(cpu)?;
    // SAFETY: read only after `topo_set` was published (see Slot).
    (s.state.load(Ordering::Acquire) == ONLINE && s.topo_set.load(Ordering::Acquire)).then(|| unsafe { *s.topo.get() })
}

/// The boot stack `cpu`'s first context runs on: (guard, bottom, top); the BSP's is the kernel's boot stack
/// (zeros: not known to hal). SC-3 adopts it as the AP's idle context.
pub fn boot_stack(cpu: usize) -> Option<(u64, u64, u64)> {
    let s = slot(cpu)?;
    Some((s.stack[0].load(Ordering::Relaxed), s.stack[1].load(Ordering::Relaxed), s.stack[2].load(Ordering::Relaxed)))
}

/// `call_on` deliveries and wake-ups `cpu` took (tests: every IPI arrived).
pub fn ipi_counts(cpu: usize) -> Option<(u64, u64)> {
    slot(cpu).map(|s| (s.calls.load(Ordering::Relaxed), s.wakes.load(Ordering::Relaxed)))
}

// ---------------------------------------------------------------------------------------------------------
// Cross-CPU calls, wake-ups and the scheduler hook (SC-3)

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallError {
    NotOnline,
    /// Another caller's request to this CPU is still open (or a `Late` call still runs there).
    Busy,
    Ipi(apic::IpiError),
    /// The target did not take the call in time (interrupts off there?); the request was withdrawn.
    Timeout,
    /// The target took the call but did not finish in time; its result is dropped when it does, and the target
    /// stays `Busy` for other callers until then.
    Late,
}

/// The `SMP_CALL` handler (hard-IRQ context on the target): runs a requested call, counts a wake-up otherwise.
fn smp_call_irq(_v: u8) {
    let Some(s) = slot(percpu::this_cpu()) else { return };
    if s.call_state.compare_exchange(CALL_REQUESTED, CALL_RUNNING, Ordering::AcqRel, Ordering::Acquire).is_ok() {
        let f = s.call_fn.load(Ordering::Acquire);
        // SAFETY: `call_on` stores only `fn(u64) -> u64` pointers here before publishing CALL_REQUESTED.
        let f: fn(u64) -> u64 = unsafe { core::mem::transmute::<usize, fn(u64) -> u64>(f) };
        let r = f(s.call_arg.load(Ordering::Relaxed));
        s.call_result.store(r, Ordering::Relaxed);
        s.calls.fetch_add(1, Ordering::Relaxed);
        if s.call_state.compare_exchange(CALL_RUNNING, CALL_DONE, Ordering::AcqRel, Ordering::Acquire).is_err() {
            // ABANDONED: the caller returned `Late` and left the mailbox to this CPU.
            s.call_state.store(CALL_IDLE, Ordering::Release);
            s.call_owner.store(false, Ordering::Release);
        }
    } else {
        s.wakes.fetch_add(1, Ordering::Relaxed);
    }
}

/// Runs `f(arg)` on `cpu` from its `SMP_CALL` interrupt and returns the result (on this CPU: directly, with
/// interrupts off). `f` runs in hard-IRQ context: no heap, no klog, no locks a thread holds with interrupts on.
/// Waits at most `timeout_ns` by `clock` (B4: never unbounded); the target must have interrupts on at some point.
pub fn call_on(cpu: usize, f: fn(u64) -> u64, arg: u64, clock: fn() -> u64, timeout_ns: u64) -> Result<u64, CallError> {
    {
        // Interrupts off before the CPU is read: nothing can move this thread between the check and `f`.
        let _irq = cpu::IrqGuard::new();
        if cpu == percpu::this_cpu() {
            return Ok(f(arg));
        }
    }
    let s = slot(cpu).filter(|s| s.state.load(Ordering::Acquire) == ONLINE).ok_or(CallError::NotOnline)?;
    if s.call_owner.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_err() {
        return Err(CallError::Busy);
    }
    // This caller owns the mailbox, which is IDLE now (every owner leaves it IDLE when it frees it).
    let release = |s: &Slot| {
        s.call_state.store(CALL_IDLE, Ordering::Release);
        s.call_owner.store(false, Ordering::Release);
    };
    s.call_fn.store(f as usize, Ordering::Relaxed);
    s.call_arg.store(arg, Ordering::Relaxed);
    s.call_state.store(CALL_REQUESTED, Ordering::Release);
    if let Err(e) = apic::send_ipi(IpiDest::Physical(s.apic_id.load(Ordering::Relaxed)), IpiKind::Fixed(crate::vectors::SMP_CALL)) {
        if s.call_state.compare_exchange(CALL_REQUESTED, CALL_IDLE, Ordering::AcqRel, Ordering::Acquire).is_ok() {
            s.call_owner.store(false, Ordering::Release);
            return Err(CallError::Ipi(e));
        }
        // Another CPU's wake-up made the target take it anyway: wait for it as usual.
    }
    let deadline = clock().saturating_add(timeout_ns);
    let mut spins = 0u32;
    loop {
        if s.call_state.load(Ordering::Acquire) == CALL_DONE {
            let r = s.call_result.load(Ordering::Relaxed);
            release(s);
            return Ok(r);
        }
        spins = spins.wrapping_add(1);
        if spins.is_multiple_of(64) && clock() > deadline {
            if s.call_state.compare_exchange(CALL_REQUESTED, CALL_IDLE, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                s.call_owner.store(false, Ordering::Release);
                return Err(CallError::Timeout);
            }
            if s.call_state.compare_exchange(CALL_RUNNING, CALL_ABANDONED, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                // The target frees the mailbox when the call ends.
                return Err(CallError::Late);
            }
            // DONE in the meantime: the result is this caller's.
            let r = s.call_result.load(Ordering::Acquire);
            release(s);
            return Ok(r);
        }
        core::hint::spin_loop();
    }
}

/// Kicks `cpu` out of `hlt` (an `SMP_CALL` IPI without a request).
pub fn wake(cpu: usize) -> Result<(), CallError> {
    let s = slot(cpu).filter(|s| s.state.load(Ordering::Acquire) == ONLINE).ok_or(CallError::NotOnline)?;
    apic::send_ipi(IpiDest::Physical(s.apic_id.load(Ordering::Relaxed)), IpiKind::Fixed(crate::vectors::SMP_CALL)).map_err(CallError::Ipi)
}

static HANDOFF: AtomicUsize = AtomicUsize::new(0);

/// SC-3's hook: every idle AP (now and every AP started later) calls `entry(cpu)` on its boot stack
/// (`boot_stack(cpu)`) with interrupts off and never comes back to CP's idle loop. The first call wins; returns
/// the number of APs woken, or `None` when a hook was set already.
pub fn hand_over_aps(entry: fn(usize) -> !) -> Option<usize> {
    if HANDOFF.compare_exchange(0, entry as usize, Ordering::AcqRel, Ordering::Acquire).is_err() {
        return None;
    }
    // Pairs with the fence in `start_ap` after an AP is marked online: either this loop sees it ONLINE, or
    // `start_ap` sees HANDOFF and wakes it itself (an AP is never left halted with the hook set).
    core::sync::atomic::fence(Ordering::SeqCst);
    Some((1..percpu::cpu_count()).filter(|&c| wake(c).is_ok()).count())
}

/// Whether SC-3 took the APs over.
pub fn handed_over() -> bool {
    HANDOFF.load(Ordering::Acquire) != 0
}

/// An AP's life after its start: interrupts on and halted, until `hand_over_aps` gives it a scheduler.
#[cfg_attr(not(target_os = "none"), allow(dead_code))]
fn idle_loop(cpu: usize) -> ! {
    loop {
        cpu::disable_interrupts();
        let h = HANDOFF.load(Ordering::Acquire);
        if h != 0 {
            // SAFETY: HANDOFF holds 0 or the `fn(usize) -> !` stored by `hand_over_aps`.
            let f: fn(usize) -> ! = unsafe { core::mem::transmute::<usize, fn(usize) -> !>(h) };
            f(cpu);
        }
        cpu::enable_and_halt();
    }
}

// ---------------------------------------------------------------------------------------------------------
// Preparation (once) and the start of one AP

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SmpError {
    NotPrepared,
    AlreadyPrepared,
    Trampoline(crate::trampoline::TrampolineError),
    /// The BSP's LAPIC is not set up (CP-3's `apic::init_bsp` first).
    NoLapic,
    /// The system vector `SMP_CALL` has another owner.
    Vector(crate::vectors::VectorError),
    /// `start_ap` for a CPU number other than the next free one (APs are numbered densely).
    WrongIndex,
    /// `start_ap` for the APIC ID of this CPU or of a CPU that is online: INIT would reset a running CPU.
    AlreadyOnline,
    /// Another `prepare` or `start_ap` runs (they share the trampoline page and the slots).
    Busy,
    /// No frame or virtual space for the AP's stacks or tables.
    NoMemory(&'static str),
    Host,
}

/// What `prepare` found on the BSP.
#[derive(Clone, Debug)]
pub struct Prepared {
    pub trampoline_page: u64,
    pub low_pml4: u64,
    pub code_len: usize,
    /// The BSP's memory types every AP is compared with (`None` without CPUID MTRR).
    pub mtrr: Option<MtrrState>,
    /// SYSCFG before/after clearing MtrrFixDramModEn on the BSP (AMD, when it was set).
    pub syscfg_fix: Option<(u64, u64)>,
    pub bsp_apic_id: u32,
    pub bsp_topology: Topology,
    /// IA32_TSC_ADJUST on the BSP (CPUID `tsc_adjust` only).
    pub bsp_tsc_adjust: Option<u64>,
}

struct Shared {
    page: u64,
    low_pml4: u64,
    features: Features,
    xsave: XsaveSig,
    xsave_size: u32,
    mtrr: Option<MtrrState>,
    efer_or: u64,
    cr4: u64,
}

static PREPARED: AtomicBool = AtomicBool::new(false);
static SHARED: AtomicUsize = AtomicUsize::new(0);

/// One `prepare` or `start_ap` at a time: the trampoline page, PREPARED's check-then-set and a starting AP's slot
/// each have this one writer, as their SAFETY comments rely on.
static START_BUSY: AtomicBool = AtomicBool::new(false);

struct StartLock;

impl StartLock {
    fn take() -> Result<StartLock, SmpError> {
        START_BUSY.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).map(|_| StartLock).map_err(|_| SmpError::Busy)
    }
}

impl Drop for StartLock {
    fn drop(&mut self) {
        START_BUSY.store(false, Ordering::Release);
    }
}

fn shared() -> Option<&'static Shared> {
    let p = SHARED.load(Ordering::Acquire);
    // SAFETY: SHARED holds 0 or a pointer from `Box::leak` in `prepare`, never freed or changed.
    (p != 0).then(|| unsafe { &*(p as *const Shared) })
}

/// Once, on the BSP after CP-3's LAPIC set-up and before the first `start_ap`: installs the trampoline in the
/// KEEP page and builds the low PML4 (identity for the page, the kernel half shared), clears AMD's
/// MtrrFixDramModEn and snapshots the BSP's MTRRs, records the BSP's APIC ID and topology (`node` from SRAT),
/// registers the `SMP_CALL` vector and sizes the per-CPU slots for `possible` CPUs.
pub fn prepare(t: bootinfo::Trampoline, node: u32, alloc_table: &mut dyn FnMut() -> Option<u64>) -> Result<Prepared, SmpError> {
    if !crate::BARE_METAL {
        return Err(SmpError::Host);
    }
    let _lock = StartLock::take()?;
    if PREPARED.load(Ordering::Acquire) {
        return Err(SmpError::AlreadyPrepared);
    }
    let bsp_apic = apic::id().ok_or(SmpError::NoLapic)?;
    crate::trampoline::check_pages(t.page_phys, t.low_pml4_phys).map_err(SmpError::Trampoline)?;
    crate::trampoline::install(t.page_phys).map_err(SmpError::Trampoline)?;
    let root = kernel_root();
    crate::trampoline::build_low_pml4(t.low_pml4_phys, t.page_phys, root, alloc_table).map_err(SmpError::Trampoline)?;
    match crate::vectors::register_system(crate::vectors::SMP_CALL, smp_call_irq) {
        Ok(()) => {}
        Err(e) => return Err(SmpError::Vector(e)),
    }
    let syscfg_fix = cpu::amd_syscfg_fix();
    let mtrr = cpu::mtrr_state();
    let v = cpu::live();
    let features = Features::from_view(&v);
    let efer = crate::msr::read(crate::msr::IA32_EFER);
    let bsp_tsc_adjust = v.has("tsc_adjust").then(|| crate::msr::read(crate::msr::IA32_TSC_ADJUST));
    let sh = Shared {
        page: t.page_phys,
        low_pml4: t.low_pml4_phys,
        features,
        xsave: XsaveSig::from_view(&v),
        xsave_size: crate::xsave::layout().size,
        mtrr: mtrr.clone(),
        efer_or: crate::msr::EFER_LME | (efer & crate::msr::EFER_NXE),
        cr4: cpu::read_cr4(),
    };
    SHARED.store(Box::leak(Box::new(sh)) as *const Shared as usize, Ordering::Release);
    let topo = this_cpu_topology(&v, node);
    mark_bsp_online(bsp_apic, topo);
    PREPARED.store(true, Ordering::Release);
    Ok(Prepared {
        trampoline_page: t.page_phys,
        low_pml4: t.low_pml4_phys,
        code_len: crate::trampoline::code_len(),
        mtrr,
        syscfg_fix,
        bsp_apic_id: bsp_apic,
        bsp_topology: topo,
        bsp_tsc_adjust,
    })
}

/// Records the BSP as online CPU 0 (its APIC ID, its topology with `node` from SRAT) without preparing any AP
/// start (CP-4b): for the one-CPU paths (CONFIG `smp=off`/`cpus=1`, no trampoline, no MADT), so `apic_id(0)`,
/// `topology(0)` and `cpu_of_apic` hold with one CPU as well. `prepare` records the same; a second record keeps
/// the first topology. Touches no trampoline or vector; reads only IA32_PM_ENABLE and IA32_HWP_CAPABILITIES, and
/// only on hybrid Intel CPUs with CPUID `hwp` (`hwp_highest_perf`).
pub fn record_bsp(node: u32) -> Result<Topology, SmpError> {
    if !crate::BARE_METAL {
        return Err(SmpError::Host);
    }
    let _lock = StartLock::take()?;
    let bsp_apic = apic::id().ok_or(SmpError::NoLapic)?;
    let topo = this_cpu_topology(&cpu::live(), node);
    mark_bsp_online(bsp_apic, topo);
    Ok(topo)
}

/// Slot 0 = the BSP, online (callers hold the start lock).
fn mark_bsp_online(bsp_apic: u32, topo: Topology) {
    // Every possible CPU gets its slot now, so the IRQ-context lookups never allocate.
    let _ = SLOTS.get_for(percpu::possible_cpus().saturating_sub(1));
    if let Some(s) = slot(0) {
        s.apic_id.store(bsp_apic, Ordering::Relaxed);
        set_topology(s, topo);
        s.state.store(ONLINE, Ordering::Release);
    }
}

fn kernel_root() -> u64 {
    match crate::switch::kernel_root() {
        0 => crate::paging::current_root(),
        r => r,
    }
}

/// One AP to start.
#[derive(Clone, Copy, Debug)]
pub struct ApRequest {
    /// The CPU number it gets: must be `percpu::cpu_count()` (dense numbering).
    pub cpu: u32,
    pub apic_id: u32,
    /// NUMA node (SRAT), for TSC_AUX and the topology.
    pub node: u32,
    /// The LINT pin the MADT names for this CPU's NMI.
    pub nmi: NmiPin,
    /// From INIT to "ready" at most (100 ms on hardware, spec §6.1; more under a hypervisor).
    pub ready_ns: u64,
}

/// Where an AP got to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApStage {
    /// No sign of life after the SIPIs.
    Silent,
    /// The real-mode code ran (`alive` 1) but never reached 64-bit code.
    RealMode,
    /// It left the trampoline page but never reached the kernel.
    LeftTrampoline,
    InKernel,
    Refused,
    Online,
    Ready,
}

const ST_NONE: u8 = 0;
const ST_IN_KERNEL: u8 = 1;
const ST_REFUSED: u8 = 2;
const ST_ONLINE: u8 = 3;
const ST_READY: u8 = 4;

/// TSC handshake: rounds, and how long the BSP waits for one answer (V1 `aptsc.asm`).
pub const TSC_ROUNDS: u32 = 9;
const TSC_ROUND_NS: u64 = 50_000_000;
const TSC_DONE: u32 = u32::MAX;
/// INIT to SIPI (spec §11.11), and how long a first SIPI may go unanswered before the second.
pub const INIT_DELAY_NS: u64 = 10_000_000;
pub const SIPI_ANSWER_NS: u64 = 200_000;
/// Boot stack of an AP: 64 KiB above one guard page (like a kernel thread's).
pub const AP_STACK_PAGES: u64 = 16;

#[derive(Clone, Debug, Default)]
struct ApSide {
    refused: Option<&'static str>,
    setup: SetupReport,
    mtrr: MtrrOutcome,
    syscfg_fix: Option<(u64, u64)>,
    lapic: apic::ApLapic,
    xsave_size: u32,
    flags: FlagBits,
    topology: Topology,
    tsc_adjust: Option<u64>,
    syscfg_differs: Option<(u64, u64)>,
}

/// What happened to an AP's MTRRs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MtrrOutcome {
    /// No CPUID MTRR.
    #[default]
    Absent,
    Same,
    /// Copied from the BSP (QEMU after INIT); `syscfg`: AMD SYSCFG memory-type bits too; `verified`: read back
    /// equal.
    Copied { syscfg: bool, verified: bool },
}

/// The block one AP starts with; leaked (an AP that misses its deadline may still read it until INIT lands).
#[cfg_attr(not(target_os = "none"), allow(dead_code))]
struct ApBoot {
    cpu: u32,
    node: u32,
    nmi: NmiPin,
    local: u64,
    gdt: &'static crate::gdt::CpuGdt,
    shared: &'static Shared,
    clock: fn() -> u64,
    quiet_ns: u64,
    stage: AtomicU8,
    /// Set once the AP loaded the kernel's IDT (from then on an NMI no longer triple-faults it).
    idt_loaded: AtomicBool,
    /// The AP's last step (`STEPS` index): what a deadline failure reports.
    step: AtomicU8,
    tsc_ask: AtomicU32,
    tsc_told: AtomicU32,
    tsc_value: AtomicU64,
    side: UnsafeCell<ApSide>,
}

// SAFETY: `side` is written only by the AP before it publishes ST_ONLINE or ST_REFUSED (Release) and read by the
// BSP only after it saw one of them (Acquire); the rest is immutable or atomic.
unsafe impl Sync for ApBoot {}

/// An AP that came up.
#[derive(Clone, Debug)]
pub struct ApStarted {
    pub cpu: u32,
    pub apic_id: u32,
    pub sipis: u32,
    /// From INIT: first sign of life, kernel set-up done, ready.
    pub ns_alive: u64,
    pub ns_online: u64,
    pub ns_ready: u64,
    pub setup: SetupReport,
    pub mtrr: MtrrOutcome,
    pub syscfg_fix: Option<(u64, u64)>,
    pub lapic: apic::ApLapic,
    pub xsave_size: u32,
    /// The AP's CPUID flags (compared with the BSP's by the caller: `FlagBits::differences`).
    pub flags: FlagBits,
    pub topology: Topology,
    /// IA32_TSC_ADJUST on the AP (CPUID `tsc_adjust` only; Linux `tsc_sync.c` checks it at boot).
    pub tsc_adjust: Option<u64>,
    /// AMD: (BSP's, AP's) SYSCFG when they differ outside the memory-type bits and MtrrFixDramModEn (kept, logged).
    pub syscfg_differs: Option<(u64, u64)>,
    /// (BSP before, AP, BSP after) TSC per round.
    pub tsc_samples: Vec<(u64, u64, u64)>,
    pub ist: [crate::gdt::IstStack; 3],
    pub stack: (u64, u64, u64),
}

/// The steps of the AP's kernel set-up, for the failure report.
pub const STEPS: [&str; 15] = [
    "entry", "tables", "check", "cpu_init", "mtrr_read", "pat", "syscall", "xsave", "lapic", "flags", "tsc", "idle", "syscfg",
    "mtrr_load", "mtrr_verify",
];

/// An AP that did not come up (it was sent INIT and stays unused).
#[derive(Clone, Copy, Debug)]
pub struct StartError {
    pub cpu: u32,
    pub apic_id: u32,
    pub stage: ApStage,
    /// The AP's last step in the kernel (`STEPS`), "entry" if it never got there.
    pub step: &'static str,
    pub reason: &'static str,
    pub sipis: u32,
    pub elapsed_ns: u64,
}

/// Frame sources and the clock for `start_ap`.
pub struct Env<'a> {
    pub alloc_stack_page: &'a mut dyn FnMut() -> Option<u64>,
    pub free_stack_page: &'a mut dyn FnMut(u64),
    pub alloc_table: &'a mut dyn FnMut() -> Option<u64>,
    /// Gives a stack frame back (error paths of the boot stack's mapping).
    pub free_frame: fn(u64),
    pub clock: fn() -> u64,
}

fn no_release(_base: u64) {}

fn wait_until(clock: fn() -> u64, deadline: u64, mut done: impl FnMut() -> bool) -> bool {
    let mut spins = 0u32;
    loop {
        if done() {
            return true;
        }
        spins = spins.wrapping_add(1);
        if spins.is_multiple_of(16) && clock() >= deadline {
            return done();
        }
        core::hint::spin_loop();
    }
}

/// Starts one AP (BSP, interrupts may be on; the boot thread). `Err(SmpError)` = nothing was sent; `Ok(Err(..))`
/// = the AP was tried and did not come up (sent INIT, unused, its resources leaked).
pub fn start_ap(req: &ApRequest, env: &mut Env<'_>) -> Result<Result<ApStarted, StartError>, SmpError> {
    let sh = shared().ok_or(SmpError::NotPrepared)?;
    let _lock = StartLock::take()?;
    if req.cpu as usize != percpu::cpu_count() || req.cpu == 0 {
        return Err(SmpError::WrongIndex);
    }
    if apic::id() == Some(req.apic_id) || cpu_of_apic(req.apic_id).is_some() {
        return Err(SmpError::AlreadyOnline);
    }
    let cpu = req.cpu as usize;
    let gdt = crate::gdt::new_ap_tables(env.alloc_stack_page, env.free_stack_page, env.alloc_table).map_err(|_| SmpError::NoMemory("IST stacks"))?;
    let len = (1 + AP_STACK_PAGES) * 4096;
    let base = crate::gdt::cpu_area_alloc(len).ok_or(SmpError::NoMemory("CPU area for the boot stack"))?;
    let stack = crate::switch::KernelStack::map(base, 1, AP_STACK_PAGES, env.alloc_stack_page, env.alloc_table, env.free_frame, no_release)
        .map_err(|_| SmpError::NoMemory("boot stack"))?;
    let sb = stack.bounds();
    // The stack lives as long as the AP (forever); its bounds go to the slot for SC-3.
    core::mem::forget(stack);
    let local = percpu::prepare_ap(cpu, gdt);
    crate::vectors::prepare_cpu(cpu);
    let s = slot(cpu).ok_or(SmpError::WrongIndex)?;
    s.stack[0].store(sb.guard, Ordering::Relaxed);
    s.stack[1].store(sb.bottom, Ordering::Relaxed);
    s.stack[2].store(sb.top, Ordering::Relaxed);
    let hyper = sh.features.hypervisor;
    let boot: &'static ApBoot = Box::leak(Box::new(ApBoot {
        cpu: req.cpu,
        node: req.node,
        nmi: req.nmi,
        local,
        gdt,
        shared: sh,
        clock: env.clock,
        quiet_ns: if hyper { 5_000_000_000 } else { 1_000_000_000 },
        stage: AtomicU8::new(ST_NONE),
        idt_loaded: AtomicBool::new(false),
        step: AtomicU8::new(0),
        tsc_ask: AtomicU32::new(0),
        tsc_told: AtomicU32::new(0),
        tsc_value: AtomicU64::new(0),
        side: UnsafeCell::new(ApSide::default()),
    }));
    crate::trampoline::set_params(
        sh.page,
        &crate::trampoline::Params {
            pml4: sh.low_pml4,
            efer_or: sh.efer_or,
            cr0: cpu::CR0_WANTED,
            stack: sb.top,
            arg: boot as *const ApBoot as u64,
            cr3: kernel_root(),
            entry: crate::trampoline::entry(),
        },
    );
    let clock = env.clock;
    // No broadcast NMI until the AP has its IDT (or is parked): one would triple-fault it.
    let mut nmi_block = Some(apic::block_nmi_broadcast());
    let t0 = clock();
    let fail = |stage: ApStage, reason: &'static str, sipis: u32| -> Result<Result<ApStarted, StartError>, SmpError> {
        // Parks the AP whatever it does (INIT, no SIPI follows).
        let _ = apic::send_init_sipi(req.apic_id, IpiKind::Init);
        if matches!(stage, ApStage::Silent | ApStage::RealMode | ApStage::LeftTrampoline) {
            // It may still be in the trampoline (or take a late SIPI): x2APIC gives no delivery status, so give the
            // INIT time to land before the next start rewrites the page's parameters (V1 waited the same between
            // INIT and its next step).
            wait_until(clock, clock().saturating_add(INIT_DELAY_NS), || false);
        }
        let step = STEPS.get(boot.step.load(Ordering::Acquire) as usize).copied().unwrap_or("?");
        Ok(Err(StartError { cpu: req.cpu, apic_id: req.apic_id, stage, step, reason, sipis, elapsed_ns: clock().saturating_sub(t0) }))
    };
    if apic::send_init_sipi(req.apic_id, IpiKind::Init).is_err() {
        return fail(ApStage::Silent, "INIT IPI could not be sent", 0);
    }
    wait_until(clock, t0 + INIT_DELAY_NS, || false);
    let vector = crate::trampoline::sipi_vector(sh.page);
    let alive = || crate::trampoline::alive(sh.page) != 0 || boot.stage.load(Ordering::Acquire) != ST_NONE;
    let mut sipis = 1;
    if apic::send_init_sipi(req.apic_id, IpiKind::Startup(vector)).is_err() {
        return fail(ApStage::Silent, "SIPI could not be sent", 0);
    }
    if !wait_until(clock, clock() + SIPI_ANSWER_NS, alive) {
        sipis = 2;
        if apic::send_init_sipi(req.apic_id, IpiKind::Startup(vector)).is_err() {
            return fail(ApStage::Silent, "second SIPI could not be sent", 1);
        }
    }
    let deadline = t0 + req.ready_ns;
    let stage_now = || match boot.stage.load(Ordering::Acquire) {
        ST_NONE => match crate::trampoline::alive(sh.page) {
            0 => ApStage::Silent,
            crate::trampoline::ALIVE_REAL_MODE => ApStage::RealMode,
            _ => ApStage::LeftTrampoline,
        },
        ST_IN_KERNEL => ApStage::InKernel,
        ST_REFUSED => ApStage::Refused,
        ST_ONLINE => ApStage::Online,
        _ => ApStage::Ready,
    };
    let mut ns_alive = 0;
    let ok = wait_until(clock, deadline, || {
        if ns_alive == 0 && alive() {
            ns_alive = clock().saturating_sub(t0);
        }
        matches!(boot.stage.load(Ordering::Acquire), ST_ONLINE | ST_REFUSED)
    });
    if !ok {
        let st = stage_now();
        return fail(st, "no answer before the ready deadline", sipis);
    }
    if boot.idt_loaded.load(Ordering::Acquire) {
        nmi_block = None;
    }
    if boot.stage.load(Ordering::Acquire) == ST_REFUSED {
        // SAFETY: the AP wrote `side` before publishing ST_REFUSED (Acquire above); it only halts afterwards.
        let why = unsafe { (*boot.side.get()).refused }.unwrap_or("refused");
        return fail(ApStage::Refused, why, sipis);
    }
    let ns_online = clock().saturating_sub(t0);
    let samples = tsc_rounds(boot, clock);
    if !wait_until(clock, deadline.max(clock() + TSC_ROUND_NS), || boot.stage.load(Ordering::Acquire) == ST_READY) {
        return fail(ApStage::Online, "no ready after the TSC rounds", sipis);
    }
    let ns_ready = clock().saturating_sub(t0);
    // SAFETY: written by the AP before ST_ONLINE (Acquire above), never written again.
    let side = unsafe { (*boot.side.get()).clone() };
    s.apic_id.store(req.apic_id, Ordering::Relaxed);
    set_topology(s, side.topology);
    s.state.store(ONLINE, Ordering::Release);
    percpu::set_online(cpu + 1);
    drop(nmi_block);
    // Pairs with `hand_over_aps`: if the hook was set before this AP counted as online, wake it here (the AP read
    // HANDOFF as 0 and halted; a second wake-up is harmless).
    core::sync::atomic::fence(Ordering::SeqCst);
    if handed_over() {
        let _ = wake(cpu);
    }
    Ok(Ok(ApStarted {
        cpu: req.cpu,
        apic_id: req.apic_id,
        sipis,
        ns_alive,
        ns_online,
        ns_ready,
        setup: side.setup,
        mtrr: side.mtrr,
        syscfg_fix: side.syscfg_fix,
        lapic: side.lapic,
        xsave_size: side.xsave_size,
        flags: side.flags,
        topology: side.topology,
        tsc_adjust: side.tsc_adjust,
        syscfg_differs: side.syscfg_differs,
        tsc_samples: samples,
        ist: gdt.ist_stacks(),
        stack: (sb.guard, sb.bottom, sb.top),
    }))
}

/// Compares this CPU's MTRRs (and AMD SYSCFG memory-type bits) with the BSP's snapshot and copies the BSP's
/// when they differ, as at start-up; allocation-free, so `call_on` can run it on an AP. `None` before `prepare`
/// or without MTRRs, `Some(Err(why))` when the CPU cannot take the BSP's (MTRRcap/TOM differ).
pub fn resync_mtrr() -> Option<Result<bool, &'static str>> {
    let bsp = shared()?.mtrr.as_ref()?;
    Some(match cpu::mtrr_action_live(bsp) {
        MtrrAction::Same => Ok(false),
        MtrrAction::Copy { syscfg } => {
            cpu::mtrr_load(bsp, syscfg);
            Ok(true)
        }
        MtrrAction::Refuse(why) => Err(why),
    })
}

/// The BSP's MTRR snapshot every AP is compared with (`None` before `prepare` or without MTRRs).
pub fn bsp_mtrr() -> Option<&'static MtrrState> {
    shared()?.mtrr.as_ref()
}

/// The BSP's side of the TSC handshake: interrupts off, a stamp before and after each round.
fn tsc_rounds(b: &ApBoot, clock: fn() -> u64) -> Vec<(u64, u64, u64)> {
    let mut out = Vec::with_capacity(TSC_ROUNDS as usize);
    let _irq = cpu::IrqGuard::new();
    for r in 1..=TSC_ROUNDS {
        let t0 = cpu::rdtsc_ordered();
        b.tsc_ask.store(r, Ordering::Release);
        let deadline = clock().saturating_add(TSC_ROUND_NS);
        if !wait_until(clock, deadline, || b.tsc_told.load(Ordering::Acquire) == r) {
            break;
        }
        let t2 = cpu::rdtsc_ordered();
        out.push((t0, b.tsc_value.load(Ordering::Relaxed), t2));
    }
    b.tsc_ask.store(TSC_DONE, Ordering::Release);
    out
}

// ---------------------------------------------------------------------------------------------------------
// The AP side

#[cfg_attr(not(target_os = "none"), allow(dead_code))]
fn refuse(b: &ApBoot, why: &'static str) -> ! {
    // SAFETY: only this AP writes `side`, before publishing ST_REFUSED.
    unsafe { (*b.side.get()).refused = Some(why) };
    b.stage.store(ST_REFUSED, Ordering::Release);
    loop {
        cpu::disable_interrupts();
        cpu::halt();
    }
}

/// The AP's side of the TSC handshake: answers each round with its own stamp until the BSP says done (or stays
/// quiet for `quiet_ns`).
#[cfg_attr(not(target_os = "none"), allow(dead_code))]
fn tsc_answer(b: &ApBoot) {
    let deadline = (b.clock)().saturating_add(b.quiet_ns);
    let mut spins = 0u32;
    loop {
        let a = b.tsc_ask.load(Ordering::Acquire);
        if a == TSC_DONE {
            return;
        }
        if a != 0 && a != b.tsc_told.load(Ordering::Relaxed) {
            b.tsc_value.store(cpu::rdtsc_ordered(), Ordering::Relaxed);
            b.tsc_told.store(a, Ordering::Release);
        }
        spins = spins.wrapping_add(1);
        if spins.is_multiple_of(1024) && (b.clock)() > deadline {
            return;
        }
        core::hint::spin_loop();
    }
}

/// Entered from `hal_ap_entry` (trampoline.rs) on the AP's boot stack, interrupts off, CR3 = the kernel root.
#[cfg(target_os = "none")]
#[unsafe(no_mangle)]
extern "C" fn hal_ap_main(boot: *const ApBoot) -> ! {
    // SAFETY: `boot` is the leaked block `start_ap` passed through the trampoline's `arg`; it is never freed.
    let b: &'static ApBoot = unsafe { &*boot };
    b.stage.store(ST_IN_KERNEL, Ordering::Release);
    b.step.store(1, Ordering::Release);
    // The GDT first (loading the GS selector there clears the GS base), then GS: everything after this may use
    // `this_cpu` (klog, locks, PerCpu, the trap handlers).
    crate::gdt::load(b.gdt);
    percpu::install_this_cpu(b.local);
    crate::idt::load_this_cpu();
    b.idt_loaded.store(true, Ordering::Release);
    ap_setup(b)
}

#[cfg_attr(not(target_os = "none"), allow(dead_code))]
fn ap_setup(b: &'static ApBoot) -> ! {
    let sh = b.shared;
    b.step.store(2, Ordering::Release);
    let v = cpu::live();
    // The relaxed check, before CR4/XCR0 are written (a missing bit would fault there).
    let bsp_view_has = |n: &str| match n {
        "nx" => sh.features.nx,
        "smep" => sh.features.smep,
        "smap" => sh.features.smap,
        "umip" => sh.features.umip,
        "fsgsbase" => sh.features.fsgsbase,
        "x2apic" => sh.features.x2apic && apic::mode() == apic::Mode::X2Apic,
        "xsave" => sh.features.xsave,
        "xsaves" => sh.features.xsaves,
        "avx" => sh.features.avx,
        "avx512f" => sh.features.avx512f,
        "constant_tsc" => sh.features.invariant_tsc,
        "pge" => sh.features.pge,
        "mce" => sh.features.mce,
        "pat" => sh.features.pat,
        "rdtscp" => sh.features.rdtscp,
        "rdpid" => sh.features.rdpid,
        "pcid" => sh.features.pcid,
        "invpcid" => sh.features.invpcid,
        "pdpe1gb" => sh.features.pdpe1gb,
        _ => true,
    };
    if let Some(n) = cpu::AP_REQUIRED.iter().copied().find(|n| bsp_view_has(n) && !v.has(n)) {
        refuse(b, required_reason(n));
    }
    if !cpu::missing_v3(&v).is_empty() {
        refuse(b, "not x86-64-v3 (R10)");
    }
    if let Some(why) = cpu::xsave_mismatch(&sh.xsave, &XsaveSig::from_view(&v)) {
        refuse(b, why);
    }
    b.step.store(3, Ordering::Release);
    let setup = match cpu::init_ap_cpu(&sh.features, b.cpu, b.node, sh.cr4) {
        Ok(r) => r,
        Err(why) => refuse(b, why),
    };
    b.step.store(12, Ordering::Release);
    let syscfg_fix = cpu::amd_syscfg_fix();
    b.step.store(4, Ordering::Release);
    let mtrr = match &sh.mtrr {
        None => MtrrOutcome::Absent,
        Some(_) if !v.has("mtrr") => refuse(b, "no MTRRs on this CPU, the BSP has them"),
        Some(bsp) => match cpu::mtrr_action_live(bsp) {
            MtrrAction::Same => MtrrOutcome::Same,
            MtrrAction::Copy { syscfg } => {
                b.step.store(13, Ordering::Release);
                cpu::mtrr_load(bsp, syscfg);
                b.step.store(14, Ordering::Release);
                MtrrOutcome::Copied { syscfg, verified: cpu::mtrr_action_live(bsp) == MtrrAction::Same }
            }
            MtrrAction::Refuse(why) => refuse(b, why),
        },
    };
    let syscfg_differs = match sh.mtrr.as_ref().and_then(|m| m.amd) {
        Some((bsp, _, _)) => {
            let mine = crate::msr::read(crate::msr::AMD_SYSCFG);
            (cpu::syscfg_other_differences(bsp, mine) != 0).then_some((bsp, mine))
        }
        None => None,
    };
    b.step.store(5, Ordering::Release);
    crate::pat::init_this_cpu();
    b.step.store(6, Ordering::Release);
    crate::entry::init_syscall();
    b.step.store(7, Ordering::Release);
    let x = crate::xsave::init_this_cpu();
    if x.size != sh.xsave_size {
        refuse(b, "XSAVE area size differs from the BSP's");
    }
    b.step.store(8, Ordering::Release);
    let lapic = match apic::init_ap(b.nmi) {
        Ok(l) => l,
        Err(apic::ApLapicError::BspNotUp) => refuse(b, "LAPIC: the BSP's is not set up"),
        Err(apic::ApLapicError::OtherBase(_)) => refuse(b, "LAPIC at another address than the BSP's"),
        Err(apic::ApLapicError::NoX2Apic) => refuse(b, "no x2APIC, the BSP runs x2APIC"),
    };
    b.step.store(9, Ordering::Release);
    let flags = FlagBits::from_view(&v);
    let topology = this_cpu_topology(&v, b.node);
    let tsc_adjust = v.has("tsc_adjust").then(|| crate::msr::read(crate::msr::IA32_TSC_ADJUST));
    // SAFETY: only this AP writes `side`, before publishing ST_ONLINE (Release).
    unsafe {
        *b.side.get() = ApSide { refused: None, setup, mtrr, syscfg_fix, lapic, xsave_size: x.size, flags, topology, tsc_adjust, syscfg_differs };
    }
    b.step.store(10, Ordering::Release);
    b.stage.store(ST_ONLINE, Ordering::Release);
    tsc_answer(b);
    b.step.store(11, Ordering::Release);
    b.stage.store(ST_READY, Ordering::Release);
    idle_loop(b.cpu as usize)
}

fn required_reason(name: &str) -> &'static str {
    match name {
        "nx" => "lacks NX, which the BSP uses",
        "smep" => "lacks SMEP, which the BSP uses",
        "smap" => "lacks SMAP, which the BSP uses",
        "umip" => "lacks UMIP, which the BSP uses",
        "fsgsbase" => "lacks FSGSBASE, which the BSP uses",
        "x2apic" => "lacks x2APIC, which the BSP uses",
        "xsave" => "lacks XSAVE, which the BSP uses",
        "xsaves" => "lacks XSAVES, which the BSP uses",
        "avx" => "lacks AVX, which the BSP enables",
        "avx512f" => "lacks AVX-512, which the BSP enables",
        "constant_tsc" => "lacks the invariant TSC, which the clock relies on",
        "pge" => "lacks global pages, which the BSP uses",
        "mce" => "lacks machine-check exceptions, which the BSP enables",
        "pat" => "lacks PAT, which the BSP programs",
        "rdtscp" => "lacks RDTSCP, which the BSP uses",
        "rdpid" => "lacks RDPID, which the BSP uses",
        "pcid" => "lacks PCID, which the BSP has",
        "invpcid" => "lacks INVPCID, which the BSP has",
        "pdpe1gb" => "lacks 1 GiB pages, which the BSP has",
        _ => "lacks a feature the BSP uses",
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use crate::cpu::CpuidResult;
    use alloc::collections::BTreeMap;

    type Leaves = BTreeMap<(u32, u32), CpuidResult>;

    fn r(eax: u32, ebx: u32, ecx: u32, edx: u32) -> CpuidResult {
        CpuidResult { eax, ebx, ecx, edx }
    }
    fn text(s: &[u8; 12]) -> [u32; 3] {
        [u32::from_le_bytes([s[0], s[1], s[2], s[3]]), u32::from_le_bytes([s[4], s[5], s[6], s[7]]), u32::from_le_bytes([s[8], s[9], s[10], s[11]])]
    }
    fn vendor(t: &mut Leaves, max: u32, name: &[u8; 12]) {
        let w = text(name);
        t.insert((0, 0), r(max, w[0], w[2], w[1]));
    }
    fn view_of<T>(t: &Leaves, f: impl FnOnce(&CpuidView) -> T) -> T {
        let g = |l, s| t.get(&(l, s)).copied().unwrap_or_default();
        f(&CpuidView::new(&g))
    }
    /// Leaf 0xB / 0x1F level: (type, shift, count) for an APIC ID.
    fn level(t: &mut Leaves, leaf: u32, n: u32, ty: u32, shift: u32, count: u32, apic: u32) {
        t.insert((leaf, n), r(shift, count, (ty << 8) | n, apic));
    }

    /// Zen 4 (Mini-PC): 8 cores x 2 threads, APIC IDs 0-15, one CCX (L3 shared by 16), TOPOEXT.
    fn zen4(apic: u32) -> Leaves {
        let mut t = Leaves::new();
        vendor(&mut t, 0x10, b"AuthenticAMD");
        t.insert((1, 0), r(0x00A7_0F52, (apic << 24) | (16 << 16), 0, 1 << 28));
        level(&mut t, 0xB, 0, 1, 1, 2, apic);
        level(&mut t, 0xB, 1, 2, 4, 16, apic);
        t.insert((0x8000_0000, 0), r(0x8000_0028, 0, 0, 0));
        t.insert((0x8000_0001, 0), r(0, 0, 1 << 22, 0));
        t.insert((0x8000_0008, 0), r(0x3030, 0, (4 << 12) | 15, 0));
        t.insert((0x8000_001D, 0), r((1 << 5) | 1 | (1 << 14), 0, 0, 0));
        t.insert((0x8000_001D, 1), r((1 << 5) | 2 | (1 << 14), 0, 0, 0));
        t.insert((0x8000_001D, 2), r((2 << 5) | 3 | (1 << 14), 0, 0, 0));
        t.insert((0x8000_001D, 3), r((3 << 5) | 3 | (15 << 14), 0, 0, 0));
        t.insert((0x8000_001E, 0), r(apic, (1 << 8) | (apic >> 1), 0, 0));
        t
    }

    #[test]
    fn zen4_smt_pairs_from_data() {
        for apic in 0..16 {
            let t = zen4(apic);
            let topo = view_of(&t, |v| decode_topology(v, 0));
            assert_eq!(topo.apic_id, apic);
            assert_eq!(topo.source_leaf, 0xB);
            assert_eq!(topo.package_id, 0);
            assert_eq!(topo.core_id, apic >> 1);
            assert_eq!(topo.thread_id, apic & 1);
            assert_eq!(topo.threads_per_core, 2);
            assert_eq!(topo.cluster_id, 0, "one CCX shares the L3");
            assert_eq!(topo.smt_sibling_of, (apic & 1 == 1).then(|| apic - 1));
            assert_eq!(topo.core_type, CORE_TYPE_UNIFORM);
        }
    }

    /// QEMU `-smp 16,sockets=2,dies=2,cores=2,threads=2` on an Intel host: leaf 0x1F with a die level.
    fn qemu_dies(apic: u32) -> Leaves {
        let mut t = Leaves::new();
        vendor(&mut t, 0x1F, b"GenuineIntel");
        t.insert((1, 0), r(0x000C_0662, (apic << 24) | (8 << 16), 0, 1 << 28));
        level(&mut t, 0x1F, 0, 1, 1, 2, apic);
        level(&mut t, 0x1F, 1, 2, 2, 4, apic);
        level(&mut t, 0x1F, 2, 5, 3, 8, apic);
        level(&mut t, 0xB, 0, 1, 1, 2, apic);
        level(&mut t, 0xB, 1, 2, 3, 8, apic);
        // L3 per die (4 threads), L2 per core.
        t.insert((4, 0), r((1 << 5) | 1 | (1 << 14), 0, 0, 0));
        t.insert((4, 1), r((2 << 5) | 3 | (1 << 14), 0, 0, 0));
        t.insert((4, 2), r((3 << 5) | 3 | (3 << 14), 0, 0, 0));
        t.insert((0x8000_0000, 0), r(0x8000_0008, 0, 0, 0));
        t
    }

    #[test]
    fn qemu_sockets_dies_cores_threads() {
        let mut seen = std::collections::BTreeSet::new();
        for apic in 0..16 {
            let t = qemu_dies(apic);
            let topo = view_of(&t, |v| decode_topology(v, 0));
            assert_eq!(topo.source_leaf, 0x1F);
            assert_eq!(topo.package_id, apic >> 3, "apic {apic}");
            assert_eq!(topo.die_id, apic >> 2, "dies are numbered by the core level's shift");
            assert_eq!(topo.core_id, (apic & 7) >> 1);
            assert_eq!(topo.thread_id, apic & 1);
            assert_eq!(topo.cluster_id, apic >> 2, "L3 per die");
            assert!(seen.insert((topo.package_id, topo.core_id, topo.thread_id)), "unique");
        }
        assert_eq!(seen.len(), 16);
    }

    /// Arrow Lake (Gaming-PC): no SMT, hybrid, P cores APIC 0-14 even (shift 1 with one thread), E cores later.
    fn arrow_lake(apic: u32, e_core: bool) -> Leaves {
        let mut t = Leaves::new();
        vendor(&mut t, 0x23, b"GenuineIntel");
        t.insert((1, 0), r(0x000C_0662, (apic << 24) | (32 << 16), 0, 1 << 28));
        t.insert((7, 0), r(1, 0, 0, 1 << 15));
        level(&mut t, 0x1F, 0, 1, 1, 1, apic);
        level(&mut t, 0x1F, 1, 2, 7, 20, apic);
        t.insert((0x1A, 0), r(if e_core { 0x20 << 24 } else { 0x40 << 24 }, 0, 0, 0));
        t.insert((4, 0), r((3 << 5) | 3 | (63 << 14), 0, 0, 0));
        t
    }

    #[test]
    fn hybrid_core_types() {
        let p = view_of(&arrow_lake(4, false), |v| decode_topology(v, 0));
        let e = view_of(&arrow_lake(40, true), |v| decode_topology(v, 0));
        assert_eq!(p.core_type, CORE_TYPE_PERFORMANCE);
        assert_eq!(e.core_type, CORE_TYPE_EFFICIENCY);
        assert_eq!((p.thread_id, p.smt_sibling_of, p.threads_per_core), (0, None, 1), "no SMT: one thread, no sibling");
        assert_eq!(p.core_id, 2);
        assert_eq!(e.core_id, 20);
        assert_eq!(p.cluster_id, e.cluster_id, "one L3 for the whole package");
        // Without the hybrid flag leaf 0x1A is not trusted.
        let mut t = arrow_lake(4, false);
        t.insert((7, 0), r(1, 0, 0, 0));
        assert_eq!(view_of(&t, |v| decode_topology(v, 0)).core_type, CORE_TYPE_UNIFORM);
    }

    /// CP-6: the capacity hint from the HWP numbers the 265K reports (Linux `cpu_capacity`: 1024/1012/756).
    #[test]
    fn hybrid_capacity() {
        let with_hwp = |apic, e| {
            let mut t = arrow_lake(apic, e);
            t.insert((6, 0), r(0xF86, 0, 0, 0));
            t
        };
        // IA32_HWP_CAPABILITIES: lowest, efficient, guaranteed, highest (byte 0) like the host's P/E cores.
        let topo = |t: &Leaves, highest: u64| {
            view_of(t, |v| Topology {
                perf_highest: hwp_highest_perf(v, &mut |m| match m {
                    crate::msr::IA32_PM_ENABLE => 1,
                    crate::msr::IA32_HWP_CAPABILITIES => 0x0101_3E00 | highest,
                    _ => panic!("MSR {m:#x}"),
                }),
                ..decode_topology(v, 0)
            })
        };
        let fav = topo(&with_hwp(6, false), 88);
        let p = topo(&with_hwp(4, false), 87);
        let e = topo(&with_hwp(40, true), 65);
        assert_eq!((fav.perf_highest, p.perf_highest, e.perf_highest), (88, 87, 65));
        assert_eq!((fav.capacity(88), p.capacity(88), e.capacity(88)), (1024, 1012, 756));
        assert_eq!(p.capacity(87), 1024, "before the favoured core is up");
        assert_eq!(fav.capacity(87), 1024, "never above the scale");
        // CPUID hwp but HWP left off by the firmware (PM_ENABLE bit 0 clear): the capabilities MSR would #GP, so it
        // is never read and the core type decides.
        let mut asked = Vec::new();
        let off = view_of(&with_hwp(40, true), |v| Topology {
            perf_highest: hwp_highest_perf(v, &mut |m| {
                asked.push(m);
                assert_eq!(m, crate::msr::IA32_PM_ENABLE, "HWP_CAPABILITIES read with HWP off");
                0
            }),
            ..decode_topology(v, 0)
        });
        assert_eq!(asked, [crate::msr::IA32_PM_ENABLE]);
        assert_eq!((off.perf_highest, off.capacity(0)), (0, E_CORE_FALLBACK_CAPACITY));
        // Without CPUID hwp: no MSR read, the core type decides.
        let never = &mut |m: u32| -> u64 { panic!("MSR {m:#x} read without CPUID hwp") };
        let plain_e = view_of(&arrow_lake(40, true), |v| Topology { perf_highest: hwp_highest_perf(v, never), ..decode_topology(v, 0) });
        assert_eq!((plain_e.perf_highest, plain_e.capacity(0), plain_e.capacity(88)), (0, E_CORE_FALLBACK_CAPACITY, E_CORE_FALLBACK_CAPACITY));
        let plain_p = view_of(&arrow_lake(4, false), |v| decode_topology(v, 0));
        assert_eq!(plain_p.capacity(0), CAPACITY_SCALE);
        // AMD (even with bit 7 of leaf 6 set) and a non-hybrid Intel never read it and stay uniform.
        let mut amd = zen4(3);
        amd.insert((6, 0), r(0x84, 0, 0, 0));
        let a = view_of(&amd, |v| Topology { perf_highest: hwp_highest_perf(v, never), ..decode_topology(v, 0) });
        assert_eq!((a.perf_highest, a.capacity(0)), (0, CAPACITY_SCALE));
        let mut uniform = with_hwp(4, false);
        uniform.insert((7, 0), r(1, 0, 0, 0));
        assert_eq!(view_of(&uniform, |v| hwp_highest_perf(v, never)), 0);
    }

    #[test]
    fn legacy_leaves_only() {
        // AMD without leaf 0xB: package size from 0x80000008, threads from 0x8000001E.
        let mut t = zen4(5);
        t.insert((0, 0), r(0xA, text(b"AuthenticAMD")[0], text(b"AuthenticAMD")[2], text(b"AuthenticAMD")[1]));
        let topo = view_of(&t, |v| decode_topology(v, 3));
        assert_eq!(topo.source_leaf, 1);
        assert_eq!((topo.apic_id, topo.core_id, topo.thread_id, topo.node), (5, 2, 1, 3));
        assert_eq!(topo.smt_sibling_of, Some(4));
        // Intel without 0xB/0x1F and without HTT: one thread, package = APIC ID.
        let mut t = Leaves::new();
        vendor(&mut t, 4, b"GenuineIntel");
        t.insert((1, 0), r(0x306A9, 3 << 24, 0, 0));
        let topo = view_of(&t, |v| decode_topology(v, 0));
        assert_eq!((topo.package_id, topo.core_id, topo.thread_id, topo.threads_per_core), (3, 0, 0, 1));
    }

    #[test]
    fn hostile_levels_terminate() {
        // A hypervisor that never reports level type 0: the walk stops at its bound.
        let mut t = Leaves::new();
        vendor(&mut t, 0xB, b"GenuineIntel");
        for n in 0..200 {
            level(&mut t, 0xB, n, 2, 40, 1, 9);
        }
        let topo = view_of(&t, |v| decode_topology(v, 0));
        assert_eq!(topo.apic_id, 9);
        assert_eq!(topo.package_id, 0, "shift >= 32 keeps everything in one package");
    }

    #[test]
    fn helpers() {
        assert_eq!(ceil_log2(0), 0);
        assert_eq!(ceil_log2(1), 0);
        assert_eq!(ceil_log2(2), 1);
        assert_eq!(ceil_log2(3), 2);
        assert_eq!(ceil_log2(16), 4);
        assert_eq!(low_mask(0), 0);
        assert_eq!(low_mask(32), u32::MAX);
        assert_eq!(required_reason("smap"), "lacks SMAP, which the BSP uses");
        for n in cpu::AP_REQUIRED {
            assert!(cpu::FLAGS.iter().any(|d| d.name == *n), "{n} not in FLAGS");
        }
    }
}
