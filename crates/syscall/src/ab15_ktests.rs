//! AB-15 (measuring build only, `--cfg bitsos_fd_measure`): where the null syscall's cycles go. A program
//! makes `CALLS` getppid calls through the real path while `hal::entry::steps` charges each step its TSC
//! cycles; a second test times the single instructions and kernel helpers on that path. Numbers only, written
//! as `ab15.*` events: timing is judged on the Mini-PC, never in QEMU.

use alloc::format;
use hal::entry::steps;
use ktest::{TestResult, kassert, ktest};

const CALLS: u32 = 200_000;

/// `mov r12d, CALLS; 1: mov eax, 110 (getppid); syscall; dec r12; jnz 1b; mov eax, 231; xor edi, edi; syscall`.
fn program() -> alloc::vec::Vec<u8> {
    let mut c = alloc::vec![0x41, 0xBC];
    c.extend_from_slice(&CALLS.to_le_bytes());
    c.extend_from_slice(&[0xB8, 110, 0, 0, 0, 0x0F, 0x05, 0x49, 0xFF, 0xCC, 0x75, 0xF4]);
    c.extend_from_slice(&[0xB8, 231, 0, 0, 0, 0x31, 0xFF, 0x0F, 0x05]);
    c
}

fn ns(cycles: u64) -> u64 {
    let hz = time::tsc_hz().max(1);
    cycles.saturating_mul(1_000_000_000) / hz
}

ktest!("ab.measure.syscall_steps", Safe, syscall_steps);
fn syscall_steps() -> TestResult {
    let img = crate::ktests::elf(&program());
    let argv = ["ab15-getppid"];
    let envp = ["HOME=/"];
    let mut o = proc::SpawnOptions::new(&argv, &envp, policy::ProgramId(1));
    o.held = true;
    let p = proc::spawn_image_with("ab15-getppid", &img, &o).map_err(|e| format!("spawn: {e:?}"))?;
    steps::start(p.mm.root());
    p.release();
    let code = p.wait(Some(60 * 1_000_000_000));
    steps::stop();
    if code != Some(0) {
        return Err(format!("the program ended with {code:?}"));
    }
    let mut total = 0;
    let results = steps::results();
    for &(name, calls, cyc) in &results {
        if name != "empty_mark" {
            total += cyc;
        }
        klog::event!(Info, "ab", "ab15.step", step = name, calls = calls, cycles = cyc, ns = ns(cyc));
    }
    // Every step on the path runs once per call (`results` lists only steps with calls; a missing one is a mark
    // that no longer runs). Half of CALLS leaves room for the uncharged first mark and the exit_group round.
    let hit = |n: &str| results.iter().any(|r| r.0 == n && r.1 >= u64::from(CALLS) / 2);
    let missing: alloc::vec::Vec<&str> = steps::NAMES.iter().copied().filter(|n| *n != "empty_mark" && !hit(n)).collect();
    kassert!(missing.is_empty(), "steps not hit once per call: {missing:?}");
    kassert!(total > 0, "the steps summed to 0 cycles");
    klog::event!(Info, "ab", "ab15.round", cycles = total, ns = ns(total), tsc_hz = time::tsc_hz(); "sum of the steps per getppid");
    Ok(())
}

ktest!("ab.measure.instructions", QemuOnly, instructions);
fn instructions() -> TestResult {
    let mut v = hal::entry::steps::instructions(true);
    use steps::cycles as c;
    v.push(("time::now_ns", c(|| {
        core::hint::black_box(time::now_ns());
    })));
    v.push(("klog::now", c(|| {
        core::hint::black_box(klog::now());
    })));
    v.push(("sched::current_thread (Arc clone+drop)", c(|| {
        core::hint::black_box(sched::current_thread());
    })));
    v.push(("IrqGuard", c(|| {
        let _g = core::hint::black_box(hal::cpu::IrqGuard::new());
    })));
    for (name, cyc) in v {
        klog::event!(Info, "ab", "ab15.insn", what = name, cycles = cyc, ns = ns(cyc));
    }
    Ok(())
}
