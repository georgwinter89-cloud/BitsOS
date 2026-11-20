//! GP-CPU2: the MWAIT plan's table selection on hybrid Intel, with the 265K's CPUID answers as faked inputs.

use super::fake::{Op, Recorder};
use super::*;
use acpi::cst::{CState, Entry};
use alloc::vec;
use alloc::vec::Vec;
use hal::cpu::mwait::MwaitCaps;

fn ffh(kind: u8, hint: u64, latency_us: u32) -> CState {
    CState { kind, entry: Entry::FixedHw(hint), latency_us }
}

fn plan_with(cpu: &CpuIdle, states: &[CState]) -> Plan {
    let fw = Firmware { states, pm_timer: Some(0x1808), pm1_status: [Some(0x1800), None] };
    plan(cpu, Some(&fw), &mut |p, _| panic!("hybrid Intel asked port {p:#x}"))
}

fn hints(p: &Plan) -> Vec<u8> {
    p.deep.iter().filter_map(|d| d.mwait).collect()
}

#[test]
fn no_cst_found_takes_the_model_table() {
    let p = plan(&CpuIdle::ARROW_LAKE, None, &mut |_, _| panic!("no port"));
    assert_eq!((p.mwait_source, hints(&p)), (Some(MwaitSource::Model), vec![0x21, 0x60]));
    let empty = plan_with(&CpuIdle::ARROW_LAKE, &[]);
    assert_eq!(empty, p, "an empty _CST is the same as none");
    let c1_only = plan_with(&CpuIdle::ARROW_LAKE, &[ffh(1, 0, 1)]);
    assert_eq!(c1_only, p, "a _CST with C1 alone gives nothing to take");
}

#[test]
fn another_hybrid_model_without_cst_stays_in_c1() {
    // Alder Lake S (0x97): Linux has a native table there; BitsOS has none and no _CST was read.
    let adl = CpuIdle { model: 0x97, ..CpuIdle::ARROW_LAKE };
    assert_eq!(plan(&adl, None, &mut |_, _| false).c1_only, Some(C1Only::HybridIntel));
    let from_cst = plan_with(&adl, &[ffh(1, 0, 1), ffh(2, 0x20, 100)]);
    assert_eq!((from_cst.mwait_source, hints(&from_cst)), (Some(MwaitSource::Cst), vec![0x20]), "its own FFH _CST is taken");
}

#[test]
fn cpuid5_decides_each_hint() {
    // Leaf 5 without a C3 sub-state (nibble 3): hint 0x21 goes, 0x60 stays.
    let cpu = CpuIdle { mwait: MwaitCaps { substates: 0x1010_0020, ..MwaitCaps::ARROW_LAKE }, ..CpuIdle::ARROW_LAKE };
    let p = plan(&cpu, None, &mut |_, _| false);
    assert_eq!(hints(&p), vec![0x60]);
    assert_eq!(p.refused.iter().map(|(c, w)| (c.entry, *w)).collect::<Vec<_>>(), vec![(Entry::FixedHw(0x21), Refused::MwaitHint)]);
    let none = CpuIdle { mwait: MwaitCaps { substates: 0x0000_0020, ..MwaitCaps::ARROW_LAKE }, ..CpuIdle::ARROW_LAKE };
    let q = plan(&none, None, &mut |_, _| false);
    assert_eq!((q.c1_only, q.mwait_source), (Some(C1Only::NoUsableState), None));
    assert!(!q.needs_check());
}

#[test]
fn without_mwait_or_its_break_hybrid_intel_stays_in_c1() {
    for caps in [MwaitCaps { ecx: 1, ..MwaitCaps::ARROW_LAKE }, MwaitCaps { monitor: false, ..MwaitCaps::ARROW_LAKE }, MwaitCaps::default()] {
        let cpu = CpuIdle { mwait: caps, ..CpuIdle::ARROW_LAKE };
        assert_eq!(plan(&cpu, None, &mut |_, _| false).c1_only, Some(C1Only::NoMwait), "{caps:?}");
    }
    // ARAT and the invariant TSC are asked first, as for every CPU.
    assert_eq!(plan(&CpuIdle { arat: false, ..CpuIdle::ARROW_LAKE }, None, &mut |_, _| false).c1_only, Some(C1Only::NoArat));
    assert_eq!(plan(&CpuIdle { invariant_tsc: false, ..CpuIdle::ARROW_LAKE }, None, &mut |_, _| false).c1_only, Some(C1Only::NoInvariantTsc));
}

#[test]
fn a_mixed_cst_is_not_usable_bad_states_are_refused_by_name() {
    // One I/O state past C1: Linux's `intel_idle_cst_usable` says no; the model table is taken instead.
    let mixed = [ffh(1, 0, 1), ffh(2, 0x21, 127), CState { kind: 3, entry: Entry::Io(0x1816), latency_us: 900 }];
    assert_eq!(plan_with(&CpuIdle::ARROW_LAKE, &mixed).mwait_source, Some(MwaitSource::Model));
    let odd = [ffh(2, 0x21, 0), ffh(2, 0x20, 100), ffh(2, 0x120, 110), ffh(3, 0x60, 20_000), ffh(5, 0x60, 10)];
    let p = plan_with(&CpuIdle::ARROW_LAKE, &odd);
    assert_eq!(hints(&p), vec![0x20]);
    let why: Vec<Refused> = p.refused.iter().map(|(_, w)| *w).collect();
    assert_eq!(why, vec![Refused::Latency, Refused::Duplicate, Refused::Latency, Refused::Kind], "the hint is the low byte (0x120 -> 0x20)");
}

#[test]
fn firmware_latencies_out_of_order_are_sorted() {
    let p = plan_with(&CpuIdle::ARROW_LAKE, &[ffh(2, 0x21, 900), ffh(3, 0x60, 300)]);
    assert!(p.latency_reordered);
    assert_eq!(p.deep.iter().map(|d| (d.mwait, d.latency_ns / 1000)).collect::<Vec<_>>(), vec![(Some(0x21), 300), (Some(0x60), 900)]);
}

/// The Mini-PC's plan does not need the check and is usable at once, whatever the check state says.
#[test]
fn port_plans_never_wait_for_the_check() {
    let s = acpi::cst::find(&acpi::cst::build::table(&acpi::cst::build::name(&acpi::cst::build::MINIPC))).unwrap().states;
    let fw = Firmware { states: &s, pm_timer: Some(0x808), pm1_status: [Some(0x800), None] };
    let p = plan(&CpuIdle::ZEN4, Some(&fw), &mut |_, _| true);
    assert!(!p.needs_check() && p.mwait_source.is_none() && p.deep.iter().all(|d| d.mwait.is_none()));
    for st in [CheckState::NotNeeded, CheckState::Pending, CheckState::Passed, CheckState::Failed] {
        assert_eq!(usable(Some(&p), st), Some(&p));
    }
}

/// The 265K's MWAIT plan waits for its check from the start; a port plan never needs one.
#[test]
fn an_mwait_plan_starts_pending() {
    let p = plan(&CpuIdle::ARROW_LAKE, None, &mut |_, _| false);
    assert_eq!(CheckState::at_init(p.needs_check()), CheckState::Pending);
    assert_eq!(CheckState::at_init(false), CheckState::NotNeeded);
    assert_eq!(usable(Some(&p), CheckState::Pending), None, "pending: C1 only");
}

/// The idle step on the 265K: before the check C1, after it the deepest state that fits the gap.
#[test]
fn the_step_uses_mwait_only_after_the_check() {
    let p = plan(&CpuIdle::ARROW_LAKE, None, &mut |_, _| false);
    let hw = Recorder::default();
    let quiet = || Some(50_000_000);
    assert_eq!(step(&hw, usable(Some(&p), CheckState::Pending), Mode::Auto, false, quiet, u64::MAX, DEFAULT_RESIDENCY_FACTOR), Entered::C1);
    assert_eq!(hw.take(), vec![Op::Halt]);
    assert_eq!(step(&hw, usable(Some(&p), CheckState::Passed), Mode::Auto, false, quiet, u64::MAX, DEFAULT_RESIDENCY_FACTOR), Entered::Deep(1));
    assert_eq!(hw.take(), vec![Op::Mwait(0x60), Op::Sti]);
    // 1 ms to the next timer: 0x21 (127 us x 4 fits), not 0x60.
    assert_eq!(step(&hw, usable(Some(&p), CheckState::Passed), Mode::Auto, false, || Some(1_000_000), u64::MAX, DEFAULT_RESIDENCY_FACTOR), Entered::Deep(0));
    assert_eq!(hw.take(), vec![Op::Mwait(0x21), Op::Sti]);
    assert_eq!(step(&hw, usable(Some(&p), CheckState::Passed), Mode::C1, false, quiet, u64::MAX, DEFAULT_RESIDENCY_FACTOR), Entered::C1, "C1 mode");
    assert_eq!(hw.take(), vec![Op::Halt]);
}

/// A failed check is one Warn line with the reason in plain words, and the state says failed (the only test that
/// sets the process-wide result).
#[test]
fn a_failed_check_is_one_warn_line() {
    let p = plan(&CpuIdle::ARROW_LAKE, None, &mut |_, _| false);
    let since = klog::next_seq();
    finish_check(&p, Err((8, 0x60, check::Fault::TimerMissed)));
    assert_eq!(check_state(), CheckState::Failed);
    assert!(matches!(check_outcome(), Some(Err((8, 0x60, check::Fault::TimerMissed)))));
    let lines: Vec<klog::Record> = klog::read_since(since, 10_000).into_iter().filter(|r| r.name == "idle.check").collect();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].level == klog::Level::Warn && lines[0].text.contains("timer_missed") && lines[0].text.contains("Wecker"), "{:?}", lines[0]);
    assert_eq!(usable(Some(&p), check_state()), None, "every CPU stays in C1");
}
