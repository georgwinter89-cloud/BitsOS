//! CP-4 kernel side: SMP bring-up (spec/core.md §6.1-6.3). One Core initcall (`cpu.smp`, after `cpu.setup`):
//! 1. the trampoline record from BootInfo (kept by `acpi::init`), the MADT's CPUs (`plan`), ksync's per-CPU
//!    records for every possible CPU (before any AP takes a lock);
//! 2. `hal::smp::prepare` (trampoline page, low PML4, the BSP's MTRRs and topology), then every AP through
//!    `hal::smp::start_ap`, one after the other (dense CPU numbers; an AP that fails is parked with INIT and the
//!    next one gets its number);
//! 3. per AP an event with its times, MTRR outcome, LAPIC, topology and TSC offset; feature differences that
//!    do not refuse the AP are a warning (relaxed compare, V2-PLAN §2.3); one `smp.done` and one `smp.shape`.
//!
//! The APs then idle with interrupts on until SC-3 calls `hal::smp::hand_over_aps`.

use crate::plan::{self, PlannedAp};
use crate::report;
use crate::tsc::{self, TscOffset};
use alloc::string::String;
use alloc::vec::Vec;
use hal::cpu::hwp;
use hal::smp::{ApRequest, ApStarted, Env, MtrrOutcome, StartError};
use klog::Hex;
use ksync::SpinLock;

kreg::initcall!(kreg::Level::Core, "cpu.smp", bring_up);

/// Ready deadline per AP (INIT to ready): 100 ms on hardware (spec §6.1); generous under a hypervisor, where
/// timing is never judged.
pub const READY_NS_HARDWARE: u64 = 100_000_000;
pub const READY_NS_HYPERVISOR: u64 = 5_000_000_000;

/// What the bring-up recorded per AP (for ktests and `summary`).
#[derive(Clone, Debug)]
pub struct ApRecord {
    pub cpu: u32,
    pub apic_id: u32,
    pub sipis: u32,
    pub ns_ready: u64,
    pub mtrr: MtrrOutcome,
    pub tsc: Option<TscOffset>,
    /// IA32_TSC_ADJUST on the AP (CPUID `tsc_adjust`).
    pub tsc_adjust: Option<u64>,
    /// FLAGS names that differ from the BSP's (logged, not refused).
    pub differs: Vec<&'static str>,
    /// HWP on this AP.
    pub hwp: hal::cpu::hwp::Outcome,
}

/// The bring-up's result.
#[derive(Clone, Debug, Default)]
pub struct Outcome {
    pub ran: bool,
    /// Why nothing was started (no trampoline, no MADT, one CPU, ...).
    pub not_started: Option<&'static str>,
    pub planned: usize,
    pub up: Vec<ApRecord>,
    pub failed: Vec<StartError>,
    pub bsp_mtrr_ranges: usize,
    pub bsp_tsc_adjust: Option<u64>,
    pub total_ns: u64,
    /// HWP on the BSP (`hwp_on_bsp`).
    pub bsp_hwp: hal::cpu::hwp::Outcome,
}

static OUTCOME: SpinLock<Option<Outcome>> = SpinLock::named("cp.smp.outcome", None);

/// The bring-up's result (`None` before the initcall ran).
pub fn outcome() -> Option<Outcome> {
    OUTCOME.lock().clone()
}

klog::counter!(APS_UP, "smp.aps_up");
klog::counter!(APS_FAILED, "smp.aps_failed");

fn bring_up() -> Result<(), &'static str> {
    // First: the BSP's topology, recorded on every path of `run`, needs its real HWP number.
    let bsp_hwp = hwp_on_bsp();
    let mut out = run();
    out.bsp_hwp = bsp_hwp;
    // CP-8: every CPU that will ever run kernel code has its CR4 now; kernel faults may read the GS base.
    hal::idt::arm_gs_base_check();
    *OUTCOME.lock() = Some(out);
    report_msr_faults("smp");
    Ok(())
}

/// How many of `hal::msr::faults`' registers a boot line has named.
static FAULTS_NAMED: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// GP-CPU: one line for the registers whose fault-safe access faulted and that no earlier line named. `stage` is
/// "smp" (every CPU ran its set-up and HWP) or "boot" (the drivers and services started). A machine on which
/// nothing faults, as the Mini-PC, gets no line; later faults show in `hw_info` (`msr_faults_line`).
fn report_msr_faults(stage: &'static str) {
    let f = hal::msr::faults();
    let seen = FAULTS_NAMED.swap(f.distinct, core::sync::atomic::Ordering::AcqRel);
    if let Some(new) = report::new_fault_registers(&f, seen) {
        klog::event!(Warn, "cpu", "msr.faults", stage = stage, count = f.count, new = new, all = alloc::format!("{f}");
            "this CPU refused these registers: each access was skipped and nothing else changed");
    }
}

kreg::initcall!(kreg::Level::Late, "cpu.msr_faults", msr_faults_at_boot_end);
fn msr_faults_at_boot_end() -> Result<(), &'static str> {
    report_msr_faults("boot");
    Ok(())
}

/// One text line for `hw_info` (CPU section) and the screen report: every fault-safe MSR access that faulted since
/// boot, `cpu msr faults count=0 registers=none` on a machine where none did.
pub fn msr_faults_line() -> String {
    alloc::format!("cpu msr faults {}", hal::msr::faults())
}

/// HWP on the BSP wherever the CPU offers it (the APs follow on themselves), with the user's energy preference
/// (settings key `kernel.cpu_policy`, handed over as CONFIG `cpu_policy`).
fn hwp_on_bsp() -> hwp::Outcome {
    let keys = acpi::cpu_keys();
    // GP-CPU2: the EPP policy; read only where HWP can run, so a machine without it logs nothing new.
    let (policy, bad_policy) = keys.map_or((hwp::Policy::Balanced, false), |k| hwp::parse_policy(k.cpu_policy.as_deref()));
    if bad_policy && hwp::gate(&hal::cpu::live()).is_none() {
        klog::event!(Warn, "cpu", "config.bad", key = "cpu_policy", value = keys.and_then(|k| k.cpu_policy.clone()).unwrap_or_default();
            "cpu_policy= takes balanced|performance; balanced kept");
    }
    let o = hal::smp::hwp_bsp(policy);
    log_hwp(0, o);
    o
}

/// B6: what HWP did on `cpu`. A refusal is said once by the BSP; an AP says something when it switched HWP on or
/// failed (the refusal of an AP follows from the BSP's: the same CPUID bits and mode).
fn log_hwp(cpu: u32, o: hwp::Outcome) {
    match o {
        hwp::Outcome::On { was_on, lowest, highest, request } => {
            klog::event!(Info, "cpu", "hwp.on", cpu = cpu, was_on = was_on, lowest = lowest, highest = highest, request = Hex(request),
                epp = Hex(request >> 24 & 0xFF), policy = hal::smp::hwp_policy().name());
        }
        hwp::Outcome::Refused(r) if cpu == 0 => {
            klog::event!(Info, "cpu", "hwp.refused", cpu = cpu, reason = r.name(); "the clocks stay as the firmware left them");
        }
        hwp::Outcome::Refused(_) => {}
        hwp::Outcome::Failed(f) => {
            let (a, b) = match f {
                hwp::Failure::NotEnabled => (0, 0),
                hwp::Failure::BadCapabilities(c) => (c, 0),
                hwp::Failure::RequestDiffers(w, r) => (w, r),
                // GP-CPU: `value` is the register whose access raised #GP.
                hwp::Failure::MsrFault(msr) => (msr as u64, 0),
            };
            klog::event!(Warn, "cpu", "hwp.failed", cpu = cpu, reason = f.name(), value = Hex(a), read = Hex(b); "the clocks stay as the firmware left them");
        }
    }
}

fn skip(why: &'static str) -> Outcome {
    klog::event!(Info, "smp", "not_started", reason = why; "one CPU runs");
    record_bsp_alone();
    Outcome { ran: true, not_started: Some(why), ..Outcome::default() }
}

/// The one-CPU paths: the BSP still gets its APIC ID and topology recorded as CPU 0 (CP-4b).
fn record_bsp_alone() {
    let srat = acpi::tables().and_then(|t| t.srat.as_ref());
    match hal::smp::record_bsp(plan::node_of(srat, hal::apic::id().unwrap_or(0))) {
        Ok(t) => log_topology(0, &t),
        Err(e) => klog::event!(Warn, "smp", "bsp.not_recorded", error = alloc::format!("{e:?}"); "CPU 0 has no topology record"),
    }
}

fn run() -> Outcome {
    let start = time::now_ns();
    let Some(tramp) = acpi::trampoline() else { return skip("BootInfo has no trampoline record") };
    let Some(tables) = acpi::tables() else { return skip("no ACPI tables") };
    let Some(madt) = tables.madt.as_ref() else { return skip("no MADT") };
    if hal::apic::mode() == hal::apic::Mode::Off {
        return skip("the BSP's LAPIC is not set up (CP-3)");
    }
    let possible = hal::percpu::possible_cpus();
    // Before any AP takes a lock (SC-2 change request 4).
    ksync::prepare_cpus(possible);
    // The allocators' per-CPU caches and MM-6's per-CPU TLB state for every possible CPU, from here: their own
    // paths never create a slot (an AP growing one inside the allocator re-entered it and hung).
    mm_core::prepare_cpus();
    hal::paging::tlb::prepare_cpus();
    let bsp_apic = hal::apic::id().unwrap_or(0);
    let srat = tables.srat.as_ref();
    let prep = match hal::smp::prepare(tramp, plan::node_of(srat, bsp_apic), &mut mm_core::alloc_table_frame) {
        Ok(p) => p,
        Err(e) => {
            klog::event!(Error, "smp", "prepare.failed", error = alloc::format!("{e:?}"); "the APs stay off");
            record_bsp_alone();
            return Outcome { ran: true, not_started: Some("prepare failed"), ..Outcome::default() };
        }
    };
    let bt = prep.bsp_topology;
    klog::event!(Info, "smp", "prepared", trampoline = Hex(prep.trampoline_page), low_pml4 = Hex(prep.low_pml4),
        code_bytes = prep.code_len, bsp_apic = prep.bsp_apic_id, possible = possible,
        mtrr_ranges = prep.mtrr.as_ref().map_or(0, |m| m.ranges.len()),
        mtrr_def_type = Hex(prep.mtrr.as_ref().map_or(0, |m| m.def_type)));
    if let Some(adj) = prep.bsp_tsc_adjust {
        klog::event!(Info, "cpu", "tsc_adjust", cpu = 0u32, value = Hex(adj));
    }
    if let Some((before, after)) = prep.syscfg_fix {
        klog::event!(Info, "cpu", "syscfg.fixed", cpu = 0u32, before = Hex(before), after = Hex(after); "MtrrFixDramModEn cleared (Linux does the same)");
    }
    log_topology(0, &bt);
    let x2 = hal::apic::mode() == hal::apic::Mode::X2Apic;
    let p = plan::plan(madt, bsp_apic, x2);
    for &(apic, why) in p.skipped.iter().filter(|(_, w)| *w != plan::Skip::Bsp) {
        klog::event!(Info, "smp", "cpu.skipped", apic_id = apic, reason = why.name());
    }
    let hyper = hal::cpu::detect().hypervisor;
    let ready_ns = if hyper { READY_NS_HYPERVISOR } else { READY_NS_HARDWARE };
    let bsp_flags = hal::cpu::FlagBits::from_view(&hal::cpu::live());
    let mut out = Outcome { ran: true, planned: p.aps.len(), bsp_mtrr_ranges: prep.mtrr.as_ref().map_or(0, |m| m.ranges.len()), bsp_tsc_adjust: prep.bsp_tsc_adjust, ..Outcome::default() };
    let mut stack_page = || mm_core::frames()?.alloc(0, mm_core::Zone::Normal, mm_core::Kind::Stack);
    let mut free_page = |phys: u64| {
        if let Some(fa) = mm_core::frames() {
            fa.free(phys, 0, mm_core::Kind::Stack);
        }
    };
    let mut table = mm_core::alloc_table_frame;
    for ap in &p.aps {
        let mut env = Env { alloc_stack_page: &mut stack_page, free_stack_page: &mut free_page, alloc_table: &mut table, free_frame: free_stack_frame, clock: time::now_ns };
        match start_one(ap, madt, srat, ready_ns, &mut env) {
            Err(e) => {
                klog::event!(Error, "smp", "start.stopped", apic_id = ap.apic_id, error = alloc::format!("{e:?}"); "no further AP is started");
                break;
            }
            Ok(Err(f)) => {
                APS_FAILED.add(1);
                klog::event!(Error, "cpu", "ap.failed", apic_id = f.apic_id, cpu = f.cpu, stage = report::stage_name(f.stage),
                    step = f.step, reason = f.reason, sipis = f.sipis, elapsed_us = f.elapsed_ns / 1000; "sent INIT, stays unused");
                out.failed.push(f);
            }
            Ok(Ok(st)) => {
                APS_UP.add(1);
                out.up.push(log_ap(&st, &bsp_flags, hyper, prep.bsp_tsc_adjust));
            }
        }
    }
    out.total_ns = time::now_ns().saturating_sub(start);
    let topos: Vec<hal::smp::Topology> = (0..hal::percpu::cpu_count()).filter_map(hal::smp::topology).collect();
    let s = report::shape(&topos);
    klog::event!(Info, "smp", "shape", cpus = s.cpus, packages = s.packages, dies = s.dies, cores = s.cores,
        clusters = s.clusters, nodes = s.nodes, smt = s.max_threads_per_core, p_cores = s.p_cores, e_cores = s.e_cores,
        min_capacity = s.min_capacity, max_perf = hal::smp::max_perf(), hwp = hwp_state());
    let worst = out.up.iter().filter_map(|r| r.tsc).map(|t| t.magnitude()).max().unwrap_or(0);
    klog::event!(Info, "smp", "done", cpus = hal::percpu::cpu_count(), planned = out.planned, up = out.up.len(),
        failed = out.failed.len(), total_ms = out.total_ns / 1_000_000, tsc_worst_ticks = worst);
    out
}

fn free_stack_frame(phys: u64) {
    if let Some(fa) = mm_core::frames() {
        fa.free(phys, 0, mm_core::Kind::Stack);
    }
}

fn start_one(
    ap: &PlannedAp,
    madt: &acpi::madt::Madt,
    srat: Option<&acpi::srat::Srat>,
    ready_ns: u64,
    env: &mut Env<'_>,
) -> Result<Result<ApStarted, StartError>, hal::smp::SmpError> {
    let mut notes = Vec::new();
    let nmi = acpi::irq_plan::nmi_pin(Some(madt), ap.apic_id, &mut notes);
    if !notes.is_empty() {
        klog::event!(Warn, "apic", "nmi.bad_lint", apic_id = ap.apic_id; "LINT1 used");
    }
    let req = ApRequest { cpu: hal::percpu::cpu_count() as u32, apic_id: ap.apic_id, node: plan::node_of(srat, ap.apic_id), nmi, ready_ns };
    hal::smp::start_ap(&req, env)
}

fn log_ap(st: &ApStarted, bsp_flags: &hal::cpu::FlagBits, hyper: bool, bsp_tsc_adjust: Option<u64>) -> ApRecord {
    let tsc = tsc::offset(&st.tsc_samples);
    klog::event!(Info, "cpu", "ap.up", cpu = st.cpu, apic_id = st.apic_id, sipis = st.sipis,
        alive_us = st.ns_alive / 1000, online_us = st.ns_online / 1000, ready_us = st.ns_ready / 1000,
        mtrr = report::mtrr_name(st.mtrr), lapic_id = st.lapic.id, esr = Hex(st.lapic.esr as u64),
        cr0 = Hex(st.setup.cr0), cr0_was = Hex(st.setup.cr0_before), cr4 = Hex(st.setup.cr4), efer = Hex(st.setup.efer),
        xcr0 = Hex(st.setup.xcr0), xsave_bytes = st.xsave_size, stack_top = Hex(st.stack.2), df_ist = Hex(st.ist[0].top));
    if st.lapic.id != st.apic_id {
        klog::event!(Warn, "cpu", "ap.apic_id", cpu = st.cpu, madt = st.apic_id, lapic = st.lapic.id; "the LAPIC reports another ID than the MADT");
    }
    if let Some((before, after)) = st.syscfg_fix {
        klog::event!(Info, "cpu", "syscfg.fixed", cpu = st.cpu, before = Hex(before), after = Hex(after); "MtrrFixDramModEn cleared");
    }
    if let Some((bsp, ap)) = st.syscfg_differs {
        klog::event!(Warn, "cpu", "ap.syscfg_differs", cpu = st.cpu, bsp = Hex(bsp), ap = Hex(ap),
            bits = Hex(hal::cpu::syscfg_other_differences(bsp, ap));
            "SYSCFG differs from the BSP's outside the memory-type bits (kept, not copied)");
    }
    if st.tsc_adjust.is_some() && st.tsc_adjust != bsp_tsc_adjust {
        klog::event!(Warn, "cpu", "ap.tsc_adjust", cpu = st.cpu, ap = Hex(st.tsc_adjust.unwrap_or(0)), bsp = Hex(bsp_tsc_adjust.unwrap_or(0));
            "IA32_TSC_ADJUST differs from the BSP's (firmware left it; the offset check shows the effect)");
    }
    if matches!(st.mtrr, MtrrOutcome::Copied { verified: false, .. }) {
        klog::event!(Warn, "cpu", "ap.mtrr_unverified", cpu = st.cpu; "MTRRs copied from the BSP but read back differently");
    }
    if st.setup.cr0_before & (hal::cpu::CR0_CD | hal::cpu::CR0_NW) != 0 {
        klog::event!(Debug, "cpu", "ap.cache_was_off", cpu = st.cpu, cr0_was = Hex(st.setup.cr0_before));
    }
    log_hwp(st.cpu, st.hwp);
    log_topology(st.cpu, &st.topology);
    match tsc {
        Some(t) => {
            let adjust = Hex(st.tsc_adjust.unwrap_or(0));
            if !hyper && t.magnitude() > tsc::WARN_TICKS {
                klog::event!(Warn, "cpu", "ap.tsc", cpu = st.cpu, offset_ticks = t.median, best_ticks = t.best, min_rtt = t.min_rtt, rounds = t.rounds, tsc_adjust = adjust;
                    "TSC offset against the BSP above the warning level; the clock treats the TSCs as one");
            } else {
                klog::event!(Info, "cpu", "ap.tsc", cpu = st.cpu, offset_ticks = t.median, best_ticks = t.best, min_rtt = t.min_rtt, rounds = t.rounds, tsc_adjust = adjust,
                    over_check = t.magnitude() > tsc::CHECK_TICKS);
            }
        }
        None => klog::event!(Warn, "cpu", "ap.tsc", cpu = st.cpu, rounds = 0u32; "no TSC round came back"),
    }
    let differs = bsp_flags.differences(&st.flags);
    if !differs.is_empty() {
        klog::event!(Warn, "cpu", "ap.differs", cpu = st.cpu, flags = report::joined(&differs);
            "CPUID differs from the BSP's in features the kernel does not rely on (relaxed compare): kept");
    }
    ApRecord { cpu: st.cpu, apic_id: st.apic_id, sipis: st.sipis, ns_ready: st.ns_ready, mtrr: st.mtrr, tsc, tsc_adjust: st.tsc_adjust, differs, hwp: st.hwp }
}

/// HWP on the BSP for the shape event (B6: with "off" the capacity hint falls back to the core types): none (no Intel
/// CPUID hwp on bare metal), off (IA32_PM_ENABLE bit 0 clear: the firmware left it so and enabling failed), on.
fn hwp_state() -> &'static str {
    let caps = hal::msr::ReadCaps::live();
    match (caps.hwp, caps.hwp_enabled) {
        (false, _) => "none",
        (true, false) => "off",
        (true, true) => "on",
    }
}

fn log_topology(cpu: u32, t: &hal::smp::Topology) {
    klog::event!(Info, "cpu", "topology", cpu = cpu, apic_id = t.apic_id, package = t.package_id, die = t.die_id,
        core = t.core_id, thread = t.thread_id, threads_per_core = t.threads_per_core,
        sibling_of = t.smt_sibling_of.map_or(-1, |s| s as i64), cluster = t.cluster_id, l2 = t.l2_id, node = t.node,
        core_type = report::core_type_name(t.core_type), perf_highest = t.perf_highest, leaf = Hex(t.source_leaf as u64));
}

/// One text line for `hw_info` (CPU section): HWP and speculation control as this CPU has them.
pub fn hwp_line() -> String {
    let m = |x: Option<u64>| x.map_or(String::from("none"), |v| alloc::format!("{v:#x}"));
    let on = outcome().map_or(0, |o| usize::from(o.bsp_hwp.is_on()) + o.up.iter().filter(|r| r.hwp.is_on()).count());
    alloc::format!(
        "cpu hwp state={} on_cpus={on}/{} bsp={} request={} max_perf={} spec_ctrl={}",
        hwp_state(),
        hal::percpu::cpu_count(),
        outcome().map_or("not_run", |o| o.bsp_hwp.name()),
        m(hal::msr::read_checked(hal::msr::IA32_HWP_REQUEST)),
        hal::smp::max_perf(),
        m(hal::msr::read_checked(hal::msr::IA32_SPEC_CTRL)),
    )
}

/// Text lines for `hw_info` (CPU section) and the Mini-PC check list: one line per CPU with its topology, time
/// to ready and TSC offset, and the failures.
pub fn summary() -> Vec<String> {
    let mut out = Vec::new();
    let Some(o) = outcome() else { return alloc::vec![String::from("smp: bring-up has not run")] };
    if let Some(why) = o.not_started {
        out.push(alloc::format!("smp: not started ({why})"));
    }
    out.push(alloc::format!(
        "smp cpus={} planned={} up={} failed={} total_ms={}",
        hal::percpu::cpu_count(),
        o.planned,
        o.up.len(),
        o.failed.len(),
        o.total_ns / 1_000_000
    ));
    for cpu in 0..hal::percpu::cpu_count() {
        let Some(t) = hal::smp::topology(cpu) else { continue };
        let rec = o.up.iter().find(|r| r.cpu as usize == cpu);
        out.push(alloc::format!(
            "cpu {cpu} apic={} pkg={} die={} core={} thread={} cluster={} l2={} type={} capacity={} hwp={} ready_us={} tsc_offset={}{} tsc_adjust={:#x} mtrr={}",
            t.apic_id,
            t.package_id,
            t.die_id,
            t.core_id,
            t.thread_id,
            t.cluster_id,
            t.l2_id,
            report::core_type_name(t.core_type),
            t.capacity(hal::smp::max_perf()),
            rec.map_or(o.bsp_hwp, |r| r.hwp).name(),
            rec.map_or(0, |r| r.ns_ready / 1000),
            rec.and_then(|r| r.tsc).map_or(0, |x| x.median),
            if rec.and_then(|r| r.tsc).is_some_and(|x| x.magnitude() > tsc::CHECK_TICKS) { " (over 38)" } else { "" },
            if cpu == 0 { o.bsp_tsc_adjust } else { rec.and_then(|r| r.tsc_adjust) }.unwrap_or(0),
            rec.map_or("bsp", |r| report::mtrr_name(r.mtrr)),
        ));
    }
    for f in &o.failed {
        out.push(alloc::format!("cpu failed apic={} stage={} step={} reason={}", f.apic_id, report::stage_name(f.stage), f.step, f.reason));
    }
    out
}
