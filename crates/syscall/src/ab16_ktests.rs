//! AB-16 (CP-8 finding 8): real signal delivery, rt_sigreturn with hostile frames and the program's own iretq
//! with bad selectors while every CPU takes NMI backtraces. Programs `abi-sigstorm <mode>` (user/abi-tests)
//! run on all other CPUs at once and are started again as they end; the test CPU meanwhile takes rounds of NMI
//! samples, with hal's AMD-SYSRET imitation on (the saved SS of every NMI from user mode loses its RPL, as AMD's
//! SYSRET left it before CP-8), so the NMI return fixup meets user frames in the middle of signal work. Every
//! hostile step must end in a signal to the program (its transcript is compared with CachyOS's), never in a
//! kernel fault. QemuOnly: user code on every CPU for seconds, and the SS imitation is global.

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use ktest::{TestResult, kassert, ktest};

const MS: u64 = 1_000_000;
const SEC: u64 = 1_000_000_000;
/// Rounds per `storm` life (each round runs every step of the other modes once).
const STORM_ROUNDS: &str = "40";
/// NMI sample rounds at least (programs are started again until then).
const MIN_ROUNDS: u64 = 400;
/// The whole run is bounded; QEMU's speed is never judged, only that it ends.
const DEADLINE: u64 = 300 * SEC;

const MODES: [(&str, &str); 5] = [
    ("storm", ""),
    ("hostile", include_str!("../../../user/abi-tests/expected/sigstorm-hostile.txt")),
    ("direct", include_str!("../../../user/abi-tests/expected/sigstorm-direct.txt")),
    ("iret", include_str!("../../../user/abi-tests/expected/sigstorm-iret.txt")),
    ("nested", include_str!("../../../user/abi-tests/expected/sigstorm-nested.txt")),
];
const KNOWN: &str = include_str!("../../../user/abi-tests/expected/sigstorm.known");

/// The text before a line's first ": " (the step's name).
fn label(l: &str) -> &str {
    l.split_once(": ").map_or(l, |(a, _)| a)
}

fn known_labels() -> Vec<&'static str> {
    KNOWN.lines().filter(|l| !l.starts_with('#')).filter_map(|l| l.split_once(" | ").map(|(a, _)| a.trim())).collect()
}

/// Lines of `got` that differ from CachyOS's `want`, except those whose step is a listed known difference.
fn diff(got: &str, want: &str, known: &[&str]) -> Vec<String> {
    let g: Vec<&str> = got.lines().collect();
    let mut d = Vec::new();
    for (i, w) in want.lines().enumerate() {
        let gl = g.get(i).copied().unwrap_or("<missing>");
        if gl != w && !(label(gl) == label(w) && known.contains(&label(w))) {
            d.push(format!("line {}: linux '{w}' got '{gl}'", i + 1));
        }
    }
    if g.len() > want.lines().count() {
        d.push(format!("{} extra lines", g.len() - want.lines().count()));
    }
    d
}

/// What a finished `storm` life must have printed (its rounds compare with its first one; the first round's
/// own answers are the other modes' business).
fn storm_ok(text: &str) -> bool {
    text.lines().any(|l| l.starts_with("storm: ") && l.contains("every round as the first yes"))
        && text.lines().any(|l| l == "storm: the timer signal arrived yes")
        && text.lines().last() == Some("done")
}

struct Life {
    p: Arc<proc::Process>,
    out: Arc<proc::stdio::Capture>,
    mode: usize,
}

fn start(mode: usize, cpu: usize) -> Result<Life, String> {
    let out = proc::stdio::Capture::new(1 << 16);
    let m = MODES[mode].0;
    let argv: &[&str] = if m == "storm" { &["abi-sigstorm", m, STORM_ROUNDS] } else { &["abi-sigstorm", m] };
    let pin = format!("SIGSTORM_CPU={cpu}");
    let env = ["HOME=/", "PATH=/bin", "ABI_TEST=1", pin.as_str()];
    let mut o = proc::SpawnOptions::new(argv, &env, policy::ProgramId(1));
    o.stdin = Some(proc::stdio::Input::new(Vec::new()) as Arc<dyn vfs::File>);
    o.stdout = Some(out.clone() as Arc<dyn vfs::File>);
    o.stderr = Some(out.clone() as Arc<dyn vfs::File>);
    let p = proc::spawn_with("/bin/abi-sigstorm", &o).map_err(|e| format!("spawn {m}: {e}"))?;
    Ok(Life { p, out, mode })
}

/// Checks one ended life: exit 0 and the transcript CachyOS gives.
fn check(l: &Life, known: &[&str]) -> TestResult {
    let (m, want) = MODES[l.mode];
    let code = l.p.wait(Some(SEC)).ok_or_else(|| format!("{m}: finished but no exit status"))?;
    let text = String::from_utf8_lossy(&l.out.contents()).into_owned();
    if m == "storm" {
        kassert!(code == 0 && storm_ok(&text), "abi-sigstorm storm (exit {code}):\n{text}");
    } else {
        let d = diff(&text, want, known);
        kassert!(code == 0 && d.is_empty(), "abi-sigstorm {m} differs from CachyOS (exit {code}):\n{}", d.join("\n"));
    }
    Ok(())
}

fn sample(wait_ns: u64) -> Result<hal::nmi::Round, String> {
    let end = time::now_ns() + 5 * SEC;
    loop {
        match hal::nmi::sample_others(time::now_ns, wait_ns) {
            Ok(r) => return Ok(r),
            Err(hal::nmi::NmiError::Busy) if time::now_ns() < end => sched::sleep_ns(MS),
            Err(e) => return Err(format!("sample: {e:?}")),
        }
    }
}

ktest!("ab.signal.nmi_storm", QemuOnly, nmi_storm);
fn nmi_storm() -> TestResult {
    if !crate::ab7_ktests::have("/bin/abi-sigstorm") {
        if crate::ab7_ktests::have("/bin/abi-ids") {
            return Err(String::from("the image has ABI programs but not /bin/abi-sigstorm (did it fail to build?)"));
        }
        return ktest::fail("the user/abi-tests programs are not in this image (no musl sysroot when it was built)");
    }
    let n = hal::percpu::cpu_count();
    if n < 2 {
        return ktest::fail("one CPU: nothing to sample");
    }
    if !hal::nmi::prepared() {
        return Err("the NMI slots were not prepared at boot".into());
    }
    let known = known_labels();
    let me = hal::percpu::this_cpu();
    // One program pinned to each other CPU at a time; every mode ends at least twice, every CPU's slot runs
    // at least twice.
    let cpus: Vec<usize> = (0..n).filter(|&c| c != me).collect();
    let slots = cpus.len();
    let want_ended = (2 * MODES.len()).max(2 * slots + MODES.len());
    let (b0, x0, f0) = (proc::deliver::bad_frame_count(), hal::idt::test_amd_sysret_ss_frames(), hal::idt::user_frames_fixed());
    let mut lives: Vec<Option<Life>> = (0..slots).map(|_| None).collect();
    let (mut started, mut ended, mut rounds, mut user_samples, mut frames) = (0usize, 0usize, 0u64, 0u64, 0u64);
    hal::idt::test_amd_sysret_ss(true);
    klog::event!(Info, "ktest", "ab16.storm_start", cpus = n, programs = want_ended);
    let end = time::now_ns() + DEADLINE;
    let res = (|| -> TestResult {
        while ended < want_ended || rounds < MIN_ROUNDS {
            kassert!(time::now_ns() < end, "only {ended} of {want_ended} programs ended within {} s", DEADLINE / SEC);
            for (slot, &cpu) in lives.iter_mut().zip(&cpus) {
                if let Some(l) = slot.as_ref().filter(|l| l.p.is_finished()) {
                    check(l, &known)?;
                    ended += 1;
                    *slot = None;
                }
                if slot.is_none() {
                    *slot = Some(start(started % MODES.len(), cpu)?);
                    started += 1;
                }
            }
            let r = sample(2 * SEC)?;
            rounds += 1;
            kassert!(r.send_failed == 0, "round {rounds}: {r:?}");
            for c in (0..n).filter(|&c| c != me) {
                if let Some(regs) = hal::nmi::regs(c) {
                    kassert!(regs.nsteps <= hal::nmi::DEPTH, "CPU {c}: {} steps", regs.nsteps);
                    frames += regs.nsteps as u64;
                    if regs.frame.cs & 3 == 3 {
                        user_samples += 1;
                    }
                }
            }
        }
        Ok(())
    })();
    hal::idt::test_amd_sysret_ss(false);
    for l in lives.iter().flatten() {
        l.p.kill(9);
        let _ = l.p.wait(Some(10 * SEC));
    }
    res?;
    let bad = proc::deliver::bad_frame_count() - b0;
    let tampered = hal::idt::test_amd_sysret_ss_frames() - x0;
    let fixed = hal::idt::user_frames_fixed() - f0;
    klog::event!(Info, "ktest", "ab16.storm", cpus = n, started = started, programs = ended, rounds = rounds, user_samples = user_samples, frames = frames,
        bad_frames = bad, tampered = tampered, fixed = fixed);
    kassert!(fixed >= tampered, "{tampered} NMI frames had SS without RPL 3 but only {fixed} were repaired");
    // Every hostile and direct life hands the kernel frames it must refuse.
    kassert!(bad > 0, "no sigreturn frame was refused in {ended} programs");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_lines_are_skipped_only_by_label() {
        let known = ["iretq ss ldt selector"];
        let want = "a: x\niretq ss ldt selector: SIGSEGV SI_KERNEL trap 13\n";
        assert!(diff("a: x\niretq ss ldt selector: SIGSEGV SEGV_MAPERR trap 14\n", want, &known).is_empty());
        assert_eq!(diff("a: y\niretq ss ldt selector: SIGSEGV SI_KERNEL trap 13\n", want, &known).len(), 1);
        assert_eq!(diff("a: x\n", want, &known).len(), 1, "a missing known line still counts");
    }

    #[test]
    fn the_known_file_is_empty_since_the_null_ldtr() {
        // INT-W21: CP's null LDTR (gdt::load) gives Linux's #GP for LDT selectors, so nothing is known any more.
        assert!(known_labels().is_empty());
        for (m, want) in MODES.iter().skip(1) {
            assert!(want.starts_with(&format!("mode {m}\n")) && want.ends_with("done\n"), "{m}");
        }
    }

    #[test]
    fn storm_transcript() {
        assert!(storm_ok("mode storm\nstorm: 4 rounds, every round as the first yes (0 mismatches)\nstorm: the timer signal arrived yes\ndone\n"));
        assert!(!storm_ok("mode storm\nstorm: 4 rounds, every round as the first no (3 mismatches)\nstorm: the timer signal arrived yes\ndone\n"));
    }
}
