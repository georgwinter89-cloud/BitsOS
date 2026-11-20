//! GX-10: the fixtures of `rdna4::linuxseq` (src/rdna4/linuxseq/*.txt) are what host/gaming-pc/gx-trace-seq.py makes
//! today from the trace of Linux's cold start (plan/hw/gaming-pc-gpu-2026-11-18/init.txt.zst): byte for byte, the
//! same set of files. A stale or hand-edited fixture, a changed generator or a changed trace fails here; the fix is
//! the command in each fixture's first line. Every fixture parses, also those no stage consumes yet.

use amdgpu::rdna4::linuxseq::{SECTIONS, parse};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn txt(dir: &Path) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for e in std::fs::read_dir(dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display())) {
        let name = e.expect("a directory entry").file_name().to_string_lossy().into_owned();
        if name.ends_with(".txt") {
            out.insert(name);
        }
    }
    out
}

#[test]
fn the_fixtures_are_the_generators_from_the_trace() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..").canonicalize().expect("the repository root");
    let script = root.join("host/gaming-pc/gx-trace-seq.py");
    let trace = root.join("plan/hw/gaming-pc-gpu-2026-11-18/init.txt.zst");
    let kept = root.join("crates/drivers/amdgpu/src/rdna4/linuxseq");
    for f in [&script, &trace] {
        assert!(f.is_file(), "{} missing", f.display());
    }
    // The generator unpacks the trace with zstd: without it nothing is compared, which is a failure.
    Command::new("zstd").arg("-V").stdout(Stdio::null()).stderr(Stdio::null()).status().expect("zstd missing (install zstd)");
    let fresh = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("linuxseq-fresh");
    let _ = std::fs::remove_dir_all(&fresh);
    let out = Command::new("python3").arg(&script).arg(&trace).arg(&fresh).stdin(Stdio::null()).output().expect("python3 missing (install python3)");
    let said = String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "the generator failed:\n{said}");

    let (have, want) = (txt(&kept), txt(&fresh));
    assert_eq!(have, want, "the set of fixtures in git is not the generator's");
    assert!(want.len() >= 17, "{want:?}");
    for name in &want {
        let a = std::fs::read(kept.join(name)).expect("a kept fixture");
        let b = std::fs::read(fresh.join(name)).expect("a fresh fixture");
        assert!(a == b, "{name} is stale: run python3 host/gaming-pc/gx-trace-seq.py plan/hw/gaming-pc-gpu-2026-11-18/init.txt.zst crates/drivers/amdgpu/src/rdna4/linuxseq");
        let text = String::from_utf8(a).expect("a fixture is text");
        let section = name.trim_end_matches(".txt");
        let s = parse(section, &text).unwrap_or_else(|e| panic!("{e}"));
        assert!(!text.contains("/home/") && s.head.contains(" by host/gaming-pc/gx-trace-seq.py; regenerate: python3 host/gaming-pc/gx-trace-seq.py plan/hw/gaming-pc-gpu-2026-11-18/init.txt.zst crates/drivers/amdgpu/src/rdna4/linuxseq"), "{}", s.head);
    }
    // The kernel carries exactly the kept text of its sections.
    for (n, text) in SECTIONS {
        assert_eq!(std::fs::read_to_string(kept.join(format!("{n}.txt"))).expect("a section's fixture"), text, "{n}");
    }
    // Linux's order is the order of the trace: the sections follow one another without a gap in between that holds
    // a register access (the generator fails on a section it cannot cut; here the line ranges are checked).
    let order = ["discovery", "nbif", "mmhub", "sw-init", "ih", "psp-boot", "psp-ring", "smu", "display", "gc-boot", "gc-hub", "gc-cp", "mes", "gc-ring", "sdma", "vcn", "late"];
    assert_eq!(want, order.iter().map(|n| format!("{n}.txt")).collect::<BTreeSet<_>>());
    let mut last = 0;
    for n in order {
        let text = std::fs::read_to_string(kept.join(format!("{n}.txt"))).expect("a fixture");
        let s = parse(n, &text).unwrap_or_else(|e| panic!("{e}"));
        let (first, end) = (s.lines[0].init, s.lines[s.lines.len() - 1].init);
        assert!(first > last && end >= first, "{n}: init lines {first}-{end} after {last}");
        assert!(s.lines.windows(2).all(|w| w[0].init <= w[1].init), "{n}: lines out of order");
        last = end;
    }
}
