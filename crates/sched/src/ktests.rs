//! SC-1 ktests: XSAVE layout and modes, FPU/AVX isolation between kernel threads and between two user
//! processes, the stack guard (probe; real overflow under QEMU), many threads created and reaped, IF of new
//! threads.

use crate::{spawn, stack_stats, thread_count, yield_now, Class, JoinHandle};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};
use hal::xsave::{self, FpuState, Mode, RegDump};
use ksync::{Completion, SpinLock};
use ktest::{kassert, ktest};

type R = ktest::TestResult;

ktest!("sc.xsave.layout", Safe, xsave_layout);
ktest!("sc.xsave.modes", Safe, xsave_modes);
ktest!("sc.fpu.kernel_threads", Safe, fpu_kernel_threads);
// sc.fpu.user_processes lives in crates/sched-mcp (it needs mm, which depends on sched).
ktest!("sc.stack.guard_probe", Safe, guard_probe);
ktest!("sc.stack.overflow", QemuOnly, stack_overflow);
ktest!("sc.threads.many", Safe, many_threads);
ktest!("sc.irq.new_threads", Safe, irq_new_threads);
ktest!("sc.sched.rollback", Safe, rollback);

const SEC: u64 = 1_000_000_000;

fn join_all(hs: &[JoinHandle], what: &str) -> R {
    for h in hs {
        kassert!(h.join_timeout(30 * SEC), "{what}: thread {} did not finish within 30 s", h.id());
    }
    Ok(())
}

fn first_error(errs: &SpinLock<Vec<String>>) -> R {
    match errs.lock().first() {
        Some(e) => Err(e.clone()),
        None => Ok(()),
    }
}

fn xsave_layout() -> R {
    let l = xsave::layout();
    let f = hal::cpu::facts();
    let v = hal::cpu::live();
    let want = xsave::choose_mode(f.cr4 & hal::cpu::CR4_OSXSAVE != 0, v.has("xsaveopt"), v.has("xsaves"), v.has("xsavec"));
    kassert!(l.mode == want, "mode {:?}, CPUID wants {want:?}", l.mode);
    kassert!(l.mode != Mode::None, "no XSAVE: R10 requires it");
    kassert!(l.mask == f.xcr0, "saved states {:#x}, XCR0 {:#x}", l.mask, f.xcr0);
    let cpuid = if l.mode == Mode::Xsaves { f.xsaves_size } else { f.xsave_size };
    kassert!(l.cpuid_size == cpuid, "CPUID size {} vs facts {cpuid}", l.cpuid_size);
    kassert!(l.size >= cpuid && l.size.is_multiple_of(64), "area {} bytes for CPUID {cpuid}", l.size);
    // CPUID and the per-component sum should agree; the larger one is used either way (the boot event logs both).
    kassert!(l.size >= l.computed_size, "area {} bytes, components need {}", l.size, l.computed_size);
    if l.mode == Mode::Xsaves {
        kassert!(hal::msr::read(hal::msr::IA32_XSS) == 0, "IA32_XSS not 0: supervisor states would enter the area");
    }
    let s = FpuState::initial().ok_or("no memory for an XSAVE area")?;
    kassert!(s.xstate_bv() == 3, "initial XSTATE_BV {:#x}", s.xstate_bv());
    let comp = if l.mode == Mode::Xsaves { 1 << 63 | l.mask } else { 0 };
    kassert!(s.xcomp_bv() == comp, "initial XCOMP_BV {:#x}, wanted {comp:#x}", s.xcomp_bv());
    Ok(())
}

/// Every mode this CPU has (hal's self-test on private areas): a save into a never-restored area (non-optimising
/// instruction) and one right after a restore (optimising instruction) give the registers back after a
/// clobber; the initial area gives a clean state (no registers of the previous user leak into a new thread).
fn xsave_modes() -> R {
    let mut tried = Vec::new();
    for mode in [Mode::Xsaves, Mode::Xsaveopt, Mode::Xsave] {
        if xsave::selftest_roundtrip(mode)? {
            tried.push(mode.name());
        }
    }
    kassert!(!tried.is_empty(), "no XSAVE mode usable");
    klog::event!(Info, "sched", "ktest.xsave_modes", modes = format!("{tried:?}"));
    Ok(())
}

/// Three kernel threads keep different vector/MXCSR/x87 states across 300 switches each, changing them
/// between switches: first all pinned to one CPU (they alternate there), then wandering over the CPUs (SC-3:
/// the state moves with the thread to whichever CPU restores it).
fn fpu_kernel_threads() -> R {
    let ncpu = crate::wait_cpus_online(5 * SEC).max(1);
    let home = crate::current_cpu();
    for wander in [false, true] {
        let errs = Arc::new(SpinLock::new(Vec::new()));
        let before = hal::switch::total_switches();
        let mut hs = Vec::new();
        for k in 0..3u64 {
            let errs = errs.clone();
            let opts = crate::SpawnOpts { cpu: Some(home), pin: !wander, rt_prio: crate::RT_PRIO_DEFAULT };
            hs.push(
                crate::spawn_with("sc-fpu", Class::Normal, opts, move || {
                    for i in 0..300u64 {
                        let p = RegDump::pattern(k << 32 | i | (wander as u64) << 40);
                        xsave::load_user_regs(&p);
                        if wander {
                            crate::pin_current(Some((k as usize + i as usize) % ncpu));
                        } else {
                            yield_now();
                        }
                        if let Some(d) = xsave::read_user_regs().diff(&p) {
                            errs.lock().push(format!("thread {k} round {i} (wander {wander}, CPU {}): {d}", crate::current_cpu()));
                            return;
                        }
                    }
                })
                .map_err(|e| format!("{e:?}"))?,
            );
        }
        join_all(&hs, "fpu threads")?;
        first_error(&errs)?;
        let switches = hal::switch::total_switches() - before;
        // 3 x 300 yields; the last thread left alone yields without a switch, so a little under 900.
        kassert!(switches >= 600, "only {switches} switches (wander {wander}): the threads did not alternate");
        if wander {
            let migrated: u64 = hs.iter().map(|h| h.thread().migrations()).sum();
            kassert!(ncpu < 2 || migrated >= 300, "the wandering threads moved only {migrated} times");
        }
    }
    Ok(())
}

/// Code of user process `k`: `1: vpaddq %ymm1,%ymm0,%ymm0; mov $0xC0DE000k,%eax; syscall; jmp 1b`.
/// The guard page below a new thread's stack is unmapped, the page above it mapped, and a load from it lands
/// in the #PF hook, which knows it as the running thread's guard.
fn guard_probe() -> R {
    let out = Arc::new(SpinLock::new(None));
    let o = out.clone();
    let h = spawn("sc-guard", Class::Normal, move || {
        let b = hal::switch::current_stack();
        let root = hal::paging::current_root();
        let r = if b.guard == 0 {
            Err(String::from("thread without stack bounds"))
        } else if hal::paging::translate(root, b.guard).is_some() {
            Err(format!("guard page {:#x} is mapped", b.guard))
        } else if hal::paging::translate(root, b.bottom).is_none() || hal::paging::translate(root, b.top - 8).is_none() {
            Err(String::from("stack not mapped"))
        } else {
            Ok((b, hal::switch::probe_current_guard()))
        };
        *o.lock() = Some(r);
    })
    .map_err(|e| format!("{e:?}"))?;
    join_all(core::slice::from_ref(&h), "guard probe")?;
    let r = out.lock().take().ok_or("no result")?;
    let (b, probe) = r?;
    kassert!(probe == Some(crate::guard::PROBE_MARK), "probe returned {probe:?}");
    kassert!(b.top - b.bottom == crate::stack::STACK_PAGES * 4096 && b.bottom - b.guard == crate::stack::GUARD_PAGES * 4096);
    Ok(())
}

/// A real overflow: hal's self-test recurses from a fresh thread until it runs into the guard page; the #DF hook
/// sees the hit and resumes the self-test instead of panicking. The thread then ends normally, the system goes
/// on and the stack comes back.
fn stack_overflow() -> R {
    if !hal::cpu::live().has("hypervisor") {
        return ktest::skip("resuming after a #DF is only done under a hypervisor");
    }
    let overflows = crate::guard::OVERFLOWS.get();
    let live = stack_stats().live;
    let out = Arc::new(SpinLock::new(None));
    let o = out.clone();
    let h = spawn("sc-overflow", Class::Normal, move || {
        let if_before = hal::cpu::interrupts_enabled();
        let depth = hal::switch::overflow_selftest();
        *o.lock() = Some((depth, if_before, hal::cpu::interrupts_enabled()));
    })
    .map_err(|e| format!("{e:?}"))?;
    join_all(core::slice::from_ref(&h), "overflow")?;
    drop(h);
    // SC-3: the stack comes back through the reaper (after every CPU flushed its TLB).
    crate::reap();
    let (depth, if_before, if_after) = out.lock().take().ok_or("the thread did not come back from the overflow")?;
    let d = depth.ok_or("overflow self-test refused (no guard or inside a switch)")?;
    kassert!(if_before == if_after, "IF {if_before} before the overflow, {if_after} after");
    kassert!(crate::guard::OVERFLOWS.get() == overflows + 1, "overflow not counted");
    kassert!(stack_stats().live <= live, "stack not freed: {} live, {live} before", stack_stats().live);
    kassert!(d > 50 && d < 200, "overflow after {d} frames of 0.5 KiB in a 64 KiB stack");
    klog::event!(Info, "sched", "ktest.overflow", depth = d);
    Ok(())
}

fn used_stack_bytes() -> u64 {
    mm_core::frames().map(|f| f.stats().used_by_kind[mm_core::Kind::Stack as usize]).unwrap_or(0)
}

/// More threads alive at once than one chunk of stack slots holds (R1: no count limit), all reaped; then many
/// short-lived threads reuse the slots without new address space.
fn many_threads() -> R {
    const ALIVE: usize = 600;
    const WAVES: usize = 10;
    const PER_WAVE: usize = 100;
    // Earlier tests' threads leave stacks for the reaper (SC-3): free them first so the counts start still.
    crate::reap();
    let before = stack_stats();
    let threads_before = thread_count();
    let bytes_before = used_stack_bytes();
    // INT-A1b: a driver may start a long-lived thread during the test (usb-msd-scan and the block worker of a
    // stick that attaches meanwhile under -smp 16); the counts below allow exactly the threads that are new
    // since here and not ours, with their stacks.
    let ids_before: Vec<u64> = crate::threads().iter().map(|t| t.0).collect();
    let foreign = || crate::threads().iter().filter(|t| !ids_before.contains(&t.0) && !t.1.starts_with("sc-")).count();
    let gate = Arc::new(Completion::new());
    let started = Arc::new(AtomicUsize::new(0));
    let finished = Arc::new(AtomicUsize::new(0));
    let mut hs = Vec::with_capacity(ALIVE);
    for i in 0..ALIVE {
        let (g, s, f) = (gate.clone(), started.clone(), finished.clone());
        hs.push(
            spawn("sc-many", Class::Normal, move || {
                s.fetch_add(1, Ordering::AcqRel);
                g.wait();
                f.fetch_add(1, Ordering::AcqRel);
            })
            .map_err(|e| format!("spawn {i}: {e:?}"))?,
        );
    }
    let t0 = time::now_ns();
    while started.load(Ordering::Acquire) < ALIVE && time::now_ns() - t0 < 30 * SEC {
        crate::sleep_ms(1);
    }
    kassert!(started.load(Ordering::Acquire) == ALIVE, "{} of {ALIVE} threads started", started.load(Ordering::Acquire));
    let peak = stack_stats();
    kassert!(peak.live >= before.live + ALIVE, "{} stacks live, {} expected", peak.live, before.live + ALIVE);
    kassert!(peak.chunks * crate::stack::SLOTS_PER_CHUNK as usize >= peak.live, "stacks outside the chunks");
    // Counted by name: earlier tests' threads may end meanwhile and a driver's may start (see `foreign`).
    kassert!(crate::threads().iter().filter(|t| t.1 == "sc-many").count() == ALIVE, "not all {ALIVE} sc-many threads are in the table");
    gate.complete();
    join_all(&hs, "alive wave")?;
    drop(hs);
    kassert!(crate::reap(), "the reaper did not free everything within 2 s");
    kassert!(finished.load(Ordering::Acquire) == ALIVE);
    // `<=`: threads of earlier tests may end meanwhile; none of ours may stay.
    let after = stack_stats();
    let extra = foreign();
    let extra_bytes = extra as u64 * crate::stack::STACK_PAGES * 4096;
    kassert!(after.live <= before.live + extra, "{} stacks live after reaping, {} before, {extra} new driver threads", after.live, before.live);
    kassert!(thread_count() <= threads_before + extra, "{} threads left, {threads_before} before, {extra} new driver threads", thread_count());
    kassert!(used_stack_bytes() <= bytes_before + extra_bytes, "stack frames not given back: {} -> {} ({extra} new driver threads)", bytes_before, used_stack_bytes());
    for w in 0..WAVES {
        let mut hs = Vec::with_capacity(PER_WAVE);
        for _ in 0..PER_WAVE {
            let f = finished.clone();
            hs.push(spawn("sc-short", Class::Background, move || {
                f.fetch_add(1, Ordering::AcqRel);
            }).map_err(|e| format!("wave {w}: {e:?}"))?);
        }
        join_all(&hs, "short wave")?;
        drop(hs);
        // Slots come back only through the reaper (SC-3); without it each wave would take fresh ones.
        crate::reap();
    }
    kassert!(finished.load(Ordering::Acquire) == ALIVE + WAVES * PER_WAVE);
    let end = stack_stats();
    kassert!(end.chunks == after.chunks, "short-lived threads took new address space ({} -> {} chunks)", after.chunks, end.chunks);
    let extra = foreign();
    kassert!(end.live <= before.live + extra && thread_count() <= threads_before + extra, "{} stacks / {} threads at the end, {} / {threads_before} before, {extra} new driver threads", end.live, thread_count(), before.live);
    klog::event!(Info, "sched", "ktest.many", alive = ALIVE, chunks = end.chunks, free_slots = end.free_slots);
    Ok(())
}

/// With interrupts on (CP-3), a new thread starts with IF=1, also when its spawner had them off; a thread that
/// turned them off keeps them off across switches while the others run with them on.
fn irq_new_threads() -> R {
    if !acpi::irq::enabled() {
        return ktest::skip("interrupts are not on (acpi::irq::init did not enable them)");
    }
    let seen = Arc::new(SpinLock::new(Vec::new()));
    let mut hs = Vec::new();
    let s = seen.clone();
    hs.push(spawn("sc-if", Class::Normal, move || s.lock().push(("plain", hal::cpu::interrupts_enabled(), true))).map_err(|e| format!("{e:?}"))?);
    {
        let _off = hal::cpu::IrqGuard::new();
        let s = seen.clone();
        hs.push(spawn("sc-if", Class::Normal, move || s.lock().push(("spawned with IF=0", hal::cpu::interrupts_enabled(), true))).map_err(|e| format!("{e:?}"))?);
    }
    let s = seen.clone();
    hs.push(
        spawn("sc-if", Class::Normal, move || {
            let off = hal::cpu::IrqGuard::new();
            for _ in 0..3 {
                yield_now();
                s.lock().push(("own IF=0 across a switch", hal::cpu::interrupts_enabled(), false));
            }
            drop(off);
            s.lock().push(("after the guard", hal::cpu::interrupts_enabled(), true));
        })
        .map_err(|e| format!("{e:?}"))?,
    );
    let s = seen.clone();
    hs.push(
        spawn("sc-if", Class::Normal, move || {
            for _ in 0..3 {
                yield_now();
                s.lock().push(("other thread meanwhile", hal::cpu::interrupts_enabled(), true));
            }
        })
        .map_err(|e| format!("{e:?}"))?,
    );
    join_all(&hs, "IF threads")?;
    let v = seen.lock();
    kassert!(v.len() == 9, "{} observations", v.len());
    for (what, got, want) in v.iter() {
        kassert!(got == want, "{what}: IF={got}, expected {want}");
    }
    Ok(())
}

/// A switch that hal refuses (here: a target that never started, so it has no saved stack pointer) is rolled
/// back: the target is runnable at the front of its queue again, the caller runs on and is in no run queue, for
/// a yield and for a block (which ends as a spurious wake-up). Both stay on this CPU (pinned) meanwhile.
fn rollback() -> R {
    use ksync::SchedHooks;
    let cpu = crate::current_cpu();
    let _pin = crate::pin_for_now(cpu).ok_or(format!("could not pin the test to CPU {cpu}"))?;
    let me = crate::current_id();
    let id = crate::NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let bad = Arc::new(crate::Thread::new(id, String::from("sc-unstarted"), Class::KernelRt, hal::switch::KernelContext::adopt_running(id, None), None, crate::NO_CPU));
    bad.rt_prio.store(crate::RT_PRIO_MAX, Ordering::Relaxed);
    bad.pin.store(cpu, Ordering::Relaxed);
    bad.cpu.store(cpu, Ordering::Relaxed);
    if crate::table::insert(bad.clone()).is_err() {
        return Err(String::from("no memory for the table"));
    }
    let cs = crate::cpus::of(cpu).ok_or("no CPU state")?;
    {
        let mut rq = cs.rq.lock();
        let pushed = rq.push_rt(crate::RT_PRIO_MAX, true, id, bad.clone()).is_ok();
        cs.nr_queued.store(rq.len(), Ordering::Release);
        if !pushed {
            drop(rq);
            drop(crate::table::remove(id));
            return Err(String::from("no memory for the run queue"));
        }
    }
    let check = |what: &str| -> R {
        let caller = crate::table::get(me).ok_or("caller not listed")?;
        let rq = cs.rq.lock();
        kassert!(rq.order().first() == Some(&id), "{what}: refused target not back at the front of its queue");
        kassert!(bad.state() == crate::State::Runnable, "{what}: refused target left in state {:?}", bad.state());
        kassert!(caller.state() == crate::State::Running, "{what}: caller in state {:?}", caller.state());
        kassert!(!rq.contains(me), "{what}: caller left in a run queue");
        Ok(())
    };
    let switches = hal::switch::switch_count_of(cpu);
    crate::EXPECTED_SWITCH_FAIL.store(id, Ordering::Relaxed);
    yield_now();
    let r1 = check("yield");
    crate::HOOKS.prepare_to_block();
    crate::HOOKS.block(Some(time::now_ns() + SEC));
    let r2 = check("block");
    let no_switch = hal::switch::switch_count_of(cpu) == switches;
    {
        let mut rq = cs.rq.lock();
        rq.remove_id(id);
        cs.nr_queued.store(rq.len(), Ordering::Release);
    }
    drop(crate::table::remove(id));
    crate::EXPECTED_SWITCH_FAIL.store(0, Ordering::Relaxed);
    r1?;
    r2?;
    kassert!(no_switch, "a switch happened although the only candidate was refused");
    Ok(())
}
