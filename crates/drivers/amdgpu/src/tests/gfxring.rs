//! GX-7j: the GFX ring stage on the host. The ktests' checks (`ktests_gfxring`), Linux's GFX tail and CP start
//! against the 27.10. capture (the fixture tests/data/linux-780m-gfx-writes.txt, public; compared with the capture
//! itself where plan/ is), the clear-state buffer against the capture's ring dump, and the register map against
//! the generated tables.

use super::regdiff::{capture, table_names};
use crate::gpucore::fakechip::{self, StartAs};
use crate::gpucore::gfxring::{self, GfxAddrs, Variant};
use crate::gpucore::regdiff::{self, DiffKind, Names as _};
use crate::ipmod::gfx11::R;
use crate::ktests_gfxring as k;
use std::string::String;
use std::vec::Vec;

#[test]
fn starts_and_sizes() {
    k::check_starts(&fakechip::discovery_780m()).unwrap();
}

#[test]
fn failures() {
    k::check_failures(&fakechip::discovery_780m()).unwrap();
}

#[test]
fn restart_through_a_stop_rule() {
    k::check_restart_rule(&fakechip::discovery_780m()).unwrap();
}

#[test]
fn again_restart_and_quiesce_halt() {
    k::check_again(&fakechip::discovery_780m()).unwrap();
}

#[test]
fn stop_puts_the_doorbell_side_back() {
    k::check_stop_back(&fakechip::discovery_780m()).unwrap();
}

#[test]
fn start_variants() {
    k::check_variants(&fakechip::discovery_780m()).unwrap();
}

#[test]
fn variant_failures_end_halted() {
    k::check_variant_failures(&fakechip::discovery_780m()).unwrap();
}

#[test]
fn after_an_sdma_stage() {
    k::check_after_sdma(&fakechip::discovery_780m()).unwrap();
}

#[test]
fn values() {
    k::check_values().unwrap();
}

/// Every register the stage names is the generated table's name at the 780M's GC bases (CGTT_WD_CLK_CTRL and
/// TCP_CNTL are not in gc_11_0_0's header: gfx_v11_0.c's own define and gc_11_0_3's; their bytes must be free).
#[test]
fn register_map_is_the_generated_tables() {
    let names = table_names();
    let a = GfxAddrs::from_discovery(&fakechip::discovery_780m()).unwrap();
    for r in R::all() {
        let b = a.byte(r).unwrap();
        if matches!(r, R::CgttWdClkCtrl | R::TcpCntl) {
            assert!(names.names(b).is_empty(), "{:?} at {b:#x}: tables say {:?}", r, names.names(b));
            continue;
        }
        assert!(names.names(b).iter().any(|n| n == r.name()), "{:?} at {b:#x}: tables say {:?}", r, names.names(b));
    }
    assert!(names.names(a.aper_en.unwrap()).iter().any(|n| n == "RCC_DEV0_EPF0_0_RCC_DOORBELL_APER_EN"));
}

fn phase(name: &str) -> &'static regdiff::PhaseDef {
    regdiff::PHASES.iter().find(|p| p.name == name).unwrap()
}

fn any_rule(ph: &regdiff::PhaseDef, names: &[String]) -> bool {
    ph.rules.iter().any(|(p, r)| *r == regdiff::Rule::Any && names.iter().any(|n| n.starts_with(p)))
}

/// Linux's GFX tail and CP start in the capture (tests/data, public): the gfx_tail and gfx_ring phases' writes,
/// addresses written as 0.
fn fixture() -> Vec<(u32, u32)> {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/linux-780m-gfx-writes.txt");
    let text = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
    text.lines().filter_map(crate::ktests_bringup::w_line).collect()
}

fn fixture_text(linux: &[(u32, u32)], names: &super::regdiff::Names) -> String {
    let mut s = String::from(
        "# Linux amdgpu load on the 780M (27.10.2026 capture): GFX hw_init after gfxhub_enable (golden registers,\n# constants_init, the RLC resume of the PSP path), then cp_set_doorbell_range through cp_gfx_start's CP_DEVICE_ID\n# (Linux 7.1 maps the GFX queue through MES in between). W <BAR5 byte> <value>; addresses as 0. Test: tests::gfxring.\n",
    );
    for ph in [phase("gfx_tail"), phase("gfx_ring")] {
        for (_, b, v) in regdiff::select(ph, linux, names) {
            let n = names.names(b);
            let v = if any_rule(ph, n) { 0 } else { v };
            s += &std::format!("W {b:#x} {v:#x} # {}\n", n.first().map_or("?", |x| x.as_str()));
        }
    }
    s
}

#[test]
fn gfx_fixture_is_the_capture() {
    let Some(t) = capture() else {
        std::eprintln!("SKIPPED gfx_fixture_is_the_capture: no capture (plan/ is not public); the fixture stands in");
        return;
    };
    let want = fixture_text(&t.writes(false), &table_names());
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/linux-780m-gfx-writes.txt");
    if std::env::var_os("GX_WRITE_FIXTURE").is_some() {
        std::fs::write(&p, &want).unwrap();
    }
    let have = std::fs::read_to_string(&p).unwrap_or_default();
    assert!(have == want, "{} is not the capture's GFX tail; it should read:\n{want}", p.display());
}

/// The writes the phases cannot select (no table name) and the CP's unhalt, against the capture by byte: Linux
/// 7.1 writes each once in the window, with our value.
#[test]
fn unnamed_golden_and_unhalt_are_the_capture() {
    let Some(t) = capture() else {
        std::eprintln!("SKIPPED unnamed_golden_and_unhalt_are_the_capture: no capture (plan/ is not public); check_values has the values");
        return;
    };
    let a = GfxAddrs::from_discovery(&fakechip::discovery_780m()).unwrap();
    let w = t.writes(false);
    let start = w.iter().position(|&(b, _)| Some(b) == a.byte(R::CgttGsNggClkCtrl)).unwrap();
    let find = |r: R| w[start..].iter().find(|&&(b, _)| Some(b) == a.byte(r)).map(|x| x.1);
    assert_eq!(find(R::CgttWdClkCtrl), Some(0x10));
    assert_eq!(find(R::TcpCntl), Some(0x239c_0020));
    assert_eq!(find(R::CpMeCntl), Some(0x0100_0000), "Linux's unhalt (async path) is ours (cp_gfx_enable)");
// GX-7j2: MEC ME1 unhalted first (cp_compute_enable), from the cold 0x40000000, as gfx11::mec_cntl writes it.
    assert_eq!(find(R::CpMecCntl), Some(crate::ipmod::gfx11::mec_cntl(0x4000_0000, true)));
    let pos = |r: R| w[start..].iter().position(|&(b, _)| Some(b) == a.byte(r));
    assert!(pos(R::CpRbDoorbellRangeUpper) < pos(R::CpMecCntl) && pos(R::CpMecCntl) < pos(R::CpMeCntl), "the capture's order: doorbell ranges, MEC, ME");
}

/// The clear-state buffer is the capture's: the first 960 dwords of the debugfs dump of GFX ring 0 (after its
/// 12-byte header), where cp_gfx_start wrote the same stream; RLC_CSIB_LENGTH was 960 there.
#[test]
fn csb_is_the_capture() {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../plan/hw/gx-780m-linux-trace-2026-10-27/gx-6d9cae1d/bundle/load/rings/amdgpu_ring_gfx_0.0.0");
    let Ok(ring) = std::fs::read(&p) else {
        std::eprintln!("SKIPPED csb_is_the_capture: {} missing (plan/ is not public)", p.display());
        return;
    };
    assert!(ring.len() >= 12 + gfxring::CSB.len());
    assert!(&ring[12..12 + gfxring::CSB.len()] == gfxring::CSB, "gfx11_csb.bin differs from the capture's ring dump");
    if let Some(t) = capture() {
        let a = GfxAddrs::from_discovery(&fakechip::discovery_780m()).unwrap();
        let len = t.writes(false).into_iter().find(|&(b, _)| Some(b) == a.byte(R::RlcCsibLength)).map(|x| x.1);
        assert_eq!(len, Some(gfxring::CSB_DW));
    }
}

/// Our stage against Linux: the tail from what the stage found is Linux's write for write (gfx_tail); the CP part
/// has Linux's six writes in place (the doorbell ranges, CP_MAX_CONTEXT, CP_DEVICE_ID) and the direct path's
/// CP_RB0/CP_RB_ writes as the only differences, all extra (Linux 7.1 maps the ring through MES). The run's own
/// writes give the same.
#[test]
fn gfx_phases_against_linux() {
    let d = fakechip::discovery_780m();
    let names = table_names();
    let (out, ..) = k::gfx_run(&d, 512 << 20, StartAs::Cold, &|_| {}).unwrap();
    let r = out.gfx_ring.as_ref().unwrap();
    let a = GfxAddrs::from_discovery(&d).unwrap();
    let linux = fixture();
    let done: Vec<(u32, u32)> = out.entries.iter().map(|e| (e.byte, e.value)).collect();
    for ours in [gfxring::linux_sequence(r, &a), done] {
        let tail = regdiff::diff_phase(phase("gfx_tail"), &linux, &ours, &names, &[]);
        std::eprintln!("{}", regdiff::render(core::slice::from_ref(&tail)));
        assert!(tail.linux == 165 && tail.matched == tail.linux && tail.diffs.is_empty(), "{}", regdiff::render(core::slice::from_ref(&tail)));
        let ring = regdiff::diff_phase(phase("gfx_ring"), &linux, &ours, &names, &[]);
        std::eprintln!("{}", regdiff::render(core::slice::from_ref(&ring)));
        let extra_ok = ring.diffs.iter().all(|x| x.kind == DiffKind::Extra && (x.name.contains("CP_RB0_") || x.name.contains("CP_RB_")));
        assert!(ring.linux == 6 && ring.matched == 6 && ring.count(DiffKind::Extra) == 16 && extra_ok, "{}", regdiff::render(core::slice::from_ref(&ring)));
    }
    // GX-7j2: e1c writes what GX-7j wrote (MEC is no phase register); e1a writes Linux's four doorbell ranges and
    // nothing of the ring (CP_MAX_CONTEXT and CP_DEVICE_ID missing: Linux 7.1 writes them after MES started).
    for (v, matched, missing, extra) in [(Variant::E1c, 6, 0, 16), (Variant::E1a, 4, 2, 0)] {
        let (out, ..) = k::gfx_run_v(&d, 512 << 20, StartAs::Cold, v, &|_| {}).unwrap();
        let r = out.gfx_ring.as_ref().unwrap();
        let done: Vec<(u32, u32)> = out.entries.iter().map(|e| (e.byte, e.value)).collect();
        for ours in [gfxring::linux_sequence(r, &a), done] {
            let ring = regdiff::diff_phase(phase("gfx_ring"), &linux, &ours, &names, &[]);
            std::eprintln!("{}: {}", v.name(), regdiff::render(core::slice::from_ref(&ring)));
            assert!(
                ring.linux == 6 && ring.matched == matched && ring.count(DiffKind::Missing) == missing && ring.count(DiffKind::Extra) == extra,
                "{}: {}",
                v.name(),
                regdiff::render(core::slice::from_ref(&ring))
            );
        }
    }
    if let Some(t) = capture() {
        let ours = gfxring::linux_sequence(r, &a);
        let tail = regdiff::diff_phase(phase("gfx_tail"), &t.writes(false), &ours, &names, &[]);
        assert!(tail.diffs.is_empty(), "{}", regdiff::render(core::slice::from_ref(&tail)));
    }
}

/// The OTG CRC is a pass rule of `until=gfx_ring` (as of `sdma` and `gart`).
#[test]
fn crc_fails_the_gfx_ring_run() {
    use crate::gpucore::psp::Until;
    use crate::psplive::verdict;
    assert!(!verdict(Until::GfxRing, true, true, false) && verdict(Until::GfxRing, true, true, true));
    assert!(Until::GfxRing.loads_fw() && Until::GfxRing.runs_gc() && Until::GfxRing.runs_gmc() && Until::GfxRing.after_gart());
}

/// GX-7j3: the direct path's MQD pointer (the CPG page-0 read) on the fake's table.
#[test]
fn mqd_pointer_no_page0_read() {
    k::check_mqd_pointer(&fakechip::discovery_780m()).unwrap();
}
