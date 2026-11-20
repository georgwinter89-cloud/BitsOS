//! Wave 44, item ME: the flush after a GART change once MES runs, held against the whole load trace of 18.11.
//! (plan/hw/gaming-pc-gpu-2026-11-18/trace-load.txt.zst): after MES's start the CPU writes no engine 17 request of
//! either hub at all, and a bind's flush is three HDP flushes and nothing else between them (trace-load lines
//! 106997-107002, the first bind after the init's end). The init trace has no bind after MES, so the kernel's
//! unit (`linuxseq::bind_flush_mes`, the HDP line of section mes three times) is pinned to this here.

use amdgpu::rdna4::linuxseq::{What, bind_flush_mes};
use std::path::PathBuf;
use std::process::{Command, Stdio};

/// (dword, value) of a register write or read line of the trace-load (`amdgpu_device_wreg: 0x7550, 0x.., 0x..`).
fn access(line: &str, kind: &str) -> Option<(u32, u32)> {
    let rest = line.split(&format!("amdgpu_device_{kind}: 0x7550, ")).nth(1)?;
    let mut p = rest.split(", ");
    let dword = u32::from_str_radix(p.next()?.trim().strip_prefix("0x")?, 16).ok()?;
    let value = u32::from_str_radix(p.next()?.trim().strip_prefix("0x")?, 16).ok()?;
    Some((dword, value))
}

/// The dword of a register as a fixture of the kernel names it.
fn dword_of(fixture: &str, name: &str) -> u32 {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/rdna4/linuxseq");
    let text = std::fs::read_to_string(dir.join(fixture)).unwrap();
    let line = text.lines().find(|l| l.split(' ').nth(2) == Some(name)).unwrap_or_else(|| panic!("{fixture} names no {name}"));
    u32::from_str_radix(line.split(' ').nth(3).unwrap().strip_prefix("0x").unwrap(), 16).unwrap()
}

#[test]
fn after_mes_starts_a_binds_flush_is_three_hdp_flushes_and_no_request_of_the_cpu() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..").canonicalize().expect("the repository root");
    let trace = root.join("plan/hw/gaming-pc-gpu-2026-11-18/trace-load.txt.zst");
    let out = Command::new("zstd").arg("-dc").arg(&trace).stdin(Stdio::null()).output().expect("zstd missing (install zstd)");
    assert!(out.status.success(), "zstd failed on {}", trace.display());
    let text = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = text.lines().collect();
    let (gc_req, mm_req) = (dword_of("gc-hub.txt", "GCVM_INVALIDATE_ENG17_REQ"), dword_of("mmhub.txt", "MMVM_INVALIDATE_ENG17_REQ"));
    let mes_cntl = dword_of("mes.txt", "CP_MES_CNTL");
    let first_mes = lines.iter().position(|l| access(l, "wreg").is_some_and(|(d, _)| d == mes_cntl)).expect("the trace starts MES");
    let last_req = lines.iter().rposition(|l| access(l, "wreg").is_some_and(|(d, _)| d == gc_req || d == mm_req)).expect("the trace flushes by registers before MES");
    assert!(last_req < first_mes, "a request written by the CPU at trace-load line {} after MES's start at {}", last_req + 1, first_mes + 1);
    assert_eq!((last_req + 1, first_mes + 1), (104042, 104201), "the last register flush is gchub's, MES starts at its CP_MES_CNTL");
    // The kernel's unit: three HDP flushes through Linux's remapped register, each with the memory size's read.
    let unit = bind_flush_mes().unwrap();
    let (flush, mem) = match unit[0].what {
        What::Hdp { flush, mem, .. } => (flush, mem),
        other => panic!("{other:?}"),
    };
    assert!(unit.len() == 3 && unit.iter().all(|l| matches!(l.what, What::Hdp { flush: f, mem: m, .. } if (f, m) == (flush, mem))));
    // The first bind after the init (a buffer created in GTT right before it): exactly that, line for line.
    let at = 106_997 - 1;
    assert!(lines[at - 1].contains("amdgpu_bo_create:") && lines[at + 6].contains("amdgpu_bo_move:"), "{}\n{}", lines[at - 1], lines[at + 6]);
    for k in 0..3 {
        assert_eq!(access(lines[at + 2 * k], "wreg"), Some((flush, 0)), "line {}", at + 2 * k + 1);
        assert_eq!(access(lines[at + 2 * k + 1], "rreg").map(|a| a.0), Some(mem), "line {}", at + 2 * k + 2);
    }
}
