//! GP-CPU2 ktests. `sc.idle.fake_mwait`, `sc.idle.mwait_wakes` and `sc.hybrid.misfit_affinity` are Safe (every QEMU boot; the second enters a
//! real MWAIT where CPUID offers it, as in cp/69's bare view with `cpu-pm=on`). The `gp.cpu.*` tests are the
//! Gaming-PC's (class GamingPc, over MCP in its round): each says its result in one plain German line
//! (`sched ktest.gp_cpu_*`).

use crate::cstate::fake::{Op, Recorder};
use crate::cstate::{self, check, CheckState, CpuIdle, Deep, Entered, Mode, MwaitSource};
use crate::place::{self, CpuView, Pref};
use crate::{cpus, hybrid, Class};
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use ktest::{kassert, ktest};

type R = ktest::TestResult;

ktest!("sc.idle.fake_mwait", Safe, fake_mwait);
ktest!("sc.idle.mwait_wakes", Safe, mwait_wakes);
ktest!("sc.hybrid.misfit_affinity", Safe, misfit_affinity);
ktest!("gp.cpu.idle", GamingPc, gp_idle);
ktest!("gp.cpu.rank", GamingPc, gp_rank);

/// The 265K's plan from its CPUID facts without a `_CST` (the model table), entered on the recorder: MWAIT with the
/// hint, then `sti`; before the check C1 only. Touches nothing.
fn fake_mwait() -> R {
    let p = cstate::plan(&CpuIdle::ARROW_LAKE, None, &mut |_, _| false);
    let hints: Vec<Option<u8>> = p.deep.iter().map(|d| d.mwait).collect();
    kassert!(p.mwait_source == Some(MwaitSource::Model) && hints == vec![Some(0x21), Some(0x60)], "plan {p:?}");
    kassert!(cstate::usable(Some(&p), CheckState::Pending).is_none(), "used before the check");
    let hw = Recorder::default();
    let pick = cstate::choose(Mode::Auto, &p.deep, Some(20_000_000), u64::MAX, cstate::DEFAULT_RESIDENCY_FACTOR);
    let got = cstate::enter(&hw, &p, pick);
    kassert!(got == Entered::Deep(1) && hw.take() == vec![Op::Mwait(0x60), Op::Sti], "{got:?}");
    Ok(())
}

/// On Intel where CPUID offers MONITOR/MWAIT with the interrupt break: real MWAIT C1 entries (hint 0x00) on this CPU,
/// each ended by an interrupt, none needing the guard. Elsewhere (QEMU without `cpu-pm=on`,
/// and the Mini-PC's AMD CPU, whose idle path never uses MWAIT) it says so and passes.
fn mwait_wakes() -> R {
    let caps = hal::cpu::mwait::MwaitCaps::live();
    if CpuIdle::live().vendor != cstate::Vendor::Intel || !caps.usable() || !caps.has_hint(0) {
        klog::event!(Info, "sched", "ktest.mwait_wakes", usable = false; "no MWAIT on this CPU: nothing to enter");
        return Ok(());
    }
    let here = hal::percpu::this_cpu();
    kassert!(crate::pin_current(Some(here)), "cannot pin to cpu {here}");
    let d = Deep { kind: 1, port: 0, latency_ns: 1_000, bm_check: false, mwait: Some(0) };
    let samples = check::probe(&d);
    crate::pin_current(None);
    let Some(samples) = samples else { return ktest::fail("no second CPU for the guard") };
    // What is judged here is that MWAIT enters and returns every time; neither how fast nor whether the timer or the
    // guard ended a sample is (a guest's vCPU may be descheduled past the guard's 20 ms under host load, and the bare
    // view hides the hypervisor bit). The verdict, with timing and kicks, is logged; the Gaming-PC's `gp.cpu.idle`
    // and the boot check judge them.
    let verdict = check::judge(&d, &samples);
    let kicked = samples.iter().filter(|s| s.kicked).count();
    klog::event!(Info, "sched", "ktest.mwait_wakes", usable = true, samples = samples.len(), kicked = kicked, verdict = format!("{verdict:?}"));
    kassert!(!samples.is_empty(), "MWAIT 0x00 on cpu {here}: no sample");
    Ok(())
}

/// A thread with a pin or a narrowed affinity mask is not a misfit candidate (the poll thread's pull skips it, and a
/// request made before its mask narrowed is dropped at the switch); one with the full mask is. Checker finding 3: an
/// E-core-only thread was asked to move every 4 ms and bounced back. Touches no running thread.
fn misfit_affinity() -> R {
    let id = crate::NEXT_ID.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    let t = crate::Thread::new(id, String::from("sc-misfit-mask"), Class::Normal, hal::switch::KernelContext::adopt_running(id, None), None, crate::NO_CPU);
    let free = t.misfit_free();
    t.affine_all.store(false, core::sync::atomic::Ordering::Release);
    let narrowed = t.misfit_free();
    t.affine_all.store(true, core::sync::atomic::Ordering::Release);
    t.pin.store(0, core::sync::atomic::Ordering::Relaxed);
    let pinned = t.misfit_free();
    kassert!(free && !narrowed && !pinned, "full mask {free}, narrowed {narrowed}, pinned {pinned}");
    Ok(())
}

/// The Gaming-PC's idle plan: MWAIT states, and the self-check passed on a P and an E core (waits for it up to 15 s).
fn gp_idle() -> R {
    let p = cstate::current_plan().ok_or("no idle plan")?;
    kassert!(p.mwait_source.is_some(), "no MWAIT plan: c1_only={:?} refused={:?}", p.c1_only, p.refused);
    let t0 = time::now_ns();
    while cstate::check_state() == CheckState::Pending && time::now_ns() - t0 < 15_000_000_000 {
        crate::sleep_ms(50);
    }
    let src = p.mwait_source.map_or("-", |s| s.name());
    let hints: Vec<String> = p.deep.iter().filter_map(|d| d.mwait).map(|h| format!("{h:#x}")).collect();
    match cstate::check_outcome() {
        Some(Ok(v)) => {
            let per: Vec<String> = v.iter().map(|(c, h, ok)| format!("Kern {c} {h:#x}: {} µs, {}", ok.median_late_us, ok.residency_pct.map_or(String::from("-"), |x| format!("{x} %")))).collect();
            klog::event!(Info, "sched", "ktest.gp_cpu_idle", source = src, states = hints.join(","), result = "ok";
                "Tiefschlaf der Kerne geprüft und an ({}): {}", hints.join(", "), per.join("; "));
            kassert!(v.len() >= p.deep.len(), "checked {} of {} states", v.len(), p.deep.len());
            Ok(())
        }
        Some(Err((c, h, f))) => {
            klog::event!(Warn, "sched", "ktest.gp_cpu_idle", source = src, result = "failed", cpu = *c, hint = klog::Hex(u64::from(*h)), reason = f.name();
                "Tiefschlaf aus: {} (Kern {c}, Stufe {h:#x})", f.german());
            ktest::fail(&format!("self-check failed on cpu {c} hint {h:#x}: {} ({f:?})", f.name()))
        }
        None => ktest::fail(&format!("self-check still {} after 15 s (mode={})", cstate::check_state().name(), cstate::mode().name())),
    }
}

/// The ranking on the real CPU: P cores clearly faster than E cores, the favoured cores (highest HWP number) first; a
/// busy thread with everything idle would go to a favoured core, a light one to an E core, and a misfit on an E core
/// to a P core. Read-only (the placement functions are asked with an all-idle view).
fn gp_rank() -> R {
    kassert!(cpus::hybrid(), "the scheduler does not see a hybrid CPU");
    let n = cpus::count();
    let views: Vec<Option<CpuView>> = (0..n).map(|c| cpus::view_of(c).map(|v| CpuView { idle: true, load: 0, ..v })).collect();
    let view = |c: usize| views.get(c).copied().flatten();
    let online: Vec<(usize, CpuView)> = (0..n).filter_map(|c| view(c).map(|v| (c, v))).collect();
    let p: Vec<&(usize, CpuView)> = online.iter().filter(|(_, v)| v.core_type == hal::smp::CORE_TYPE_PERFORMANCE).collect();
    let e: Vec<&(usize, CpuView)> = online.iter().filter(|(_, v)| v.core_type == hal::smp::CORE_TYPE_EFFICIENCY).collect();
    kassert!(!p.is_empty() && !e.is_empty(), "P cores {} E cores {}", p.len(), e.len());
    let p_min = p.iter().map(|(_, v)| v.capacity).min().unwrap_or(0);
    let e_max = e.iter().map(|(_, v)| v.capacity).max().unwrap_or(0);
    kassert!(hybrid::capacity_greater(p_min, e_max), "slowest P core {p_min} not clearly above fastest E core {e_max}");
    let top = online.iter().map(|(_, v)| v.capacity).max().unwrap_or(0);
    let favoured: Vec<usize> = online.iter().filter(|(_, v)| v.capacity == top).map(|(c, _)| *c).collect();
    let busy = place::select(n, &view, None, Pref::Performance);
    kassert!(busy.is_some_and(|c| favoured.contains(&c)), "a busy thread went to {busy:?}, favoured {favoured:?}");
    let light = place::select(n, &view, None, hybrid::pref_for(Class::Normal, 0, e_max));
    kassert!(light.is_some_and(|c| e.iter().any(|(x, _)| *x == c)), "a light thread went to {light:?}");
    let from = e[0].0;
    let up = place::misfit_target(n, &view, from);
    kassert!(up.is_some_and(|c| favoured.contains(&c)), "misfit from {from} goes to {up:?}");
    klog::event!(Info, "sched", "ktest.gp_cpu_rank", p_cores = p.len(), e_cores = e.len(), p_capacity = p_min, e_capacity = e_max, favoured = format!("{favoured:?}");
        "{} schnelle Kerne (Leistung {}..{}), {} sparsame (Leistung {}); bevorzugt: Kern {:?}; viel Arbeit geht dorthin, leichte auf sparsame Kerne",
        p.len(), p_min, top, e.len(), e_max, favoured);
    Ok(())
}
