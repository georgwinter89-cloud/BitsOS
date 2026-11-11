//! CP-8 ktests: NMIs that arrive while user code runs, with the frame an AMD CPU pushes after SYSRET.
//!
//! The 27.10. Mini-PC crash: AMD's SYSRET loads SS with `STAR[63:48] + 8` and does not force RPL 3 as Intel (and
//! QEMU) do, so after a `sysretq` the program runs with SS = 0x18. An NMI then saves SS = 0x18, and returning
//! to the program by `iretq` with that frame raises #GP(0x18) (SS.RPL != CS.RPL) in ring 0 with the program's GS
//! base. QEMU never builds such a frame, so `cp.entry.nmi_user_ss` has hal write it (`idt::test_amd_sysret_ss`:
//! the saved SS of every NMI from user mode loses its RPL before any hook runs), while workers on every other CPU loop through
//! syscalls (returns by sysret and by iret alternate) and the test CPU takes NMI sample rounds.

use alloc::format;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use hal::entry::{ReturnReason, UserContext};
use hal::idt::{self, SavedHook, TrapFrame};
use ktest::{TestResult, kassert, ktest};
use mm::{AddressSpace, Prot, VmaKind};

ktest!("cp.entry.nmi_user_ss", QemuOnly, nmi_user_ss);
ktest!("cp.trap.foreign_gs", Safe, foreign_gs);
ktest!("cp.trap.user_gs_window", Safe, user_gs_window);
ktest!("cp.entry.sysret_ss", Safe, sysret_ss);

const MS: u64 = 1_000_000;
const SEC: u64 = 1_000_000_000;
const CODE: u64 = 0x40_0000;
const STACK_TOP: u64 = 0x7f00_0000_0000;
const STACK_LEN: u64 = 64 * 1024;
const ROUNDS: u32 = 300;

/// `start: mov ecx, 2000; 1: dec ecx; jnz 1b; mov eax, 39 (getpid); syscall; jmp start`: mostly user time,
/// so the NMIs land in user mode, with a syscall (and its return) every few microseconds.
const PROGRAM: [u8; 18] = [
    0xb9, 0xd0, 0x07, 0x00, 0x00, // mov ecx, 2000
    0xff, 0xc9, // dec ecx
    0x75, 0xfc, // jnz -4
    0xb8, 0x27, 0x00, 0x00, 0x00, // mov eax, 39
    0x0f, 0x05, // syscall
    0xeb, 0xee, // jmp start (-18)
];

static RUN: AtomicBool = AtomicBool::new(false);
static SYSRETS: AtomicU64 = AtomicU64::new(0);
static IRETS: AtomicU64 = AtomicU64::new(0);
static TRAPS: AtomicU64 = AtomicU64::new(0);
static LAST_TRAP: AtomicU64 = AtomicU64::new(0);
/// Workers that reached their user loop (INT-W18b: at -smp 2 the 300 rounds ended before the one worker ran).
static STARTED: AtomicU64 = AtomicU64::new(0);

fn worker(cpu: usize, lives: u64) {
    let Ok(asp) = AddressSpace::new() else { return };
    let rx = Prot { read: true, write: false, exec: true };
    let rw = Prot { read: true, write: true, exec: false };
    if asp.map_fixed(CODE, 4096, rx, VmaKind::Image).is_err() || asp.write_loader(CODE, &PROGRAM).is_err() {
        return;
    }
    if asp.map_fixed(STACK_TOP - STACK_LEN, STACK_LEN, rw, VmaKind::Stack).is_err() {
        return;
    }
    let home = hal::paging::home_root();
    asp.activate();
    let mut ctx = UserContext::new(CODE, STACK_TOP - 64);
    // A GS base of 0 (as Brave's threads had), one in the lower half and one in the kernel half (a program may
    // wrgsbase any canonical value), so no check may take the GS base as evidence of the mode.
    let gs = match lives % 3 {
        0 => 0,
        1 => 0x0000_7e00_dead_0000 + (cpu as u64) * 0x1000,
        _ => 0xffff_c000_dead_0000 + (cpu as u64) * 0x1000,
    };
    ctx.gs_base = gs;
    STARTED.fetch_add(1, Ordering::AcqRel);
    let mut n = 0u64;
    while RUN.load(Ordering::Acquire) && n < 20_000 {
        match ctx.execute(true) {
            ReturnReason::Syscall => {
                ctx.set_syscall_return(cpu as i64);
                // Every other return by iret (rcx != rip), as after a signal frame or ptrace.
                if n & 1 == 1 {
                    ctx.regs.rcx = 0;
                }
            }
            ReturnReason::Trap { vector, .. } => {
                TRAPS.fetch_add(1, Ordering::Relaxed);
                LAST_TRAP.store(vector as u64, Ordering::Relaxed);
                break;
            }
        }
        n += 1;
    }
    SYSRETS.fetch_add(ctx.stats.sysret, Ordering::Relaxed);
    IRETS.fetch_add(ctx.stats.iret, Ordering::Relaxed);
    hal::paging::activate(home);
    drop(asp);
}

fn sample() -> Result<hal::nmi::Round, alloc::string::String> {
    let end = time::now_ns() + 5 * SEC;
    loop {
        match hal::nmi::sample_others(time::now_ns, 2 * SEC) {
            Ok(r) => return Ok(r),
            Err(hal::nmi::NmiError::Busy) if time::now_ns() < end => sched::sleep_ns(MS),
            Err(e) => return Err(format!("sample: {e:?}")),
        }
    }
}

fn nmi_user_ss() -> TestResult {
    let n = hal::percpu::cpu_count();
    if n < 2 {
        return ktest::fail("one CPU: nothing to sample");
    }
    if !hal::nmi::prepared() {
        return Err("the NMI slots were not prepared at boot".into());
    }
    let me = hal::percpu::this_cpu();
    let (t0, s0, i0, x0) = (TRAPS.load(Ordering::Relaxed), SYSRETS.load(Ordering::Relaxed), IRETS.load(Ordering::Relaxed), idt::test_amd_sysret_ss_frames());
    let f0 = idt::user_frames_fixed();
    let started0 = STARTED.load(Ordering::Acquire);
    idt::test_amd_sysret_ss(true);
    RUN.store(true, Ordering::Release);
    let mut workers: Vec<Option<sched::JoinHandle>> = (0..n).map(|_| None).collect();
    let mut lives = 0u64;
    let mut user_samples = 0u64;
    let res = (|| -> TestResult {
        for round in 0..ROUNDS {
            for (c, w) in workers.iter_mut().enumerate() {
                if c == me || w.as_ref().is_some_and(|h| !h.is_finished()) {
                    continue;
                }
                let o = sched::SpawnOpts { cpu: Some(c), pin: true, ..sched::SpawnOpts::default() };
                let l = lives;
                *w = Some(sched::spawn_with("cp8-user", sched::Class::Normal, o, move || worker(c, l)).map_err(|e| format!("spawn: {e:?}"))?);
                lives += 1;
            }
            if round == 0 {
                // The rounds take microseconds with few CPUs; let every first worker reach user mode first.
                let end = time::now_ns() + 5 * SEC;
                while STARTED.load(Ordering::Acquire) < started0 + lives {
                    kassert!(time::now_ns() < end, "only {} of {lives} workers started within 5 s", STARTED.load(Ordering::Acquire) - started0);
                    sched::sleep_ns(MS);
                }
            }
            let r = sample()?;
            kassert!(r.send_failed == 0, "round {round}: {r:?}");
            user_samples += (0..n).filter(|&c| c != me).filter(|&c| hal::nmi::regs(c).is_some_and(|g| g.user)).count() as u64;
        }
        Ok(())
    })();
    idt::test_amd_sysret_ss(false);
    RUN.store(false, Ordering::Release);
    for h in workers.iter().flatten() {
        h.join_timeout(10 * SEC);
    }
    res?;
    let traps = TRAPS.load(Ordering::Relaxed) - t0;
    let (sysrets, irets) = (SYSRETS.load(Ordering::Relaxed) - s0, IRETS.load(Ordering::Relaxed) - i0);
    let tampered = idt::test_amd_sysret_ss_frames() - x0;
    let fixed = idt::user_frames_fixed() - f0;
    klog::event!(Info, "cp", "ktest.nmi_user_ss", rounds = ROUNDS, workers = lives, user_samples = user_samples, tampered = tampered,
        fixed = fixed, sysrets = sysrets, irets = irets, traps = traps, cpus = n);
    kassert!(fixed >= tampered, "{tampered} NMI frames had SS 0x18 but only {fixed} were repaired");
    kassert!(traps == 0, "{traps} user traps (last vector {})", LAST_TRAP.load(Ordering::Relaxed));
    kassert!(tampered > 0 && user_samples > 0, "no NMI came from user mode ({tampered} frames, {user_samples} samples)");
    kassert!(sysrets > 0 && irets > 0, "both return paths must run: sysret {sysrets}, iret {irets}");
    Ok(())
}

// ---------------------------------------------------------------------------------------------------------
// CR-2 of RB-12: a #GP taken in kernel mode while the GS base is not this CPU's block (as the 27.10. #GP at the
// NMI's iretq, after the GS base went back to the program's) runs its hook with the kernel GS, and the foreign
// base is back when the hook resumes the code. Before CP-8 the panic path read gs:[..] with that base, took a #PF,
// whose hook read it again, until the stack ran into its guard page (#DF).

static GS_SEEN: AtomicU64 = AtomicU64::new(0);
static GS_WANT: AtomicU64 = AtomicU64::new(0);
static GS_HITS: AtomicU64 = AtomicU64::new(0);
/// Lock-free: the hook runs in exception context, and any other CPU's kernel #GP passes through it meanwhile.
static PREV_GP: SavedHook = SavedHook::new();

/// Uses no GS itself: under the old entry it would have run with the foreign base and only recorded it.
fn foreign_gs_hook(f: &mut TrapFrame) -> bool {
    let site = idt::probe_site();
    if f.rip == site.fault_rip {
        GS_SEEN.store(hal::msr::read(hal::msr::IA32_GS_BASE), Ordering::Relaxed);
        GS_HITS.fetch_add(1, Ordering::AcqRel);
        f.rip = site.resume_rip;
        f.rax = GS_WANT.load(Ordering::Relaxed);
        return true;
    }
    if idt::probe_iret_repair(f) {
        GS_SEEN.store(hal::msr::read(hal::msr::IA32_GS_BASE), Ordering::Relaxed);
        GS_HITS.fetch_add(1, Ordering::AcqRel);
        return true;
    }
    PREV_GP.call(f)
}

fn foreign_gs() -> TestResult {
    if !hal::entry::user_fsgsbase() {
        return ktest::fail("no FSGSBASE: the probe switches the GS base with wrgsbase");
    }
    if !idt::gs_base_check_armed() {
        return Err("the lower-half GS check was never armed (platform::smp bring-up)".into());
    }
    let _irq = hal::cpu::IrqGuard::new();
    let want = hal::percpu::local_addr_of(hal::percpu::this_cpu());
    kassert!(want != 0, "this CPU has no GS block");
    GS_WANT.store(want, Ordering::Relaxed);
    PREV_GP.install(idt::GENERAL_PROTECTION, foreign_gs_hook);
    let mut bad = None;
    // The lower-half check (any kernel code): GS base 0 as Brave's threads had, and another lower-half value.
    for foreign in [0u64, 0x0000_7e00_dead_0000] {
        let h0 = GS_HITS.load(Ordering::Acquire);
        let Some((v, after)) = idt::probe_load_with_gs(foreign) else {
            bad = Some(format!("GS {foreign:#x}: the probe refused"));
            break;
        };
        let seen = GS_SEEN.load(Ordering::Relaxed);
        if GS_HITS.load(Ordering::Acquire) != h0 + 1 || v != want || seen != want || after != foreign {
            bad = Some(format!("GS {foreign:#x}: hits {}, value {v:#x}, hook saw GS {seen:#x} (block {want:#x}), GS at resume {after:#x}",
                GS_HITS.load(Ordering::Acquire) - h0));
            break;
        }
    }
    // The window test: the stubs' iretq faults with #GP(0x18) and a kernel-half GS base that is not this CPU's.
    if bad.is_none() {
        let decoy = idt::probe_iret_decoy();
        let h0 = GS_HITS.load(Ordering::Acquire);
        let after = idt::probe_iret_with_foreign_gs();
        let seen = GS_SEEN.load(Ordering::Relaxed);
        if GS_HITS.load(Ordering::Acquire) != h0 + 1 || seen != want || after != Some(decoy) {
            bad = Some(format!("iretq window, GS {decoy:#x}: hits {}, hook saw GS {seen:#x} (block {want:#x}), GS at resume {after:x?}",
                GS_HITS.load(Ordering::Acquire) - h0));
        }
    }
    let restored = PREV_GP.restore(idt::GENERAL_PROTECTION, foreign_gs_hook);
    kassert!(hal::msr::read(hal::msr::IA32_GS_BASE) == want, "GS base not restored");
    if let Some(b) = bad {
        return Err(b.into());
    }
    kassert!(restored, "someone else replaced the #GP hook during the test");
    Ok(())
}

/// The window `idt::in_user_gs_window` uses must still cover entry.rs's return paths (CR-2 relies on the order of
/// entry.rs's code blocks; AB's change request asks for explicit labels).
fn user_gs_window() -> TestResult {
    idt::user_gs_window_check().map_err(|e| e.into())
}

// ---------------------------------------------------------------------------------------------------------
// The evidence on the hardware itself: the SS selector a program finds after `sysretq`. Intel and QEMU force
// RPL 3 (0x2b since AB-10a's Linux layout); AMD loads STAR[63:48] + 8 as it is: 0x28 with an RPL-0 base 0x20,
// 0x2b with the base 0x23 (`__USER32_CS`) entry.rs writes since CP-8's change request (0x13 before AB-10a). The
// test reads both (the RPL-0 base on this CPU only, briefly).
// The NMI return still repairs a bad frame (`idt::user_return_fixup`) as a defence.

/// `mov eax, 39; syscall; mov eax, ss; mov edi, eax; mov eax, 39; syscall; jmp $`
const SS_PROGRAM: [u8; 20] = [
    0xb8, 0x27, 0x00, 0x00, 0x00, // mov eax, 39
    0x0f, 0x05, // syscall (returns by sysretq)
    0x8c, 0xd0, // mov eax, ss
    0x89, 0xc7, // mov edi, eax
    0xb8, 0x27, 0x00, 0x00, 0x00, // mov eax, 39
    0x0f, 0x05, // syscall
    0xeb, 0xfe, // jmp $
];

fn sysret_ss() -> TestResult {
    let f = hal::cpu::detect();
    let ss = ss_after_sysretq(true)?;
    // The pre-CP-8 STAR base (RPL 0) on this CPU only, interrupts off: the root-cause evidence on the hardware.
    let ss_rpl0 = hal::entry::with_sysret_base_rpl0(|| ss_after_sysretq(false))?;
    // How many user frames the stubs' iretq repaired since boot: with the RPL-3 STAR base it stays put during
    // `watchdog {"backtrace":true}` (before CP-8 it rose on AMD with every NMI that hit a program after sysretq).
    klog::event!(Info, "cp", "ktest.sysret_ss", ss = klog::Hex(ss), ss_base_rpl0 = klog::Hex(ss_rpl0), amd = f.amd, intel = f.intel, user_frames_fixed = idt::user_frames_fixed());
    kassert!(ss == 0x2b, "SS after sysretq {ss:#x} with the RPL-3 STAR base (amd {}, intel {})", f.amd, f.intel);
    kassert!(ss_rpl0 == 0x2b || (ss_rpl0 == 0x28 && !f.intel), "SS after sysretq {ss_rpl0:#x} with STAR base 0x20 (amd {}, intel {})", f.amd, f.intel);
    Ok(())
}

/// Runs `SS_PROGRAM` (two syscalls, the second entered after a `sysretq`) and gives the SS it read.
fn ss_after_sysretq(interrupts_on: bool) -> Result<u64, alloc::string::String> {
    let asp = AddressSpace::new().map_err(|e| format!("address space: {e:?}"))?;
    let rx = Prot { read: true, write: false, exec: true };
    let rw = Prot { read: true, write: true, exec: false };
    asp.map_fixed(CODE, 4096, rx, VmaKind::Image).map_err(|e| format!("map: {e:?}"))?;
    asp.write_loader(CODE, &SS_PROGRAM).map_err(|e| format!("load: {e:?}"))?;
    asp.map_fixed(STACK_TOP - STACK_LEN, STACK_LEN, rw, VmaKind::Stack).map_err(|e| format!("map: {e:?}"))?;
    let home = hal::paging::home_root();
    asp.activate();
    let mut ctx = UserContext::new(CODE, STACK_TOP - 64);
    let first = ctx.execute(interrupts_on);
    let second = (first == ReturnReason::Syscall).then(|| ctx.execute(interrupts_on));
    hal::paging::activate(home);
    drop(asp);
    if first != ReturnReason::Syscall || second != Some(ReturnReason::Syscall) {
        return Err(format!("program: {first:?}, then {second:?}"));
    }
    if ctx.stats.sysret != 1 {
        return Err(format!("the second entry was not a sysretq ({:?})", ctx.stats));
    }
    Ok(ctx.regs.rdi & 0xffff)
}
