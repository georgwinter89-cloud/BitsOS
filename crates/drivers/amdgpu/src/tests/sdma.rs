//! GX-7i: the SDMA stage on the host. The ktests' checks (`ktests_sdma`), Linux's start sequence against the
//! 27.10. capture (the fixture tests/data/linux-780m-sdma-writes.txt, public; compared with the capture itself
//! where plan/ is) and the register map against the generated tables.

use super::regdiff::{capture, table_names};
use crate::gpucore::fakechip::{self, StartAs};
use crate::gpucore::regdiff::{self, DiffKind, Names as _};
use crate::gpucore::sdma::{self, SdmaAddrs};
use crate::ipmod::sdma6::R;
use crate::ktests_sdma as k;
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
fn again_and_halt() {
    k::check_again(&fakechip::discovery_780m()).unwrap();
}

#[test]
fn restart_with_old_pointers() {
    k::check_restart(&fakechip::discovery_780m()).unwrap();
}

#[test]
fn stop_puts_the_doorbell_side_back() {
    k::check_stop_back(&fakechip::discovery_780m()).unwrap();
}

#[test]
fn values() {
    k::check_values().unwrap();
}

/// Every SDMA0 register the stage names is the generated table's name at the 780M's GC bases.
#[test]
fn register_map_is_the_generated_tables() {
    let names = table_names();
    let a = SdmaAddrs::from_discovery(&fakechip::discovery_780m()).unwrap();
    for r in R::ALL {
        let b = a.byte(r).unwrap();
        assert!(names.names(b).iter().any(|n| n == r.name()), "{:?} at {b:#x}: tables say {:?}", r, names.names(b));
    }
    let nb = a.nbio.unwrap();
    assert!(names.names(nb.aper_en).iter().any(|n| n == "RCC_DEV0_EPF0_0_RCC_DOORBELL_APER_EN"), "{:?}", names.names(nb.aper_en));
    assert!(names.names(nb.rsmu_index).iter().any(|n| n == "BIF_BX_PF0_RSMU_INDEX"), "{:?}", names.names(nb.rsmu_index));
}

fn sdma_phase() -> &'static regdiff::PhaseDef {
    regdiff::PHASES.iter().find(|p| p.name == "sdma").unwrap()
}

fn any_rule(names: &[String]) -> bool {
    sdma_phase().rules.iter().any(|(p, r)| *r == regdiff::Rule::Any && names.iter().any(|n| n.starts_with(p)))
}

/// Linux's SDMA start in the capture (tests/data, public): sdma_v6_0_start through the IB enable; ring and
/// writeback addresses written as 0.
fn fixture() -> Vec<(u32, u32)> {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/linux-780m-sdma-writes.txt");
    let text = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
    text.lines().filter_map(crate::ktests_bringup::w_line).collect()
}

fn fixture_text(linux: &[(u32, u32)], names: &super::regdiff::Names) -> String {
    let mut s = String::from(
        "# Linux amdgpu load on the 780M (27.10.2026 capture): SDMA 0's start (sdma_v6_0_start: unhalt, context-empty\n# interrupt, gfx_resume through IB_ENABLE). W <BAR5 byte> <value>; ring and writeback addresses as 0. Test: tests::sdma.\n",
    );
    for (_, b, v) in regdiff::select(sdma_phase(), linux, names) {
        let n = names.names(b);
        let v = if any_rule(n) { 0 } else { v };
        s += &std::format!("W {b:#x} {v:#x} # {}\n", n.first().map_or("?", |x| x.as_str()));
    }
    s
}

#[test]
fn sdma_fixture_is_the_capture() {
    let Some(t) = capture() else {
        std::eprintln!("SKIPPED sdma_fixture_is_the_capture: no capture (plan/ is not public); the fixture stands in");
        return;
    };
    let want = fixture_text(&t.writes(false), &table_names());
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/linux-780m-sdma-writes.txt");
    let have = std::fs::read_to_string(&p).unwrap_or_default();
    if std::env::var_os("GX_WRITE_FIXTURE").is_some() {
        std::fs::write(&p, &want).unwrap();
    }
    assert!(have == want, "{} is not the capture's SDMA start; it should read:\n{want}", p.display());
}

/// Our stage against Linux's start: the plan from what the stage found (doorbell on) matches Linux's writes one
/// for one; the run's first resume (doorbell off) differs in exactly DOORBELL and DOORBELL_OFFSET; its second
/// (doorbell on, after the stop and the restart's pointer reset) matches Linux's writes one for one.
#[test]
fn sdma_phase_against_linux() {
    let d = fakechip::discovery_780m();
    let names = table_names();
    let (out, ..) = k::sdma_run(&d, 512 << 20, StartAs::Cold, &|_| {}).unwrap();
    let r = out.sdma.as_ref().unwrap();
    let a = SdmaAddrs::from_discovery(&d).unwrap();
    let linux = fixture();
    let ours = sdma::linux_sequence(r, &a);
    let rep = regdiff::diff_phase(sdma_phase(), &linux, &ours, &names, &[]);
    std::eprintln!("{}", regdiff::render(core::slice::from_ref(&rep)));
    assert!(rep.linux == 26 && rep.matched == rep.linux && rep.diffs.is_empty(), "{}", regdiff::render(core::slice::from_ref(&rep)));
    let done: Vec<(u32, u32)> = out.entries.iter().map(|e| (e.byte, e.value)).collect();
    let first = regdiff::diff_phase(sdma_phase(), &linux, &done, &names, &[]);
    std::eprintln!("{}", regdiff::render(core::slice::from_ref(&first)));
    // A value difference shows as Linux's write missing and ours extra.
    let ok = first.diffs.iter().all(|x| x.name.starts_with("SDMA0_QUEUE0_DOORBELL")) && first.count(DiffKind::Missing) == 2 && first.count(DiffKind::Extra) == 2;
    assert!(ok && first.matched == 24, "{}", regdiff::render(core::slice::from_ref(&first)));
    // GX-7i2: resume 2 (doorbell on, after the stop) is Linux's start write for write: the halted engine's
    // F32_CNTL (TH0_RESET/TH0_ENABLE set, the fake as the Mini-PC 28.10.) and RB_CNTL's writeback bit are not
    // carried over (they made 0x08084600 and 0x00841816 on the chip). The restart's pointer reset lies before it.
    let (_, a2, b2) = out.sdma_resumes[1];
    let second: Vec<(u32, u32)> = done[a2..b2].to_vec();
    let rep2 = regdiff::diff_phase(sdma_phase(), &linux, &second, &names, &[]);
    std::eprintln!("{}", regdiff::render(core::slice::from_ref(&rep2)));
    assert!(rep2.matched == 26 && rep2.diffs.is_empty(), "{}", regdiff::render(core::slice::from_ref(&rep2)));
    let r = out.sdma.as_ref().unwrap();
    assert!(r.rewinds.len() == 1 && r.rewinds[0].found.0 != 0 && r.rewinds[0].after == (0, 0, 0), "{:x?}", r.rewinds);
    if let Some(t) = capture() {
        let rep = regdiff::diff_phase(sdma_phase(), &t.writes(false), &ours, &names, &[]);
        assert!(rep.diffs.is_empty(), "{}", regdiff::render(core::slice::from_ref(&rep)));
    }
}

/// The OTG CRC is a pass rule of `until=sdma` (and `gart`): a changed CRC fails the run even when every stage
/// and the DCN snapshot passed; earlier milestones only print it.
#[test]
fn crc_fails_the_sdma_run() {
    use crate::gpucore::psp::Until;
    use crate::psplive::{crc_compare, verdict};
    let (same, _) = crc_compare(&[(0, Some(0xaabb))], &[(0, Some(0xaabc))]);
    assert!(!same);
    assert!(!verdict(Until::Sdma, true, true, same), "a CRC change passed until=sdma");
    assert!(!verdict(Until::Gart, true, true, same));
    assert!(verdict(Until::Gfx, true, true, same), "until=gfx only prints the CRC");
    assert!(verdict(Until::Sdma, true, true, true));
    assert!(!verdict(Until::Sdma, true, false, true) && !verdict(Until::Sdma, false, true, true));
}
