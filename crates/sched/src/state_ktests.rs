//! SC-3 ktests for state that travels with a thread or changes under it: the user GS base across a move to
//! another CPU (hal saves it at every switch), and a KernelRt thread that lowers its own priority below a
//! waiting one gives way at once.

use crate::{Class, SpawnOpts, current_cpu, spawn_with};
use alloc::format;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use hal::msr::{IA32_KERNEL_GS_BASE, read, write};
use ktest::{kassert, ktest};

type R = ktest::TestResult;

ktest!("sched.state.user_gs_migrates", Safe, user_gs_migrates);
ktest!("sched.class.rt_lowered", Safe, rt_lowered);

const SEC: u64 = 1_000_000_000;

fn online() -> Vec<usize> {
    (0..crate::cpus::count()).filter(|&c| crate::cpus::of(c).is_some_and(|s| s.online.load(Ordering::Acquire))).collect()
}

/// Threads with different user GS bases (IA32_KERNEL_GS_BASE in the kernel; never used there, so harmless)
/// share CPUs and move between them; each always finds its own value.
fn user_gs_migrates() -> R {
    crate::wait_cpus_online(5 * SEC);
    let cpus = online();
    let bad = Arc::new(AtomicU64::new(0));
    let mut hs = Vec::new();
    for k in 0..(2 * cpus.len()).clamp(2, 16) {
        let (cpus, bad) = (cpus.clone(), bad.clone());
        let mine = 0x0000_1000_0000_0000 + ((k as u64) << 20);
        let h = spawn_with("sched-gs", Class::Normal, SpawnOpts::default(), move || {
            write(IA32_KERNEL_GS_BASE, mine);
            for step in 0..6 {
                crate::pin_current(Some(cpus[(k + step) % cpus.len()]));
                crate::yield_now();
                crate::sleep_ms(1);
                if read(IA32_KERNEL_GS_BASE) != mine {
                    bad.fetch_add(1, Ordering::Relaxed);
                }
            }
            crate::pin_current(None);
            write(IA32_KERNEL_GS_BASE, 0);
        });
        hs.push(h.map_err(|e| format!("spawn: {e:?}"))?);
    }
    for h in &hs {
        kassert!(h.join_timeout(30 * SEC), "thread {} did not finish", h.id());
    }
    let moved: u64 = hs.iter().map(|h| h.thread().migrations()).sum();
    kassert!(bad.load(Ordering::Relaxed) == 0, "{} checks found another thread's GS base", bad.load(Ordering::Relaxed));
    kassert!(cpus.len() < 2 || moved > 0, "the threads never moved");
    Ok(())
}

/// A (KernelRt 50) starts B (KernelRt 40) on its own CPU: B waits. A lowers itself to 30: B runs before A goes on.
fn rt_lowered() -> R {
    let cpu = current_cpu();
    let done = Arc::new(AtomicBool::new(false));
    let seen = Arc::new(AtomicU64::new(u64::MAX));
    let (d, s) = (done.clone(), seen.clone());
    let a = spawn_with("sched-rt-a", Class::KernelRt, SpawnOpts { cpu: Some(cpu), pin: true, rt_prio: 50 }, move || {
        let d2 = d.clone();
        let b = spawn_with("sched-rt-b", Class::KernelRt, SpawnOpts { cpu: Some(current_cpu()), pin: true, rt_prio: 40 }, move || {
            d2.store(true, Ordering::Release);
        });
        let Ok(b) = b else { return };
        let before = u64::from(d.load(Ordering::Acquire));
        crate::set_rt_priority(30);
        let after = u64::from(d.load(Ordering::Acquire));
        s.store(before << 1 | after, Ordering::Release);
        b.join_timeout(5 * SEC);
    })
    .map_err(|e| format!("spawn: {e:?}"))?;
    kassert!(a.join_timeout(10 * SEC), "thread A did not finish");
    let v = seen.load(Ordering::Acquire);
    kassert!(v != u64::MAX, "thread A could not start B");
    kassert!(v >> 1 == 0, "B ran before A lowered its priority");
    kassert!(v & 1 == 1, "B did not run when A lowered its priority below it");
    Ok(())
}
