//! L7-RESTE-AB ktests: the abi-tests programs of the L7 leftovers against CachyOS's transcripts (the same cases
//! `cargo xtask abi-diff` runs), so the merged gate sees them.

use crate::sx2_ktests::{diff, have};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use ktest::{TestResult, kassert, ktest};

const SEC: u64 = 1_000_000_000;

/// Runs `path arg` as the abi-diff harness does (as uid 1000), "nodac" as root; its exit code and output.
fn run(path: &str, arg: &str) -> Result<(i32, String), String> {
    let out = proc::stdio::Capture::new(1 << 20);
    let argv: Vec<&str> = if arg.is_empty() { Vec::from([path]) } else { Vec::from([path, arg]) };
    let env = ["HOME=/", "PATH=/bin", "ABI_TEST=1"];
    let mut o = proc::SpawnOptions::new(&argv, &env, policy::ProgramId(1));
    if arg == "nodac" {
        o.uid = 0;
    }
    o.stdin = Some(proc::stdio::Input::new(Vec::new()) as Arc<dyn vfs::File>);
    o.stdout = Some(out.clone() as Arc<dyn vfs::File>);
    o.stderr = Some(out.clone() as Arc<dyn vfs::File>);
    let p = proc::spawn_with(path, &o).map_err(|e| format!("spawn {path}: {e}"))?;
    match p.wait(Some(120 * SEC)) {
        Some(c) => Ok((c, String::from_utf8_lossy(&out.contents()).into_owned())),
        None => {
            p.kill(9);
            let _ = p.wait(Some(10 * SEC));
            Err(format!("{path} {arg} did not end within 120 s:\n{}", String::from_utf8_lossy(&out.contents())))
        }
    }
}

fn programs(cases: &[(&str, &str, &str)]) -> TestResult {
    if !have(cases[0].0) {
        if have("/bin/abi-ids") {
            return Err(format!("the image has ABI programs but not {} (did it fail to build?)", cases[0].0));
        }
        return ktest::fail("the user/abi-tests programs are not in this image (no musl sysroot when it was built)");
    }
    for &(path, arg, want) in cases {
        let (code, text) = run(path, arg)?;
        let d = diff(&text, want);
        kassert!(code == 0 && d.is_empty(), "{path} {arg} differs from CachyOS (exit {code}):\n{}", d.join("\n"));
        klog::event!(Info, "ktest", "l7.program", path = path, mode = arg, lines = text.lines().count());
    }
    Ok(())
}

ktest!("ab.l7.execdump", Safe, execdump);
/// Item 1: a run-only program is not dumpable for its owner (linux-7.3.1/fs/exec.c:1359 `would_dump`).
/// "nodac" needs root, which CachyOS's run has not; its lines are Linux's by reading `generic_permission`
/// (fs/namei.c:521): uid 0 without CAP_DAC_READ_SEARCH/CAP_DAC_OVERRIDE gets only the others' bits.
fn execdump() -> TestResult {
    programs(&[
        ("/bin/abi-execdump", "", include_str!("../../../user/abi-tests/expected/l7-execdump.txt")),
        ("/bin/abi-execdump", "nodac", NODAC),
    ])
}

const NODAC: &str = "root, run-only file of uid 1000: child dumpable = yes
root, run-only file of uid 1000: exit status 0x0
root without DAC capabilities, run-only file of uid 1000: child dumpable = no
root without DAC capabilities, run-only file of uid 1000: exit status 0x0
";

ktest!("ab.l7.pidfs", Safe, pidfs);
/// Item 2: pidfs file handles (linux-7.3.1/fs/fhandle.c, fs/pidfs.c:881) and setns(pidfd, several CLONE_NEW*)
/// (kernel/nsproxy.c:569).
fn pidfs() -> TestResult {
    programs(&[
        ("/bin/abi-pidfs", "handle", include_str!("../../../user/abi-tests/expected/l7-pidfs-handle.txt")),
        ("/bin/abi-pidfs", "setns", include_str!("../../../user/abi-tests/expected/l7-pidfs-setns.txt")),
    ])
}

ktest!("ab.l7.bracketed_paste", Safe, bracketed_paste);
/// Item 3: the VT keeps DEC mode 2004 for TIOCL_GETBRACKETEDPASTE (linux-7.3.1/drivers/tty/vt/vt.c:2040).
fn bracketed_paste() -> TestResult {
    let was = console::vt::bracketed_paste();
    console::vt::write("\x1b[?2004h", 7);
    let on = console::vt::bracketed_paste();
    console::vt::write("\x1b[?2004l", 7);
    let off = console::vt::bracketed_paste();
    if was {
        console::vt::write("\x1b[?2004h", 7);
    }
    kassert!(on && !off, "CSI ? 2004 h/l on the VT (none without a display): on after h {on}, still on after l {off}");
    Ok(())
}
