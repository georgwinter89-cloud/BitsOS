//! GM-PROTON ktests (`gm.wine.*`): the kernel sequences Proton's Wine runs on its 64-bit path, as the `abi-wine`
//! program (user/abi-tests/c/wine.c) prints them, against CachyOS's transcripts (`expected/wine-*.txt`): the
//! preloader's reservations, fault decoding, the TEB in GS, thread suspend, fsync's shared waits and the seccomp
//! filter by instruction pointer (plan/notes/gm-PROTON.md 1.4).
//!
//! `Safe` and rerunnable: each mode is its own process (the seccomp filter dies with it), ended with a deadline.

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use ktest::{TestResult, kassert, ktest};

const SEC: u64 = 1_000_000_000;

fn have(path: &str) -> bool {
    vfs::root_ns().lookup(path).is_ok()
}

fn diff(got: &str, want: &str) -> Vec<String> {
    let g: Vec<&str> = got.lines().collect();
    let mut d: Vec<String> = want
        .lines()
        .enumerate()
        .filter_map(|(i, w)| match g.get(i) {
            Some(l) if *l == w => None,
            Some(l) => Some(format!("line {}: got {l:?}, want {w:?}", i + 1)),
            None => Some(format!("line {}: missing, want {w:?}", i + 1)),
        })
        .collect();
    if g.len() > want.lines().count() {
        d.push(format!("{} extra lines", g.len() - want.lines().count()));
    }
    d
}

ktest!("gm.wine.programs", Safe, programs);
fn programs() -> TestResult {
    if !have("/bin/abi-wine") {
        if have("/bin/abi-ids") {
            return Err(String::from("the image has ABI programs but not /bin/abi-wine (did it fail to build?)"));
        }
        return ktest::fail("the user/abi-tests programs are not in this image (no musl sysroot when it was built)");
    }
    let modes: [(&str, &str); 6] = [
        ("reserve", include_str!("../../../user/abi-tests/expected/wine-reserve.txt")),
        ("faults", include_str!("../../../user/abi-tests/expected/wine-faults.txt")),
        ("gs", include_str!("../../../user/abi-tests/expected/wine-gs.txt")),
        ("suspend", include_str!("../../../user/abi-tests/expected/wine-suspend.txt")),
        ("fsync", include_str!("../../../user/abi-tests/expected/wine-fsync.txt")),
        ("seccomp", include_str!("../../../user/abi-tests/expected/wine-seccomp.txt")),
    ];
    for (mode, want) in modes {
        let out = proc::stdio::Capture::new(1 << 16);
        let argv = ["/bin/abi-wine", mode];
        let env = ["HOME=/", "PATH=/bin", "ABI_TEST=1"];
        let mut o = proc::SpawnOptions::new(&argv, &env, policy::ProgramId(1));
        o.stdin = Some(proc::stdio::Input::new(Vec::new()) as Arc<dyn vfs::File>);
        o.stdout = Some(out.clone() as Arc<dyn vfs::File>);
        o.stderr = Some(out.clone() as Arc<dyn vfs::File>);
        let p = proc::spawn_with("/bin/abi-wine", &o).map_err(|e| format!("spawn: {e}"))?;
        let Some(code) = p.wait(Some(120 * SEC)) else {
            p.kill(9);
            let _ = p.wait(Some(10 * SEC));
            return Err(format!("abi-wine {mode} did not end within 120 s:\n{}", String::from_utf8_lossy(&out.contents())));
        };
        let text = String::from_utf8_lossy(&out.contents()).into_owned();
        let d = diff(&text, want);
        kassert!(code == 0 && d.is_empty(), "abi-wine {mode} differs from CachyOS (exit {code}):\n{}", d.join("\n"));
        klog::event!(Info, "ktest", "gm.wine.program", mode = mode, lines = text.lines().count());
    }
    Ok(())
}
