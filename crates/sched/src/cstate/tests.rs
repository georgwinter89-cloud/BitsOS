use super::fake::{Op, Recorder};
use super::*;
use acpi::cst::{build, CState, Entry};
use alloc::vec;
use alloc::vec::Vec;

const PM1A: u16 = 0x800;

fn minipc_states() -> Vec<CState> {
    acpi::cst::find(&build::table(&build::name(&build::MINIPC))).unwrap().states
}

fn fw(states: &[CState]) -> Firmware<'_> {
    Firmware { states, pm_timer: Some(0x808), pm1_status: [Some(PM1A), None] }
}

fn all_ok(_: u16, _: u32) -> bool {
    true
}

fn plan_of(cpu: &CpuIdle, states: &[CState]) -> Plan {
    plan(cpu, Some(&fw(states)), &mut all_ok)
}

fn io(kind: u8, port: u64, latency_us: u32) -> CState {
    CState { kind, entry: Entry::Io(port), latency_us }
}

#[test]
fn minipc_plan_has_c2_and_c3_with_the_bus_master_check() {
    let p = plan_of(&CpuIdle::ZEN4, &minipc_states());
    assert_eq!(
        p.deep,
        vec![
            Deep { kind: 2, port: 0x414, latency_ns: 18_000, bm_check: false, mwait: None },
            Deep { kind: 3, port: 0x415, latency_ns: 350_000, bm_check: true, mwait: None },
        ]
    );
    assert_eq!((p.c1_only, p.bm_status, p.dummy_read), (None, [Some(PM1A), None], None), "AMD: no dummy read");
    assert!(p.refused.is_empty() && !p.latency_reordered);
}

#[test]
fn deep_states_only_on_bare_metal_with_arat_and_an_invariant_tsc() {
    let s = minipc_states();
    let mut asked = 0;
    let cases = [
        (CpuIdle { hypervisor: true, ..CpuIdle::ZEN4 }, C1Only::Hypervisor),
        (CpuIdle { arat: false, ..CpuIdle::ZEN4 }, C1Only::NoArat),
        (CpuIdle { invariant_tsc: false, ..CpuIdle::ZEN4 }, C1Only::NoInvariantTsc),
    ];
    for (cpu, why) in cases {
        let p = plan(&cpu, Some(&fw(&s)), &mut |_, _| {
            asked += 1;
            true
        });
        assert_eq!((p.deep.len(), p.c1_only), (0, Some(why)));
    }
    assert_eq!(asked, 0, "no port of the table is even looked at");
    assert_eq!(plan(&CpuIdle::ZEN4, None, &mut all_ok).c1_only, Some(C1Only::NoCst));
    assert_eq!(plan_of(&CpuIdle::ZEN4, &[]).c1_only, Some(C1Only::NoCst));
    let c1_only = [CState { kind: 1, entry: Entry::FixedHw(0), latency_us: 1 }];
    assert_eq!(plan_of(&CpuIdle::ZEN4, &c1_only).c1_only, Some(C1Only::NoUsableState));
}

#[test]
fn hostile_states_are_refused_one_by_one() {
    let states = [
        io(0, 0x410, 5),
        io(4, 0x411, 5),
        io(255, 0x412, 5),
        CState { kind: 2, entry: Entry::FixedHw(0x10), latency_us: 18 },
        CState { kind: 2, entry: Entry::Other(0), latency_us: 18 },
        io(2, 0, 18),
        io(2, 0x1_0000, 18),
        io(2, 0x413, 0),
        io(2, 0x413, MAX_LATENCY_US + 1),
        io(2, 0x413, u32::MAX),
        io(2, 0x414, 18),
        io(2, 0x414, 20),
        io(3, 0x414, 350),
        io(2, 0x1F7, 18),
    ];
    let p = plan(&CpuIdle::ZEN4, Some(&fw(&states)), &mut |port, _| port != 0x1F7);
    let why: Vec<Refused> = p.refused.iter().map(|r| r.1).collect();
    use Refused::*;
    assert_eq!(why, vec![Kind, Kind, Kind, Ffh, Space, PortZero, PortRange, Latency, Latency, Latency, Duplicate, Duplicate, Guard]);
    assert_eq!(p.deep, vec![Deep { kind: 2, port: 0x414, latency_ns: 18_000, bm_check: false, mwait: None }]);
}

#[test]
fn c3_follows_linux_bus_master_rules() {
    let s = minipc_states();
    // Zen and modern Intel: C3 with the bus-master check; older AMD and other vendors would need WBINVD.
    let zen1 = CpuIdle { family: 0x17, model: 1, ..CpuIdle::ZEN4 };
    let k10 = CpuIdle { family: 0x10, model: 4, ..CpuIdle::ZEN4 };
    let hygon = CpuIdle { vendor: Vendor::Other, family: 0x18, ..CpuIdle::ZEN4 };
    let intel = CpuIdle { vendor: Vendor::Intel, family: 6, model: 0xC6, ..CpuIdle::ZEN4 };
    assert_eq!(plan_of(&zen1, &s).deep.len(), 2);
    assert_eq!(plan_of(&intel, &s).deep.len(), 2);
    for cpu in [k10, hygon] {
        let p = plan_of(&cpu, &s);
        assert_eq!(p.deep.len(), 1, "{cpu:?}");
        assert_eq!(p.refused, vec![(s[2], Refused::C3NeedsFlush)]);
        assert_eq!(p.bm_status, [None, None], "no C3, no bus-master port");
    }
    // No usable PM1 status: C3 is refused, C2 stays; the guard is asked once for PM1.
    let mut pm1_asked = 0;
    let p = plan(&CpuIdle::ZEN4, Some(&fw(&s)), &mut |port, bytes| {
        if port == PM1A {
            pm1_asked += 1;
            assert_eq!(bytes, 2);
            return false;
        }
        true
    });
    assert_eq!((p.deep.len(), p.refused[0].1, pm1_asked), (1, Refused::C3NoBmStatus, 1));
    let no_pm1 = Firmware { pm1_status: [None, None], ..fw(&s) };
    assert_eq!(plan(&CpuIdle::ZEN4, Some(&no_pm1), &mut all_ok).refused[0].1, Refused::C3NoBmStatus);
    let pm1b = Firmware { pm1_status: [Some(PM1A), Some(0x804)], ..fw(&s) };
    assert_eq!(plan(&CpuIdle::ZEN4, Some(&pm1b), &mut all_ok).bm_status, [Some(PM1A), Some(0x804)]);
    let pm1b_refused = plan(&CpuIdle::ZEN4, Some(&pm1b), &mut |p, _| p != 0x804);
    assert_eq!(pm1b_refused.refused[0].1, Refused::C3NoBmStatus, "half a bus-master status is none");
}

#[test]
fn intel_reads_the_pm_timer_after_the_state_and_amd_does_not() {
    let s = minipc_states();
    let intel = CpuIdle { vendor: Vendor::Intel, family: 6, model: 0xC6, ..CpuIdle::ZEN4 };
    assert_eq!(plan_of(&intel, &s).dummy_read, Some(0x808));
    assert_eq!(plan(&intel, Some(&fw(&s)), &mut |p, _| p != 0x808).dummy_read, None, "guard refused: no read");
    assert_eq!(plan_of(&CpuIdle::ZEN4, &s).dummy_read, None);
}

#[test]
fn latencies_out_of_order_are_sorted_like_linux() {
    let p = plan_of(&CpuIdle::ZEN4, &[io(3, 0x415, 10), io(2, 0x414, 100)]);
    assert!(p.latency_reordered);
    assert_eq!(p.deep.iter().map(|d| (d.kind, d.latency_ns)).collect::<Vec<_>>(), vec![(2, 10_000), (3, 100_000)]);
    let two_c2 = plan_of(&CpuIdle::ZEN4, &[io(2, 0x416, 60), io(2, 0x414, 18)]);
    assert!(!two_c2.latency_reordered);
    assert_eq!(two_c2.deep.iter().map(|d| d.port).collect::<Vec<_>>(), vec![0x414, 0x416], "several of one type: by latency");
}

fn minipc_deep() -> Vec<Deep> {
    plan_of(&CpuIdle::ZEN4, &minipc_states()).deep
}

#[test]
fn auto_goes_deep_only_when_both_timer_and_prediction_allow() {
    let d = minipc_deep();
    assert_eq!(choose(Mode::Auto, &d, None, 50_000, 4), None, "50 us predicted: C1");
    assert_eq!(choose(Mode::Auto, &d, None, 100_000, 4), Some(0), "100 us: C2 (72 us residency)");
    assert_eq!(choose(Mode::Auto, &d, Some(30_000), 10_000_000, 4), None, "timer in 30 us: C1");
    assert_eq!(choose(Mode::Auto, &d, Some(1_000_000), 10_000_000, 4), Some(0), "timer in 1 ms: C2 not C3");
    assert_eq!(choose(Mode::Auto, &d, Some(2_000_000), 10_000_000, 4), Some(1), "2 ms: C3");
    assert_eq!(choose(Mode::Auto, &d, None, u64::MAX, 4), Some(1));
    assert_eq!(choose(Mode::Auto, &d, None, u64::MAX, u32::MAX), Some(1), "no overflow");
    assert_eq!(choose(Mode::C1, &d, None, u64::MAX, 4), None);
    assert_eq!(choose(Mode::Forced, &d, Some(0), 0, 4), Some(0));
    assert_eq!(choose(Mode::Auto, &[], None, u64::MAX, 4), None);
    assert_eq!(choose(Mode::Forced, &[], None, u64::MAX, 4), None);
}

#[test]
fn prediction_drops_fast_and_grows_slowly() {
    assert_eq!(predict(1_000_000, 10_000), 505_000);
    assert_eq!(predict(0, 8_000_000), 1_000_000);
    let mut p = 0;
    for _ in 0..40 {
        p = predict(p, 5_000_000);
    }
    assert!(p > 4_900_000);
    p = predict(p, 20_000);
    p = predict(p, 20_000);
    p = predict(p, 20_000);
    assert!(p < 700_000, "three short wakes bring it below C3's residency: {p}");
    assert_eq!(predict(u64::MAX, u64::MAX), u64::MAX);
    assert_eq!(predict(u64::MAX, 0), u64::MAX - u64::MAX / 2, "no overflow");
}

#[test]
fn entry_steps_match_linux_io_idle_and_enter_bm() {
    let p = plan_of(&CpuIdle::ZEN4, &minipc_states());
    let hw = Recorder::default();
    assert_eq!(enter(&hw, &p, None), Entered::C1);
    assert_eq!(hw.take(), vec![Op::Halt]);
    assert_eq!(enter(&hw, &p, Some(7)), Entered::C1, "out of range: C1");
    assert_eq!(hw.take(), vec![Op::Halt]);
    assert_eq!(enter(&hw, &p, Some(0)), Entered::Deep(0));
    assert_eq!(hw.take(), vec![Op::In8(0x414), Op::Sti], "C2: no bus-master look, no dummy read on AMD");
    assert_eq!(enter(&hw, &p, Some(1)), Entered::Deep(1));
    assert_eq!(hw.take(), vec![Op::In16(PM1A), Op::In8(0x415), Op::Sti], "quiet bus: C3");
    hw.pm1.set(BM_STS | 1);
    assert_eq!(enter(&hw, &p, Some(1)), Entered::Demoted { from: 1, to: Some(0) });
    assert_eq!(hw.take(), vec![Op::In16(PM1A), Op::Out16(PM1A, BM_STS), Op::In8(0x414), Op::Sti], "busy: cleared, C2");
    assert_eq!(hw.pm1.get(), 1, "only BM_STS is written");
    // Only C3 listed and the bus busy: C1.
    let only_c3 = plan_of(&CpuIdle::ZEN4, &[io(3, 0x415, 350)]);
    hw.pm1.set(BM_STS);
    assert_eq!(enter(&hw, &only_c3, Some(0)), Entered::Demoted { from: 0, to: None });
    assert_eq!(hw.take(), vec![Op::In16(PM1A), Op::Out16(PM1A, BM_STS), Op::Halt]);
    // Intel: the PM timer read after the state; PM1b read and cleared too.
    let intel = CpuIdle { vendor: Vendor::Intel, family: 6, model: 0xC6, ..CpuIdle::ZEN4 };
    let s = minipc_states();
    let two = Firmware { pm1_status: [Some(PM1A), Some(0x804)], ..fw(&s) };
    let pi = plan(&intel, Some(&two), &mut all_ok);
    hw.pm1.set(BM_STS);
    assert_eq!(enter(&hw, &pi, Some(1)), Entered::Demoted { from: 1, to: Some(0) });
    assert_eq!(
        hw.take(),
        vec![Op::In16(PM1A), Op::In16(0x804), Op::Out16(PM1A, BM_STS), Op::Out16(0x804, BM_STS), Op::In8(0x414), Op::In32(0x808), Op::Sti]
    );
}

/// The measuring build's C1 mode and a CPU unlike the boot CPU: no port is touched, not even the timer queue is asked.
#[test]
fn c1_mode_and_c1_only_cpus_touch_no_port() {
    let p = plan_of(&CpuIdle::ZEN4, &minipc_states());
    let hw = Recorder::default();
    let c1 = Mode::C1;
    let asked = core::cell::Cell::new(false);
    let gap = || {
        asked.set(true);
        None
    };
    assert_eq!(step(&hw, Some(&p), c1, false, gap, u64::MAX, 4), Entered::C1);
    assert_eq!(hw.take(), vec![Op::Halt]);
    assert_eq!(step(&hw, Some(&p), Mode::Auto, true, gap, u64::MAX, 4), Entered::C1, "this CPU is C1 only");
    assert_eq!(hw.take(), vec![Op::Halt]);
    assert_eq!(step(&hw, None, Mode::Auto, false, gap, u64::MAX, 4), Entered::C1, "no plan yet");
    assert_eq!(hw.take(), vec![Op::Halt]);
    assert!(!asked.get(), "the timer queue is asked only when a deep state is possible");
    // The same quiet CPU in the normal mode: C3.
    let deep = Mode::Auto;
    assert_eq!(step(&hw, Some(&p), deep, false, || None, u64::MAX, 4), Entered::Deep(1));
    assert_eq!(hw.take(), vec![Op::In16(PM1A), Op::In8(0x415), Op::Sti]);
    // A busy timer queue (gap 0): C1.
    assert_eq!(step(&hw, Some(&p), deep, false, || Some(0), u64::MAX, 4), Entered::C1);
    assert_eq!(hw.take(), vec![Op::Halt]);
}

/// Fail closed: a refused C-state port sleeps in C1 instead of spinning; a refused PM1 read counts as bus
/// activity, so C3 is not entered unchecked.
#[test]
fn refused_ports_fail_closed() {
    let p = plan_of(&CpuIdle::ZEN4, &minipc_states());
    let hw = Recorder::default();
    hw.refuse.set(Some(0x414));
    assert_eq!(enter(&hw, &p, Some(0)), Entered::Refused(0));
    assert_eq!(hw.take(), vec![Op::Halt], "the refused read is replaced by sti; hlt");
    hw.refuse.set(None);
    assert_eq!(bm_word::<()>(Err(())), BM_STS);
    assert_eq!(bm_word::<()>(Ok(0)), 0);
    hw.pm1.set(bm_word::<()>(Err(())));
    assert_eq!(enter(&hw, &p, Some(1)), Entered::Demoted { from: 1, to: Some(0) });
}

/// The 265K's `_CST` as Linux shows it (cpuidle `desc`/`latency` of P and E cores alike, 5.11.2026): C1 by MWAIT 0x0,
/// C2 by MWAIT 0x21 (127 us), C3 by MWAIT 0x60 (1048 us).
fn arrow_lake_states() -> Vec<CState> {
    let ffh = |kind, hint, latency_us| CState { kind, entry: Entry::FixedHw(hint), latency_us };
    vec![ffh(1, 0x00, 1), ffh(2, 0x21, 127), ffh(3, 0x60, 1048)]
}

/// GP-CPU2 (replaces CP-265K's `arrow_lake_stays_in_c1_by_hlt`, whose subject this item changes): on the 265K the
/// plan holds the MWAIT states Linux's `intel_idle` takes, from an all-FFH `_CST`, else from the model's table when
/// `_CST` holds port states; no port is ever asked, and nothing is entered before the self-check.
#[test]
fn arrow_lake_takes_mwait_states_like_intel_idle() {
    let cpu = CpuIdle::ARROW_LAKE;
    for (states, src) in [(arrow_lake_states(), MwaitSource::Cst), (vec![io(1, 0x1814, 1), io(2, 0x1815, 127), io(3, 0x1816, 1048)], MwaitSource::Model)] {
        let mut asked = false;
        let p = plan(&cpu, Some(&fw(&states)), &mut |_, _| {
            asked = true;
            true
        });
        assert_eq!((p.c1_only, p.mwait_source), (None, Some(src)));
        let got: Vec<(u8, Option<u8>, u64)> = p.deep.iter().map(|d| (d.kind, d.mwait, d.latency_ns / 1000)).collect();
        assert_eq!(got, vec![(2, Some(0x21), 127), (3, Some(0x60), 1048)]);
        assert!(p.dummy_read.is_none() && p.bm_status == [None, None] && !asked && p.needs_check(), "no port asked: {p:?}");
        assert_eq!(usable(Some(&p), CheckState::Pending), None, "nothing deep before the check");
        assert_eq!(usable(Some(&p), CheckState::Failed), None);
        let hw = Recorder::default();
        assert_eq!(enter(&hw, &p, Some(1)), Entered::Deep(1));
        assert_eq!(hw.take(), vec![Op::Mwait(0x60), Op::Sti], "mwait with the hint, then sti");
        assert_eq!(enter(&hw, &p, None), Entered::C1);
        assert_eq!(hw.take(), vec![Op::Halt], "C1 stays sti; hlt");
    }
}

/// The FFH states alone (a non-hybrid Intel with the same table) are refused by name, and C1 remains.
#[test]
fn intel_ffh_states_are_refused_and_c1_remains() {
    let cpu = CpuIdle { hybrid: false, ..CpuIdle::ARROW_LAKE };
    let s = arrow_lake_states();
    let p = plan_of(&cpu, &s);
    assert_eq!(p.c1_only, Some(C1Only::NoUsableState));
    assert_eq!(p.refused.iter().map(|(c, why)| (c.kind, *why)).collect::<Vec<_>>(), vec![(2, Refused::Ffh), (3, Refused::Ffh)]);
}

/// A hypervisor says so first, and the hybrid bit changes nothing on AMD: the Mini-PC's plan is untouched.
#[test]
fn hybrid_reason_only_on_bare_metal_intel() {
    let s = minipc_states();
    assert_eq!(plan_of(&CpuIdle { hypervisor: true, ..CpuIdle::ARROW_LAKE }, &s).c1_only, Some(C1Only::Hypervisor));
    assert_eq!(plan_of(&CpuIdle { hybrid: true, ..CpuIdle::ZEN4 }, &s), plan_of(&CpuIdle::ZEN4, &s));
}
