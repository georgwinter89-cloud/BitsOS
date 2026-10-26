//! CP-5 kernel side (spec/core.md §7.2): one Core initcall (`cpu.timers`, after `cpu.smp`, before SC's
//! `sched.smp` by name order) that
//! 1. installs `sched::timer_tick` at the end of every LAPIC timer interrupt (`time::set_tick_hook`);
//! 2. starts every AP's LAPIC timer on that AP (`hal::smp::call_on` → `time::lapic::start_this_cpu`; the boot
//!    CPU's started in `acpi::irq::init`), logs each;
//! 3. installs `sched::irq_exit_preempt` at every interrupt exit from kernel mode
//!    (`hal::vectors::set_irq_exit_hook`), so a busy kernel thread no longer keeps its CPU from the others, once
//!    the scheduler says it is ready for it: `sched::timers::tick_preempts()` (SC returns `time::interrupt_driven()`
//!    there with change request 1 of plan/notes/cp-CP-5.md, together with the guard that keeps
//!    `run_exclusive` unpreempted). Until then nothing is preempted at an interrupt exit, as before CP-5;
//! 4. logs one summary.

use alloc::vec::Vec;

kreg::initcall!(kreg::Level::Core, "cpu.timers", start);
kreg::initcall!(kreg::Level::Core, "time.recheck", start_recheck);

/// INT-A1e: a kernel started by a Reload checks the rate it was handed against the HPET one second after the
/// calibration (`time::recheck_due_ns`). A short thread sleeps until then and finishes it, so the boot never
/// waits; nothing is spawned after a cold boot (the rate was just measured).
fn start_recheck() -> Result<(), &'static str> {
    if time::recheck_due_ns().is_none() {
        return Ok(());
    }
    sched::spawn("time.recheck", sched::Class::Normal, || {
        if let Some(due) = time::recheck_due_ns() {
            let now = time::now_ns();
            if due > now {
                sched::sleep_ns(due - now);
            }
        }
        let _ = time::finish_recheck();
    })
    .map(|_| ())
    .map_err(|_| "the re-check thread could not be spawned")
}

/// The cross-CPU call's bound per AP (calibration takes ~3 ms; generous under a hypervisor).
pub const START_NS: u64 = 2_000_000_000;

fn start_ap(_: u64) -> u64 {
    time::lapic::encode(time::lapic::start_this_cpu())
}

fn start() -> Result<(), &'static str> {
    let tick = time::set_tick_hook(sched::timer_tick);
    let n = hal::percpu::cpu_count();
    let mut failed: Vec<usize> = Vec::new();
    for cpu in 1..n {
        match hal::smp::call_on(cpu, start_ap, 0, time::now_ns, START_NS) {
            Ok(code) => {
                let r = time::lapic::decode(code);
                time::log_cpu_start(cpu, r);
                if r.is_err() {
                    failed.push(cpu);
                }
            }
            Err(e) => {
                klog::event!(Warn, "time", "lapic.timer_unreached", cpu = cpu, error = alloc::format!("{e:?}"); "this CPU's timers stay on the polled queue");
                failed.push(cpu);
            }
        }
    }
    // The handshake: SC says when preemption at the interrupt exit is safe (its own preemption-off sections).
    let preempt = sched::timers::tick_preempts();
    if preempt {
        hal::vectors::set_irq_exit_hook(Some(sched::irq_exit_preempt));
    } else {
        klog::event!(Info, "time", "preempt.waiting", interrupt_driven = time::interrupt_driven();
            "sched::timers::tick_preempts() is false: no preemption at interrupt exit yet (change request 1, plan/notes/cp-CP-5.md)");
    }
    let kinds: Vec<&str> = (0..n).filter_map(|c| time::lapic::cpu_stats(c).and_then(|s| s.kind)).map(|k| k.name()).collect();
    let mode = if kinds.iter().all(|k| *k == "tsc_deadline") && !kinds.is_empty() {
        "tsc_deadline"
    } else if kinds.iter().all(|k| *k == "oneshot") && !kinds.is_empty() {
        "oneshot"
    } else if kinds.is_empty() {
        "none"
    } else {
        "mixed"
    };
    klog::event!(Info, "time", "timers.ready", cpus = n, live = time::lapic::live_cpus(), mode = mode, interrupt_driven = time::interrupt_driven(),
        tick_hook = tick, preempt_hook = hal::vectors::irq_exit_hook_installed(), failed = alloc::format!("{failed:?}"));
    Ok(())
}
