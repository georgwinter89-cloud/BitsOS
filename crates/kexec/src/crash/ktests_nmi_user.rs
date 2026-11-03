//! RB-12: NMI backtraces of every CPU while those CPUs run user code (the 26.10. crash: the MCP watchdog's
//! all-CPU backtrace while Brave played a video ended in a #DF on CPU 9, an endless #PF recursion on its NMI
//! stack). The cause of that crash and of the 27.10. one was found in CP-8, not a nested NMI: AMD's SYSRET left
//! programs with SS 0x18 (STAR base without RPL 3), an NMI from such a program saved it, and its `iretq` raised
//! #GP(0x18) in ring 0 with the program's GS base; the panic path then recursed on the NMI stack into #DF. QEMU
//! forces RPL 3, so this test cannot hit that; `cp.entry.nmi_user_ss` writes such frames. Worker threads, one per other CPU, each run a small program in an address space of their own: a deep
//! recursion on the user stack (1500 frames) and a syscall, in a loop, with a GS base that points nowhere, so
//! the CPUs cross the entry's swapgs windows (syscall, sysret, iret, interrupts from user mode) all the time.
//! Workers end and are started again (thread exit under the sampler). The test CPU meanwhile takes rounds of
//! NMI samples of every other CPU. QemuOnly: it runs user code on every CPU for seconds.

use alloc::format;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use hal::entry::{ReturnReason, UserContext};
use ktest::{TestResult, kassert, ktest};
use mm::{AddressSpace, Prot, VmaKind};

const MS: u64 = 1_000_000;
const SEC: u64 = 1_000_000_000;
const CODE: u64 = 0x40_0000;
const STACK_TOP: u64 = 0x7f00_0000_0000;
const STACK_LEN: u64 = 256 * 1024;
/// Recursion depth per loop (80 bytes a frame: 120 KiB of the 256 KiB stack).
const DEPTH: u32 = 1500;

/// `start: mov edi, DEPTH; call f; mov eax, 39 (getpid); syscall; jmp start`
/// `f: push rbp; mov rbp, rsp; sub rsp, 64; dec edi; jz 1f; call f; 1: leave; ret`
fn program() -> [u8; 38] {
    let d = DEPTH.to_le_bytes();
    [
        0xbf, d[0], d[1], d[2], d[3], // mov edi, DEPTH
        0xe8, 0x09, 0x00, 0x00, 0x00, // call f (+9 -> 19)
        0xb8, 0x27, 0x00, 0x00, 0x00, // mov eax, 39
        0x0f, 0x05, // syscall
        0xeb, 0xed, // jmp start (-19)
        0x55, // f: push rbp
        0x48, 0x89, 0xe5, // mov rbp, rsp
        0x48, 0x83, 0xec, 0x40, // sub rsp, 64
        0xff, 0xcf, // dec edi
        0x74, 0x05, // jz 1f
        0xe8, 0xef, 0xff, 0xff, 0xff, // call f (-17 -> 19)
        0xc9, // 1: leave
        0xc3, // ret
    ]
}

static RUN: AtomicBool = AtomicBool::new(false);
static SYSCALLS: AtomicU64 = AtomicU64::new(0);
static TRAPS: AtomicU64 = AtomicU64::new(0);
static LAST_TRAP: AtomicU64 = AtomicU64::new(0);
/// Workers that reached their user loop (INT-W18b: at -smp 2 the rounds could end before the one worker ran).
static ENTERED: AtomicU64 = AtomicU64::new(0);

/// One worker life: its own address space, the program until `RUN` drops or `rounds` syscalls passed.
fn worker(rounds: u64, gs: u64) {
    let Ok(asp) = AddressSpace::new() else { return };
    let rx = Prot { read: true, write: false, exec: true };
    let rw = Prot { read: true, write: true, exec: false };
    let code = program();
    if asp.map_fixed(CODE, 4096, rx, VmaKind::Image).is_err() || asp.write_loader(CODE, &code).is_err() {
        return;
    }
    if asp.map_fixed(STACK_TOP - STACK_LEN, STACK_LEN, rw, VmaKind::Stack).is_err() {
        return;
    }
    let home = hal::paging::home_root();
    asp.activate();
    let mut ctx = UserContext::new(CODE, STACK_TOP - 64);
    ctx.gs_base = gs;
    ctx.fs_base = gs;
    ENTERED.fetch_add(1, Ordering::AcqRel);
    let mut n = 0;
    while n < rounds && RUN.load(Ordering::Acquire) {
        match ctx.execute(true) {
            ReturnReason::Syscall => {
                ctx.set_syscall_return(1);
                SYSCALLS.fetch_add(1, Ordering::Relaxed);
            }
            ReturnReason::Trap { vector, .. } => {
                TRAPS.fetch_add(1, Ordering::Relaxed);
                LAST_TRAP.store(vector as u64, Ordering::Relaxed);
                break;
            }
        }
        n += 1;
    }
    hal::paging::activate(home);
    drop(asp);
}

fn sample(wait_ns: u64) -> Result<hal::nmi::Round, alloc::string::String> {
    let end = time::now_ns() + 5 * SEC;
    loop {
        match hal::nmi::sample_others(time::now_ns, wait_ns) {
            Ok(r) => return Ok(r),
            Err(hal::nmi::NmiError::Busy) if time::now_ns() < end => sched::sleep_ns(MS),
            Err(e) => return Err(format!("sample: {e:?}")),
        }
    }
}

fn nmi_user_stress() -> TestResult {
    let n = hal::percpu::cpu_count();
    if n < 2 {
        return ktest::skip("one CPU: nothing to sample");
    }
    if !hal::nmi::prepared() {
        return Err("the NMI slots were not prepared at boot".into());
    }
    let rounds: u32 = crate::boot::config("rb_nmi_user_rounds").and_then(|s| s.parse().ok()).unwrap_or(400);
    let me = hal::percpu::this_cpu();
    RUN.store(true, Ordering::Release);
    let (s0, t0) = (SYSCALLS.load(Ordering::Relaxed), TRAPS.load(Ordering::Relaxed));
    let e0 = ENTERED.load(Ordering::Acquire);
    let mut workers: Vec<Option<sched::JoinHandle>> = (0..n).map(|_| None).collect();
    let mut started = 0u64;
    let mut frames = 0u64;
    let mut user_hits = 0u64;
    let res = (|| -> TestResult {
        for round in 0..rounds {
            // Keep one worker on every other CPU; a finished one (thread exit) is started again.
            for (c, w) in workers.iter_mut().enumerate() {
                if c == me || w.as_ref().is_some_and(|h| !h.is_finished()) {
                    continue;
                }
                let life = 50 + (round as u64 % 7) * 40;
                let gs = if (started & 1) == 0 { 0 } else { 0x0000_7e00_dead_0000 + (c as u64) * 0x1000 };
                let o = sched::SpawnOpts { cpu: Some(c), pin: true, ..sched::SpawnOpts::default() };
                *w = Some(sched::spawn_with("rb12-user", sched::Class::Normal, o, move || worker(life, gs)).map_err(|e| format!("spawn: {e:?}"))?);
                started += 1;
            }
            if round == 0 {
                // With few CPUs the rounds take microseconds; let the first workers reach user mode first.
                let end = time::now_ns() + 5 * SEC;
                while ENTERED.load(Ordering::Acquire) < e0 + started {
                    kassert!(time::now_ns() < end, "only {} of {started} workers started within 5 s", ENTERED.load(Ordering::Acquire) - e0);
                    sched::sleep_ns(MS);
                }
            }
            let r = sample(2 * SEC)?;
            kassert!(r.send_failed == 0, "round {round}: {r:?}");
            for c in (0..n).filter(|&c| c != me) {
                if let Some(regs) = hal::nmi::regs(c) {
                    kassert!(regs.nsteps <= hal::nmi::DEPTH, "CPU {c}: {} steps", regs.nsteps);
                    frames += regs.nsteps as u64;
                    if regs.nsteps == 0 {
                        user_hits += 1;
                    }
                }
            }
        }
        Ok(())
    })();
    // INT-W20: on a loaded host the one worker of -smp 2 sometimes had not reached user mode in all the rounds
    // (0 syscalls, 0 user samples); it gets a bounded 5 s more before the check below.
    let end = time::now_ns() + 5 * SEC;
    while res.is_ok() && SYSCALLS.load(Ordering::Relaxed) - s0 < n as u64 - 1 && time::now_ns() < end {
        sched::sleep_ns(MS);
    }
    RUN.store(false, Ordering::Release);
    for h in workers.iter().flatten() {
        h.join_timeout(10 * SEC);
    }
    res?;
    let calls = SYSCALLS.load(Ordering::Relaxed) - s0;
    let traps = TRAPS.load(Ordering::Relaxed) - t0;
    klog::event!(Info, "crash", "ktest.nmi_user", rounds = rounds, workers = started, syscalls = calls, traps = traps, frames = frames, user_samples = user_hits, cpus = n);
    kassert!(traps == 0, "{traps} user traps (last vector {})", LAST_TRAP.load(Ordering::Relaxed));
    // At least one syscall per worker CPU (INT-W18: `calls > rounds` failed at the gate's -smp 2, where the
    // one worker made 87 calls in the 400 short rounds; it ran, it was only alone).
    kassert!(calls >= (n as u64 - 1), "only {calls} syscalls on {} worker CPUs: the workers did not run", n - 1);
    Ok(())
}
ktest!("rb.crash.nmi_user_stress", QemuOnly, nmi_user_stress);

fn bp_hook(f: &mut hal::idt::TrapFrame) -> bool {
    // Only the test's int3 in kernel mode (a trap: the CPU resumes after it).
    f.vector == 3 && f.cs & 3 == 0
}

/// CONFIG `rb_nmi_nest=on` (it may end the boot): one CPU's NMI handler takes a breakpoint trap (iret inside the
/// NMI, which ends NMI blocking), then this CPU samples again (a second NMI of ours, as the
/// watchdog's next round sends it). Before RB-12 that NMI restarted at the top
/// of the NMI stack over the first one's saved state and the CPU went wild; with RB-12 the nested NMI is caught
/// (`nmi.nested` panic naming the interrupted rip). Survives only once the NMI entry is nesting-safe (CR-1).
fn nmi_nested() -> TestResult {
    if crate::boot::config("rb_nmi_nest").as_deref() != Some("on") {
        return ktest::skip("CONFIG rb_nmi_nest=on not set (the nested NMI may end the boot)");
    }
    let n = hal::percpu::cpu_count();
    if n < 2 || !hal::nmi::prepared() {
        return ktest::skip("needs a second CPU and the NMI slots");
    }
    let me = hal::percpu::this_cpu();
    let target = (0..n).find(|&c| c != me).ok_or("no other CPU")?;
    let prev = hal::idt::exception_hook(3);
    hal::idt::set_exception_hook(3, bp_hook);
    hal::nmi::nest_test::arm(target);
    let r = sample(2 * SEC);
    let end = time::now_ns() + SEC;
    while hal::nmi::nest_test::stage() != 2 && time::now_ns() < end {
        core::hint::spin_loop();
    }
    let stage = hal::nmi::nest_test::stage();
    // The second NMI is one of ours, as the watchdog's next sample round (or its retry) would send it.
    let second = if stage == 2 { Some(sample(2 * SEC)) } else { None };
    hal::nmi::nest_test::release();
    match prev {
        Some(h) => hal::idt::set_exception_hook(3, h),
        None => hal::idt::clear_exception_hook(3),
    }
    r?;
    if let Some(r2) = second {
        r2?;
    }
    // Whether the second NMI really came while the first one still ran (its interrupted rsp on the NMI stack).
    let ist = hal::gdt::ist_stacks_of(target).and_then(|s| s.into_iter().find(|s| s.ist == hal::gdt::IST_NMI));
    let nested = hal::nmi::regs(target).zip(ist).is_some_and(|(g, s)| g.frame.cs & 3 == 0 && s.contains(g.frame.rsp));
    klog::event!(Info, "crash", "ktest.nmi_nested.second", target = target, nested = nested);
    kassert!(stage == 2, "the target's NMI handler never took the breakpoint (stage {stage})");
    sched::sleep_ns(50 * MS);
    let before = hal::nmi::answers(target);
    let r = sample(2 * SEC)?;
    kassert!(hal::nmi::answers(target) > before, "CPU {target} did not answer after the nested NMI: {r:?}");
    // Answering NMIs is not enough: a CPU caught in a loop on its NMI stack answers them too. It must run
    // ordinary code again (interrupts on, its scheduler alive).
    let call = hal::smp::call_on(target, |x| x + 1, 41, time::now_ns, SEC);
    kassert!(call == Ok(42), "CPU {target} does not take interrupts after the nested NMI: {call:?}");
    klog::event!(Info, "crash", "ktest.nmi_nested", target = target, survived = true);
    Ok(())
}
ktest!("rb.crash.nmi_nested", QemuOnly, nmi_nested);
