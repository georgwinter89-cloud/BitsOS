//! AB-1 ktests: the syscall entry, user copies with fault fixup, interrupts in ring 3, FS/GS base, ENOSYS.
//!
//! Process tests run small position-independent programs (`hal::entry::test_programs`) wrapped into a static
//! ELF here, through the real path (`proc::spawn_image`, `execute(true)`, `dispatch`, wave-A handlers); each
//! program ends with `exit_group(0)` or the number of its failed check. The `execute` tests drive a
//! `UserContext` directly in an address space of their own.

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use hal::entry::{ReturnReason, UserContext, test_programs};
use ktest::{TestResult, kassert, ktest};
use mm::{AddressSpace, Prot, VmaKind};

const SEC: u64 = 1_000_000_000;
const BASE: u64 = 0x40_0000;
const HDR: usize = 64 + 56;

/// A static x86-64 ELF with one R+X segment (headers and code) at 0x400000; entry right after the headers.
pub(crate) fn elf(code: &[u8]) -> Vec<u8> {
    let total = (HDR + code.len()) as u64;
    let mut b = vec![0u8; HDR];
    b[0..4].copy_from_slice(b"\x7fELF");
    b[4] = 2; // ELFCLASS64
    b[5] = 1; // little endian
    b[6] = 1; // EV_CURRENT
    b[16..18].copy_from_slice(&2u16.to_le_bytes()); // ET_EXEC
    b[18..20].copy_from_slice(&0x3Eu16.to_le_bytes()); // x86-64
    b[20..24].copy_from_slice(&1u32.to_le_bytes());
    b[24..32].copy_from_slice(&(BASE + HDR as u64).to_le_bytes());
    b[32..40].copy_from_slice(&64u64.to_le_bytes()); // phoff
    b[52..54].copy_from_slice(&64u16.to_le_bytes()); // ehsize
    b[54..56].copy_from_slice(&56u16.to_le_bytes()); // phentsize
    b[56..58].copy_from_slice(&1u16.to_le_bytes()); // phnum
    let ph = 64;
    b[ph..ph + 4].copy_from_slice(&1u32.to_le_bytes()); // PT_LOAD
    b[ph + 4..ph + 8].copy_from_slice(&5u32.to_le_bytes()); // R + X
    b[ph + 16..ph + 24].copy_from_slice(&BASE.to_le_bytes());
    b[ph + 24..ph + 32].copy_from_slice(&BASE.to_le_bytes());
    b[ph + 32..ph + 40].copy_from_slice(&total.to_le_bytes());
    b[ph + 40..ph + 48].copy_from_slice(&total.to_le_bytes());
    b[ph + 48..ph + 56].copy_from_slice(&0x1000u64.to_le_bytes());
    b.extend_from_slice(code);
    b
}

fn start(name: &str, code: &[u8]) -> Result<Arc<proc::Process>, String> {
    if code.is_empty() {
        return Err(String::from("no test program in this build"));
    }
    proc::spawn_image(name, &elf(code), &[name], &["HOME=/"], policy::ProgramId(1)).map_err(|e| format!("spawn {name}: {e:?}"))
}

fn finish(name: &str, p: &proc::Process, timeout_s: u64) -> TestResult {
    let code = p.wait(Some(timeout_s * SEC)).ok_or_else(|| format!("{name} did not end within {timeout_s} s"))?;
    match code {
        0 => Ok(()),
        c if c >= 128 => Err(format!("{name} was ended by the kernel (signal {}); see the proc fault line", c - 128)),
        c => Err(format!("{name}: check {c} failed (numbers in hal::entry test_programs)")),
    }
}

fn run(name: &str, code: &[u8], timeout_s: u64) -> TestResult {
    let p = start(name, code)?;
    finish(name, &p, timeout_s)
}

// ---------------------------------------------------------------------------------------------------------
// Process tests

ktest!("ab.syscall.bad_pointers", Safe, bad_pointers);
fn bad_pointers() -> TestResult {
    let before = hal::usercopy::stats();
    run("ab-badptr", test_programs::bad_pointers(), 10)?;
    let after = hal::usercopy::stats();
    if after.fast_path {
        // Since MM-5a the address space's copies refuse unmapped ranges before copying; only the path string of
        // check 19 (open at 0x10) still reaches the fast copy's fixup (the resolver finds no mapping there).
        kassert!(after.fast_faults >= before.fast_faults + 1, "no fixup for the unmapped path");
    }
    Ok(())
}

ktest!("ab.syscall.wave_a", Safe, wave_a_calls);
fn wave_a_calls() -> TestResult {
    let before = crate::calls(uapi::nr::GETPID);
    run("ab-wave-a", test_programs::wave_a(), 10)?;
    kassert!(crate::calls(uapi::nr::GETPID) >= before + 2, "getpid not counted");
    let st = crate::stats();
    kassert!(st.iter().any(|s| s.nr == uapi::nr::CLOCK_GETTIME && s.errors > 0), "clock_gettime's EFAULT not counted as an error");
    Ok(())
}

ktest!("ab.syscall.enosys_log_once", Safe, enosys_log_once);
fn enosys_log_once() -> TestResult {
    const NR: usize = 4001;
    let count = crate::enosys_count(NR);
    let other = crate::enosys_count(usize::MAX);
    let seq = klog::next_seq();
    run("ab-enosys", test_programs::enosys(), 10)?;
    kassert!(crate::enosys_count(NR) == count + 3, "ENOSYS count for {NR}: {} -> {}", count, crate::enosys_count(NR));
    kassert!(crate::enosys_count(usize::MAX) == other + 2, "numbers from 4096 on: {} -> {}", other, crate::enosys_count(usize::MAX));
    let filter = klog::Filter { level: None, subsystem: Some("syscall"), name: Some("enosys") };
    let logged = klog::read(seq, 1000, &filter).records.iter().filter(|r| r.text.split_whitespace().any(|w| w == "nr=4001")).count();
    let want = if count == 0 { 1 } else { 0 };
    kassert!(logged == want, "{logged} enosys events for {NR} (expected {want}: logged once per number)");
    kassert!(crate::stats().iter().any(|s| s.nr == NR && s.enosys && s.calls == count + 3), "stats() lacks the ENOSYS line");
    Ok(())
}

ktest!("ab.entry.fsgsbase_kept", Safe, fsgsbase_kept);
fn fsgsbase_kept() -> TestResult {
    if !hal::entry::user_fsgsbase() {
        return ktest::skip("CR4.FSGSBASE is off: programs cannot use wrfsbase");
    }
    // Two instances with different bases sleep in turns, so each one's bases are switched out and back in.
    let a = start("ab-fsgs-1", test_programs::fsgsbase())?;
    let b = start("ab-fsgs-2", test_programs::fsgsbase())?;
    finish("ab-fsgs-1", &a, 20)?;
    finish("ab-fsgs-2", &b, 20)
}

// ---------------------------------------------------------------------------------------------------------
// execute() tests in an address space of their own

const CODE: u64 = 0x40_0000;

/// Runs `f` with a fresh address space holding `code` at CODE (R+X) loaded in CR3; puts the kernel root back.
fn in_user_space(code: &[u8], extra: impl FnOnce(&AddressSpace) -> TestResult, f: impl FnOnce() -> TestResult) -> TestResult {
    let asp = AddressSpace::new().map_err(|e| format!("address space: {e:?}"))?;
    if !code.is_empty() {
        asp.map_fixed(CODE, (code.len() as u64).div_ceil(4096) * 4096, Prot { read: true, write: false, exec: true }, VmaKind::Image).map_err(|e| format!("map: {e:?}"))?;
        asp.write_loader(CODE, code).map_err(|e| format!("load: {e:?}"))?;
    }
    extra(&asp)?;
    let kernel_root = hal::paging::home_root();
    asp.activate();
    let r = f();
    hal::paging::activate(kernel_root);
    drop(asp);
    r
}

static IRQ_HITS: AtomicU64 = AtomicU64::new(0);
fn irq_hit(_v: u8) {
    IRQ_HITS.fetch_add(1, Ordering::Relaxed);
}

ktest!("ab.entry.user_interrupts", Safe, user_interrupts);
/// User code runs with IF=1 while a self-IPI fires (sent with interrupts off just before `execute`, so it is
/// taken at the first user instruction): the handler runs, the program resumes with every register and its
/// FS/GS base intact, and `execute` returns only at the program's syscall. Odd rounds take SYSRET.
fn user_interrupts() -> TestResult {
    if !hal::entry::user_fsgsbase() {
        return ktest::skip("CR4.FSGSBASE is off: the test program checks its bases with rdfsbase");
    }
    in_user_space(test_programs::register_loop(), |_| Ok(()), || {
        let cpu = hal::percpu::this_cpu();
        let v = hal::vectors::alloc(irq_hit, Some(cpu as u32)).map_err(|e| format!("vector: {e:?}"))?;
        let r = interrupt_rounds(cpu, v.vector);
        hal::vectors::free(v);
        r
    })
}

fn interrupt_rounds(cpu: usize, vector: u8) -> TestResult {
    const ROUNDS: u32 = 16;
    let mut ctx = UserContext::new(CODE, 0);
    let (mut fs, mut gs) = (0x0000_7F00_1234_0000u64, 0x0000_7F00_5678_0000u64);
    let irqs_before = hal::entry::user_interrupts();
    for round in 0..ROUNDS {
        if round == ROUNDS / 2 {
            // New bases between rounds: the entry writes what the kernel puts into the context.
            fs += 0x1000;
            gs += 0x1000;
        }
        ctx.fs_base = fs;
        ctx.gs_base = gs;
        ctx.regs.r14 = fs;
        ctx.regs.r15 = gs;
        let with_irq = round % 2 == 0;
        let (hits, seen, sysret) = (IRQ_HITS.load(Ordering::Relaxed), ctx.stats.interrupts, ctx.stats.sysret);
        let reason = {
            let _irq = hal::cpu::IrqGuard::new();
            if with_irq {
                kassert!(hal::percpu::this_cpu() == cpu, "moved to another CPU");
                hal::apic::send_ipi(hal::apic::IpiDest::SelfOnly, hal::apic::IpiKind::Fixed(vector)).map_err(|e| format!("self IPI: {e:?}"))?;
            }
            ctx.execute(true)
        };
        match reason {
            ReturnReason::Syscall => {}
            ReturnReason::Trap { vector, error, cr2 } => {
                return Err(format!("round {round}: trap {vector} error {error:#x} cr2 {cr2:#x} rip {:#x}", ctx.regs.rip));
            }
        }
        kassert!(ctx.regs.rax == 0, "round {round}: the program's check {} failed (1-10 registers, 11 FS, 12 GS)", ctx.regs.rax);
        kassert!(ctx.fs_base == fs && ctx.gs_base == gs, "round {round}: bases read back {:#x}/{:#x}, want {fs:#x}/{gs:#x}", ctx.fs_base, ctx.gs_base);
        if with_irq {
            kassert!(IRQ_HITS.load(Ordering::Relaxed) == hits + 1, "round {round}: the IPI handler ran {} times", IRQ_HITS.load(Ordering::Relaxed) - hits);
            kassert!(ctx.stats.interrupts > seen, "round {round}: the IPI did not arrive in user mode");
        }
        if round > 0 {
            kassert!(ctx.stats.sysret > sysret, "round {round}: return after a plain syscall did not use SYSRET");
        }
    }
    kassert!(hal::entry::user_interrupts() >= irqs_before + (ROUNDS / 2) as u64, "global user interrupt count did not grow");
    Ok(())
}

ktest!("ab.entry.bad_rip", Safe, bad_rip);
/// A context whose rip is not canonical never reaches iretq/sysretq (their #GP would come in ring 0 after
/// swapgs): it returns as the program's own #GP. Canonical addresses near the top fault in user mode.
fn bad_rip() -> TestResult {
    in_user_space(&[], |_| Ok(()), || {
        let cases: [(u64, u8); 4] = [
            (0x0000_8000_0000_0000, 13),
            (0xFFFF_8000_0000_1000, 13),
            // The last bytes below USER_TOP: SYSRET is allowed there; nothing is mapped, so #PF in user mode.
            (hal::USER_TOP - 16, 14),
            // Canonical but in the top page: IRET, then #PF in user mode.
            (0x0000_7FFF_FFFF_FFF0, 14),
        ];
        for (rip, want) in cases {
            let mut ctx = UserContext::new(rip, 0);
            ctx.regs.rcx = rip;
            ctx.regs.r11 = 0x202;
            match ctx.execute(true) {
                ReturnReason::Trap { vector, cr2, .. } if vector == want => {
                    if want == 14 {
                        kassert!(cr2 == rip, "rip {rip:#x}: #PF at {cr2:#x}");
                    }
                }
                other => return Err(format!("rip {rip:#x}: {other:?}, want trap {want}")),
            }
            if want == 13 {
                kassert!(ctx.stats.bad_rip == 1 && ctx.stats.sysret + ctx.stats.iret == 0, "rip {rip:#x} reached the return to user");
            } else if rip < hal::USER_TOP {
                kassert!(ctx.stats.sysret == 1, "rip {rip:#x}: SYSRET not used");
            } else {
                kassert!(ctx.stats.iret == 1, "rip {rip:#x}: IRET not used");
            }
        }
        Ok(())
    })
}

// ---------------------------------------------------------------------------------------------------------
// User copies from the kernel side

const PAGE_A: u64 = 0x50_0000;
const HOLE: u64 = PAGE_A + 0x1000;

static RESOLVE_AS: ksync::SpinLock<Option<Arc<AddressSpace>>> = ksync::SpinLock::named("ab.test.resolve", None);
static RESOLVE_CALLS: AtomicU64 = AtomicU64::new(0);

/// Test resolver: maps the hole page of the test address space on demand, refuses everything else.
fn resolve_hole(addr: u64, _write: bool) -> bool {
    let Some(asp) = RESOLVE_AS.lock().clone() else { return false };
    if addr & !0xFFF != HOLE || asp.root() != hal::paging::current_root() {
        return false;
    }
    RESOLVE_CALLS.fetch_add(1, Ordering::Relaxed);
    asp.map_fixed(HOLE, 4096, Prot::rw(), VmaKind::Anonymous).is_ok()
}

ktest!("ab.usercopy.fast_and_walk", Safe, usercopy_paths);
fn usercopy_paths() -> TestResult {
    use hal::usercopy as uc;
    let asp = Arc::new(AddressSpace::new().map_err(|e| format!("address space: {e:?}"))?);
    asp.map_fixed(PAGE_A, 4096, Prot::rw(), VmaKind::Anonymous).map_err(|e| format!("map: {e:?}"))?;
    let root = asp.root();
    // Not loaded: the page walk.
    let walks = uc::stats().walks;
    kassert!(uc::copy_to_user(root, HOLE - 4, b"12345678").is_err(), "walk wrote into the hole");
    uc::copy_to_user(root, HOLE - 6, b"hello\0").map_err(|_| "walk write failed")?;
    kassert!(uc::stats().walks > walks, "root not loaded but no walk");
    let kernel_root = hal::paging::home_root();
    asp.activate();
    let r = fast_checks(&asp);
    hal::paging::activate(kernel_root);
    *RESOLVE_AS.lock() = None;
    drop(asp);
    r
}

fn fast_checks(asp: &Arc<AddressSpace>) -> TestResult {
    use hal::usercopy as uc;
    let root = asp.root();
    let s0 = uc::stats();
    if !s0.fast_path {
        return ktest::skip("the user-copy fast path is not installed");
    }
    let mut b = [0u8; 6];
    uc::copy_from_user(root, &mut b, HOLE - 6).map_err(|_| "fast read failed")?;
    kassert!(&b == b"hello\0", "read back {b:?}");
    kassert!(uc::stats().walks == s0.walks, "loaded root but the copy walked");
    // A string ending in the last byte before an unmapped page.
    let mut s = Vec::new();
    let n = uc::strncpy_from_user(root, HOLE - 6, &mut s, 4096).map_err(|_| "string before the hole")?;
    kassert!(n == 5 && s == b"hello", "string {s:?}");
    // Reads and writes running into the hole fault and are fixed up.
    let mut big = [0u8; 16];
    kassert!(uc::copy_from_user(root, &mut big, HOLE - 8).is_err(), "read across the hole succeeded");
    kassert!(uc::copy_to_user(root, HOLE, b"x").is_err(), "write into the hole succeeded");
    kassert!(uc::zero_user(root, HOLE - 16, 32).is_err(), "zero across the hole succeeded");
    uc::copy_from_user(root, &mut big, HOLE - 16).map_err(|_| "re-read")?;
    kassert!(big == [0u8; 16], "zero_user did not clear the bytes before the hole: {big:?}");
    // No NUL within max bytes is TooLong (ENAMETOOLONG for paths), not a fault.
    uc::copy_to_user(root, HOLE - 6, b"hello\0").map_err(|_| "rewrite the string")?;
    kassert!(uc::strncpy_from_user(root, HOLE - 6, &mut s, 5) == Err(uc::StrError::TooLong), "5 bytes without NUL");
    kassert!(uc::strncpy_from_user(root, HOLE - 6, &mut s, 6) == Ok(5), "string of 5 with max 6");
    uc::copy_to_user(root, HOLE - 3, b"abc").map_err(|_| "write")?;
    kassert!(uc::strncpy_from_user(root, HOLE - 3, &mut s, 4096) == Err(uc::StrError::Fault), "string without NUL before the hole");
    kassert!(uc::stats().fast_faults >= s0.fast_faults + 4, "the faults did not go through the fixup");
    // The kernel half and wrapping ranges are refused before any access.
    kassert!(uc::copy_from_user(root, &mut big, 0xFFFF_8880_0000_0000).is_err(), "kernel address read");
    kassert!(uc::copy_from_user(root, &mut big, u64::MAX - 4).is_err(), "wrapping range read");
    // With a resolver the copy retries after it makes the page present (MM-5a's lazy fill).
    *RESOLVE_AS.lock() = Some(asp.clone());
    let calls = RESOLVE_CALLS.load(Ordering::Relaxed);
    let before = uc::set_fault_resolver(Some(resolve_hole));
    let r = uc::copy_to_user(root, HOLE - 4, b"resolved");
    // Back to proc's resolver: every later program's syscalls need it (AB-8).
    uc::set_fault_resolver(before);
    r.map_err(|_| "copy with resolver failed")?;
    kassert!(RESOLVE_CALLS.load(Ordering::Relaxed) == calls + 1, "resolver called {} times", RESOLVE_CALLS.load(Ordering::Relaxed) - calls);
    let mut back = [0u8; 8];
    uc::copy_from_user(root, &mut back, HOLE - 4).map_err(|_| "read after resolve")?;
    kassert!(&back == b"resolved", "read back {back:?}");
    kassert!(uc::stats().resolved > s0.resolved, "resolved copies not counted");
    Ok(())
}

// ---------------------------------------------------------------------------------------------------------
// Fixer round: interrupts that stop `execute_or_stop`, a bad rsp at an interrupt, kernel roots walk

/// `syscall; jmp .`
const SYSCALL_THEN_SPIN: [u8; 4] = [0x0F, 0x05, 0xEB, 0xFE];
/// `movabs $0x8000000000000000, %rsp; 1: pause; jmp 1b`: spins with a non-canonical stack pointer.
const BAD_RSP_SPIN: [u8; 14] = [0x48, 0xBC, 0, 0, 0, 0, 0, 0, 0, 0x80, 0xF3, 0x90, 0xEB, 0xFC];

static HOOK_CALLS: AtomicU64 = AtomicU64::new(0);
fn hook_resume() -> bool {
    HOOK_CALLS.fetch_add(1, Ordering::Relaxed);
    false
}
fn hook_stop() -> bool {
    HOOK_CALLS.fetch_add(1, Ordering::Relaxed);
    true
}

ktest!("ab.entry.stop_on_interrupt", Safe, stop_on_interrupt);
/// `execute_or_stop` comes back (None) after an interrupt in user mode when no hook is installed or the hook
/// asks for it, and resumes the program when the hook says so; proc's loop relies on it to see kills (B4).
fn stop_on_interrupt() -> TestResult {
    in_user_space(&SYSCALL_THEN_SPIN, |_| Ok(()), || {
        let cpu = hal::percpu::this_cpu();
        let v = hal::vectors::alloc(irq_hit, Some(cpu as u32)).map_err(|e| format!("vector: {e:?}"))?;
        let old = hal::entry::user_irq_hook();
        let r = stop_rounds(v.vector);
        hal::entry::set_user_irq_hook(old);
        hal::vectors::free(v);
        r
    })
}

fn stop_rounds(vector: u8) -> TestResult {
    // The self-IPI is sent with interrupts off, so it arrives at the first user instruction (before `syscall`).
    let run = |ctx: &mut UserContext| -> Result<Option<ReturnReason>, String> {
        let _irq = hal::cpu::IrqGuard::new();
        hal::apic::send_ipi(hal::apic::IpiDest::SelfOnly, hal::apic::IpiKind::Fixed(vector)).map_err(|e| format!("self IPI: {e:?}"))?;
        Ok(ctx.execute_or_stop(true))
    };
    hal::entry::set_user_irq_hook(None);
    let mut ctx = UserContext::new(CODE, 0);
    let r = run(&mut ctx)?;
    kassert!(r.is_none() && ctx.regs.rip == CODE && ctx.stats.interrupts == 1, "no hook: {r:?} at {:#x}, {} interrupts", ctx.regs.rip, ctx.stats.interrupts);
    hal::entry::set_user_irq_hook(Some(hook_resume));
    // INT-W9 (GX-3 CR 4): the hook runs after every interrupt in user mode, so a timer tick under load adds a
    // call while the program resumes; the test's own vector must be taken exactly once per round.
    let (calls, hits) = (HOOK_CALLS.load(Ordering::Relaxed), IRQ_HITS.load(Ordering::Relaxed));
    let r = run(&mut ctx)?;
    kassert!(r == Some(ReturnReason::Syscall) && ctx.regs.rip == CODE + 2, "hook says resume: {r:?} at {:#x}", ctx.regs.rip);
    kassert!(HOOK_CALLS.load(Ordering::Relaxed) > calls, "the hook did not run");
    kassert!(IRQ_HITS.load(Ordering::Relaxed) == hits + 1, "the test vector ran {} times", IRQ_HITS.load(Ordering::Relaxed) - hits);
    hal::entry::set_user_irq_hook(Some(hook_stop));
    ctx.regs.rip = CODE;
    let (calls, hits) = (HOOK_CALLS.load(Ordering::Relaxed), IRQ_HITS.load(Ordering::Relaxed));
    let r = run(&mut ctx)?;
    kassert!(r.is_none() && ctx.regs.rip == CODE, "hook says stop: {r:?} at {:#x}", ctx.regs.rip);
    // A stop ends the run at the first interrupt: one call, whichever interrupt it was.
    kassert!(HOOK_CALLS.load(Ordering::Relaxed) == calls + 1, "hook ran {} times", HOOK_CALLS.load(Ordering::Relaxed) - calls);
    kassert!(IRQ_HITS.load(Ordering::Relaxed) == hits + 1, "the test vector ran {} times", IRQ_HITS.load(Ordering::Relaxed) - hits);
    Ok(())
}

static KICK_STOP: AtomicU64 = AtomicU64::new(0);
static KICKS: AtomicU64 = AtomicU64::new(0);
const KICK_LIMIT_NS: u64 = 2 * SEC;

/// On a helper AP (hard-IRQ context, from `call_on`): sends the fixed IPI `arg & 0xFF` to APIC ID `arg >> 8`
/// every 10 ms until the test says stop, for at most `KICK_LIMIT_NS`.
fn kick(arg: u64) -> u64 {
    let (apic, vector) = ((arg >> 8) as u32, arg as u8);
    let end = time::now_ns().saturating_add(KICK_LIMIT_NS);
    let mut next = time::now_ns() + 10_000_000;
    while KICK_STOP.load(Ordering::Acquire) == 0 && time::now_ns() < end {
        if time::now_ns() >= next {
            if hal::apic::send_ipi(hal::apic::IpiDest::Physical(apic), hal::apic::IpiKind::Fixed(vector)).is_ok() {
                KICKS.fetch_add(1, Ordering::Relaxed);
            }
            next += 10_000_000;
        }
        core::hint::spin_loop();
    }
    0
}

ktest!("ab.entry.bad_stack_interrupted", Safe, bad_stack_interrupted);
/// A program spins with a non-canonical rsp while another CPU sends it interrupts. The return to user after
/// the interrupt would be an IRET that faults on that rsp in ring 0 with the user's GS; instead the program
/// gets #SS as its own fault and the kernel stays up.
fn bad_stack_interrupted() -> TestResult {
    let me = hal::percpu::this_cpu();
    let Some(helper) = (0..hal::percpu::cpu_count()).find(|&c| c != me && hal::smp::is_online(c)) else {
        return ktest::skip("needs a second CPU online");
    };
    let apic = hal::smp::apic_id(me).ok_or("no APIC ID for this CPU")?;
    in_user_space(&BAD_RSP_SPIN, |_| Ok(()), || {
        let v = hal::vectors::alloc(irq_hit, Some(me as u32)).map_err(|e| format!("vector: {e:?}"))?;
        let r = bad_stack_round(helper, apic, v.vector);
        KICK_STOP.store(1, Ordering::Release);
        hal::vectors::free(v);
        r
    })
}

fn bad_stack_round(helper: usize, apic: u32, vector: u8) -> TestResult {
    KICK_STOP.store(0, Ordering::Release);
    let kicks = KICKS.load(Ordering::Relaxed);
    // A short timeout on purpose: `Late` means the helper took the call and kicks from now on.
    let mut started = false;
    for _ in 0..100 {
        match hal::smp::call_on(helper, kick, ((apic as u64) << 8) | vector as u64, time::now_ns, 2_000_000) {
            Err(hal::smp::CallError::Late) => {
                started = true;
                break;
            }
            Err(hal::smp::CallError::Timeout) | Err(hal::smp::CallError::Busy) => continue,
            Ok(_) => return Err(String::from("the helper ended before the program ran")),
            Err(e) => return Err(format!("start the helper: {e:?}")),
        }
    }
    if !started {
        return ktest::skip("the helper CPU never took the call");
    }
    let mut ctx = UserContext::new(CODE, 0x7000_0000);
    let mut stops = 0u32;
    let reason = loop {
        let r = {
            let _irq = hal::cpu::IrqGuard::new();
            ctx.execute_or_stop(true)
        };
        match r {
            Some(r) => break r,
            // Interrupted with no hook: call again, which must refuse the entry now.
            None => stops += 1,
        }
        kassert!(stops < 4, "execute_or_stop keeps coming back without a trap");
    };
    KICK_STOP.store(1, Ordering::Release);
    kassert!(reason == ReturnReason::Trap { vector: 12, error: 0, cr2: 0 }, "got {reason:?}, want #SS");
    kassert!(ctx.regs.rsp == 0x8000_0000_0000_0000, "saved rsp {:#x}", ctx.regs.rsp);
    kassert!(ctx.stats.interrupts >= 1 && ctx.stats.bad_stack == 1, "stats {:?}", ctx.stats);
    // CP-5: this CPU's own LAPIC timer may interrupt the program before the helper's first IPI (10 ms); either
    // way an interrupt ended it (`stats.interrupts` above), which is what this test is about.
    let _ = kicks;
    Ok(())
}

ktest!("ab.usercopy.kernel_root_walks", Safe, kernel_root_walks);
/// The kernel's own roots may map supervisor-only pages below USER_TOP (the loader's identity map, the kexec
/// stub page), which a ring-0 copy would reach despite SMAP. A user copy against a kernel root must therefore
/// walk (and refuse non-USER pages), never take the fast path. Reads only, so a regression damages nothing.
fn kernel_root_walks() -> TestResult {
    use hal::usercopy as uc;
    let root = hal::paging::current_root();
    if root != hal::switch::kernel_root() && Some(root) != hal::paging::kernel_root() {
        return ktest::skip("this thread does not run on a kernel root");
    }
    // A supervisor-only page of the kernel root below 4 GiB if there is one (2 MiB steps), else a low address.
    let supervisor = (1..2048u64).map(|i| i << 21).find(|&va| matches!(hal::paging::translate(root, va), Some((_, f)) if !f.contains(hal::paging::PageFlags::USER)));
    for va in [supervisor.unwrap_or(0x1000), 0x1000, 0x40_0000] {
        let s0 = uc::stats();
        let mut b = [0u8; 16];
        kassert!(uc::copy_from_user(root, &mut b, va) == Err(uc::Efault), "read at {va:#x} through the kernel root");
        let s1 = uc::stats();
        kassert!(s1.walks > s0.walks && s1.fast_faults == s0.fast_faults, "kernel root at {va:#x}: fast path taken");
    }
    Ok(())
}
