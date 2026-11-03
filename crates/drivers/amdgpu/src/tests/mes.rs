//! GX-7k: the MES stage on the host. The ktests' checks (`ktests_mes`), Linux's MES start against the 27.10.
//! capture (the fixture tests/data/linux-780m-mes-writes.txt, public; compared with the capture itself where
//! plan/ is), and the register map against the generated tables.

use super::regdiff::{capture, table_names};
use crate::gpucore::fakechip::{self, StartAs};
use crate::gpucore::mes::{self, MesAddrs};
use crate::gpucore::psp::Until;
use crate::gpucore::regdiff::{self, Names as _};
use crate::ipmod::mes11::M;
use crate::ktests_mes as k;
use std::string::String;
use std::vec::Vec;

#[test]
fn starts_sizes_and_after_linux() {
    k::check_starts(&fakechip::discovery_780m()).unwrap();
}

#[test]
fn failures_end_halted() {
    k::check_failures(&fakechip::discovery_780m()).unwrap();
}

#[test]
fn again_stop_quiesce_and_stale_hqds() {
    k::check_again(&fakechip::discovery_780m()).unwrap();
}

#[test]
fn values() {
    k::check_values().unwrap();
}

/// Every register the stage names is the generated table's name at the 780M's GC bases.
#[test]
fn register_map_is_the_generated_tables() {
    let names = table_names();
    let a = MesAddrs::from_discovery(&fakechip::discovery_780m()).unwrap();
    for r in M::ALL {
        let b = a.byte(r).unwrap();
        assert!(names.names(b).iter().any(|n| n == r.name()), "{:?} at {b:#x}: tables say {:?}", r, names.names(b));
    }
}

fn phase() -> &'static regdiff::PhaseDef {
    regdiff::PHASES.iter().find(|p| p.name == "mes").unwrap()
}

fn any_rule(ph: &regdiff::PhaseDef, names: &[String]) -> bool {
    ph.rules.iter().any(|(p, r)| *r == regdiff::Rule::Any && names.iter().any(|n| n.starts_with(p)))
}

fn fixture_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/linux-780m-mes-writes.txt")
}

/// Linux's MES start in the capture (the phase's writes, addresses written as 0).
fn fixture() -> Vec<(u32, u32)> {
    let text = std::fs::read_to_string(fixture_path()).unwrap_or_else(|e| panic!("{}: {e}", fixture_path().display()));
    text.lines().filter_map(crate::ktests_bringup::w_line).collect()
}

fn fixture_text(linux: &[(u32, u32)], names: &super::regdiff::Names) -> String {
    let mut s = String::from(
        "# Linux amdgpu load on the 780M (27.10.2026 capture, trace-load 41680-41715): mes_v11_0_enable, the firmware\n# versions, kiq_setting and the KIQ's queue_init_register (regtrace phase mes). W <BAR5 byte> <value>; addresses as 0.\n# Test: tests::mes.\n",
    );
    let ph = phase();
    for (_, b, v) in regdiff::select(ph, linux, names) {
        let n = names.names(b);
        let v = if any_rule(ph, n) { 0 } else { v };
        s += &std::format!("W {b:#x} {v:#x} # {}\n", n.first().map_or("?", |x| x.as_str()));
    }
    s
}

#[test]
fn mes_fixture_is_the_capture() {
    let Some(t) = capture() else {
        std::eprintln!("SKIPPED mes_fixture_is_the_capture: no capture (plan/ is not public); the fixture stands in");
        return;
    };
    let want = fixture_text(&t.writes(false), &table_names());
    if std::env::var_os("GX_WRITE_FIXTURE").is_some() {
        std::fs::write(fixture_path(), &want).unwrap();
    }
    let have = std::fs::read_to_string(fixture_path()).unwrap_or_default();
    assert!(have == want, "{} is not the capture's MES start; it should read:\n{want}", fixture_path().display());
}

/// Our MES start against Linux's, write for write: the plan from what the stage found and the run's own writes
/// (both `until=mes` and `until=gfx_mes`, at every carve-out size) match the capture's 29 writes of phase mes.
#[test]
fn mes_phase_against_linux() {
    let d = fakechip::discovery_780m();
    let names = table_names();
    let linux = fixture();
    let a = MesAddrs::from_discovery(&d).unwrap();
    for (until, size) in [(Until::Mes, 512u64 << 20), (Until::GfxMes, 16 << 30)] {
        let (out, ..) = k::mes_run(&d, size, StartAs::Cold, until, &|_| {}).unwrap();
        let r = out.mes.as_ref().unwrap();
        let done: Vec<(u32, u32)> = out.entries.iter().map(|e| (e.byte, e.value)).collect();
        for ours in [mes::linux_sequence(r, &a), done] {
            let ph = regdiff::diff_phase(phase(), &linux, &ours, &names, &[]);
            std::eprintln!("{until:?}: {}", regdiff::render(core::slice::from_ref(&ph)));
            assert!(ph.linux == 29 && ph.matched == 29 && ph.diffs.is_empty(), "{until:?}: {}", regdiff::render(core::slice::from_ref(&ph)));
        }
    }
    if let Some(t) = capture() {
        let (out, ..) = k::mes_run(&d, 512 << 20, StartAs::Cold, Until::Mes, &|_| {}).unwrap();
        let ours = mes::linux_sequence(out.mes.as_ref().unwrap(), &a);
        let ph = regdiff::diff_phase(phase(), &t.writes(false), &ours, &names, &[]);
        assert!(ph.diffs.is_empty() && ph.matched == 29, "{}", regdiff::render(core::slice::from_ref(&ph)));
    }
}

/// The tool's answer for until=mes and gfx_mes ends with the stage's verdict line; the W lines are the run's.
#[test]
fn answer_lines() {
    let d = fakechip::discovery_780m();
    let (out, ..) = k::mes_run(&d, 512 << 20, StartAs::Cold, Until::GfxMes, &|_| {}).unwrap();
    let text = crate::gpucore::mes::render(out.mes.as_ref().unwrap());
    for want in ["mes found: CP_MES_CNTL 0x40000000", "mes versions: scheduler 0x0102508b", "mes kiq_setting: RLC_CP_SCHEDULERS 0x3038 -> 0x30e8", "mes mapped: GFX ring 0", "mes verdict (gfx_mes): PASS"] {
        assert!(text.iter().any(|l| l.starts_with(want)), "no line {want:?} in {text:#?}");
    }
    assert_eq!(crate::psplive::mes_verdict_line(Until::Mes, true), "mes verdict (mes): PASS");
    assert_eq!(crate::psplive::mes_verdict_line(Until::GfxMes, false), "mes verdict (gfx_mes): FAIL");
}
