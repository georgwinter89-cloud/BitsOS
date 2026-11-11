//! SC-9 ktests of the idle depth. Under QEMU the real plan is C1 only (the hypervisor guard, as designed), so
//! the deep path runs through a fake `_CST` table: the Mini-PC's states built as AML, parsed by `acpi::cst`,
//! planned with the Mini-PC's CPU facts through the live register guard, entered on a recording machine.
//! All Safe: they touch no port and leave the mode as they found it (a guard puts it back on every exit).

use crate::cstate::fake::{Op, Recorder};
use crate::cstate::{self, C1Only, CpuIdle, Entered, Firmware, Mode, BM_STS};
use acpi::cst::build;
use alloc::string::String;
use alloc::vec;
use ktest::{kassert, ktest};

type R = ktest::TestResult;

ktest!("sc.idle.fake_cst", Safe, fake_cst);
ktest!("sc.idle.live_plan", Safe, live_plan);
ktest!("sc.idle.config", Safe, config);
ktest!("sc.idle.next_deadline", Safe, next_deadline);

const PM1A: u16 = 0x800;

fn guard(port: u16, bytes: u32) -> bool {
    acpi::live::check_io(port as u64, bytes, false).is_ok()
}

fn fake_cst() -> R {
    let table = build::table(&build::name(&build::MINIPC));
    let cst = acpi::cst::find(&table);
    kassert!(cst.as_ref().is_some_and(|c| c.states.len() == 3 && c.skipped.is_empty()), "fake _CST parsed: {cst:?}");
    let cst = cst.unwrap_or_default();
    let fw = Firmware { states: &cst.states, pm_timer: Some(0x808), pm1_status: [Some(PM1A), None] };
    let p = cstate::plan(&CpuIdle::ZEN4, Some(&fw), &mut guard);
    kassert!(p.c1_only.is_none() && p.refused.is_empty(), "plan: {p:?}");
    let ports: vec::Vec<(u8, u16, bool)> = p.deep.iter().map(|d| (d.kind, d.port, d.bm_check)).collect();
    kassert!(ports == vec![(2, 0x414, false), (3, 0x415, true)], "deep states {ports:?}");
    kassert!(p.bm_status == [Some(PM1A), None] && p.dummy_read.is_none(), "bm {:?} dummy {:?}", p.bm_status, p.dummy_read);
    // The choice the idle loop makes for a quiet CPU with its next timer in 10 ms: C3.
    let pick = cstate::choose(Mode::Auto, &p.deep, Some(10_000_000), u64::MAX, cstate::DEFAULT_RESIDENCY_FACTOR);
    kassert!(pick == Some(1), "pick {pick:?}");
    let hw = Recorder::default();
    let got = cstate::enter(&hw, &p, pick);
    kassert!(got == Entered::Deep(1) && hw.take() == vec![Op::In16(PM1A), Op::In8(0x415), Op::Sti], "quiet bus: {got:?}");
    hw.pm1.set(BM_STS);
    let got = cstate::enter(&hw, &p, pick);
    let ops = hw.take();
    kassert!(
        got == (Entered::Demoted { from: 1, to: Some(0) }) && ops == vec![Op::In16(PM1A), Op::Out16(PM1A, BM_STS), Op::In8(0x414), Op::Sti],
        "busy bus: {got:?} {ops:?}"
    );
    // The same table on a CPU without ARAT, and under a hypervisor: C1 only, no port asked.
    for (cpu, why) in [(CpuIdle { arat: false, ..CpuIdle::ZEN4 }, C1Only::NoArat), (CpuIdle { hypervisor: true, ..CpuIdle::ZEN4 }, C1Only::Hypervisor)] {
        let q = cstate::plan(&cpu, Some(&fw), &mut |_, _| false);
        kassert!(q.deep.is_empty() && q.c1_only == Some(why), "{cpu:?}: {q:?}");
    }
    Ok(())
}

/// The plan `init` made for this machine: C1 only under a hypervisor; on bare metal either deep states
/// (rising latency, every port allowed by the guard) or a named reason.
fn live_plan() -> R {
    let Some(p) = cstate::current_plan() else { return Err(String::from("no idle plan after sched init")) };
    let cpu = CpuIdle::live();
    if cpu.hypervisor {
        kassert!(p.c1_only == Some(C1Only::Hypervisor) && p.deep.is_empty(), "under a hypervisor: {p:?}");
        return Ok(());
    }
    kassert!(p.deep.is_empty() == p.c1_only.is_some(), "deep states and a C1-only reason exclude each other: {p:?}");
    kassert!(p.c1_only != Some(C1Only::Hypervisor), "bare metal planned as a guest");
    kassert!(p.deep.windows(2).all(|w| w[0].latency_ns <= w[1].latency_ns), "latencies rise: {:?}", p.deep);
    for d in &p.deep {
        kassert!(guard(d.port, 1), "port {:#x} not allowed by the guard", d.port);
        kassert!(!d.bm_check || p.bm_status[0].is_some(), "C3 without a bus-master port");
    }
    Ok(())
}

/// Puts the idle mode back however the test leaves (an early `Err` included), so a rerun and the other CPUs
/// find it as before.
struct ModeBack(Mode);

impl Drop for ModeBack {
    fn drop(&mut self) {
        cstate::set_mode(self.0);
    }
}

/// CONFIG `idle=`: c1 switches the choice off (no port on the fake Mini-PC plan), deep (or nothing) back on,
/// an unknown value is a Warn and keeps deep.
fn config() -> R {
    let _back = ModeBack(cstate::mode());
    let get = |v: &'static str| move |k: &str| (k == "idle").then(|| String::from(v));
    let since = klog::next_seq();
    cstate::configure(&get("foo"));
    let foo = cstate::mode();
    let warned = klog::read_since(since, 4096)
        .iter()
        .any(|r| r.level == klog::Level::Warn && r.name == "config.unknown" && r.text.contains("foo"));
    cstate::configure(&get("c1"));
    let c1 = cstate::mode();
    cstate::configure(&get("deep"));
    let deep = cstate::mode();
    cstate::configure(&|_: &str| None);
    let none = cstate::mode();
    kassert!(foo == Mode::Auto && warned, "idle=foo: mode {foo:?}, warned {warned}");
    kassert!(c1 == Mode::C1 && deep == Mode::Auto && none == Mode::Auto, "c1 {c1:?} deep {deep:?} none {none:?}");
    // What the modes do on the fake Mini-PC plan for a quiet CPU: c1 touches no port, deep goes to C3.
    let table = build::table(&build::name(&build::MINIPC));
    let states = acpi::cst::find(&table).map(|c| c.states).unwrap_or_default();
    let fw = Firmware { states: &states, pm_timer: Some(0x808), pm1_status: [Some(PM1A), None] };
    let p = cstate::plan(&CpuIdle::ZEN4, Some(&fw), &mut guard);
    let hw = Recorder::default();
    let got = cstate::step(&hw, Some(&p), c1, false, || None, u64::MAX, cstate::DEFAULT_RESIDENCY_FACTOR);
    let ops = hw.take();
    kassert!(got == Entered::C1 && ops == vec![Op::Halt], "idle=c1: {got:?} {ops:?}");
    let got = cstate::step(&hw, Some(&p), deep, false, || None, u64::MAX, cstate::DEFAULT_RESIDENCY_FACTOR);
    let ops = hw.take();
    kassert!(got == Entered::Deep(1) && ops == vec![Op::In16(PM1A), Op::In8(0x415), Op::Sti], "idle=deep: {got:?} {ops:?}");
    // C1 mode on this CPU: the idle loop keeps working (a short sleep wakes again).
    cstate::set_mode(Mode::C1);
    crate::sleep_ns(2_000_000);
    Ok(())
}

fn nop(_: usize) {}

/// The idle loop's input `time::hrtimer::next_deadline_on`: a timer armed on a CPU shows up there (its
/// deadline or an earlier one), is gone after the cancel, and a CPU index past the last has no queue.
fn next_deadline() -> R {
    let t = time::HrTimer::new(nop, 0);
    // An odd deadline an hour ahead: no other timer has exactly it.
    let want = time::now_ns().saturating_add(3_600_000_000_000 + 12_345);
    t.arm(want);
    let Some(cpu) = t.cpu() else {
        t.cancel();
        return ktest::fail("the timer went to the polled queue (no LAPIC timer here)");
    };
    // `None` only while the queue is busy this instant: a bounded retry.
    let ask = || (0..1000).find_map(|_| time::hrtimer::next_deadline_on(cpu));
    let armed = ask();
    t.cancel();
    let after = ask();
    kassert!(matches!(armed, Some(Some(d)) if d <= want), "armed on CPU {cpu}: {armed:?}, want <= {want}");
    kassert!(matches!(after, Some(d) if d != Some(want)), "after the cancel: {after:?}");
    let past = hal::percpu::cpu_count() + 1;
    kassert!(time::hrtimer::next_deadline_on(past).is_none(), "CPU {past} has no queue");
    Ok(())
}
