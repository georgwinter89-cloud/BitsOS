//! GX-7h: the GMC stage on the host. The ktests' checks (`ktests_gmc`, also on the real discovery blob when it
//! is here), Linux's values against the 27.10. capture (the fixture tests/data/linux-780m-gmc-writes.txt, public;
//! compared with the capture itself where plan/ is), the register map against the generated tables, and the
//! pure parts (plan decision, CRC compare, watcher passes).

use super::regdiff::{capture, table_names};
use crate::gpucore::fakechip::{self, StartAs};
use crate::gpucore::gmc::{self, GmcAddrs, MmDecision, Planned};
use crate::gpucore::regdiff::{self, DiffKind, Names as _};
use crate::gpucore::watch::{Class, DisplayGuard, Mode, Watch};
use crate::ipmod::gmc11::{self, R};
use crate::ktests_gmc as k;
use std::string::String;
use std::vec::Vec;

#[test]
fn starts_and_sizes() {
    k::check_starts(&fakechip::discovery_780m()).unwrap();
}

#[test]
fn mmhub_only_where_it_differs() {
    k::check_mmhub(&fakechip::discovery_780m()).unwrap();
}

#[test]
fn refusals_failures_and_reuse() {
    k::check_failures(&fakechip::discovery_780m()).unwrap();
}

#[test]
fn tables_and_fault_decode() {
    k::check_tables(&fakechip::discovery_780m()).unwrap();
    k::check_decode().unwrap();
}

/// Linux's read-modify-writes give the capture's values from the capture's first reads.
#[test]
fn linux_values_from_the_capture() {
    assert_eq!(gmc11::l1_tlb(0x501), 0x1859);
    assert_eq!(gmc11::l2_cntl(0x8_0602), 0x8_0e01);
    assert_eq!(gmc11::l2_cntl2(0), 3);
    assert_eq!(gmc11::ctx0_cntl(0x01ff_fe00), 0x01ff_fe01);
    assert_eq!(gmc11::ctxn_cntl(0x01ff_fe00), 0x01ff_fe07);
    assert_eq!((gmc11::fault_cntl2(0x2_0000), gmc11::fault_cntl2(0xa_0000)), (0x6_0000, 0xe_0000));
    assert_eq!(gmc11::fault_cntl(0x3fff_fffc), 0x3fff_fffc);
    assert_eq!(gmc11::cp_debug(0x40_0000), 0x40_8000);
    assert_eq!(gmc11::invalidate_req(0, 0), 0x00f8_0001);
    assert_eq!(gmc::gart_location(0x80_1fff_ffff, 512 << 20), Some((0x7fff_0000_0000, 0x7fff_1fff_ffff)));
    assert_eq!(gmc::gart_location(0x82_0000_0000 - 1, 512 << 20), Some((0x7fff_0000_0000, 0x7fff_1fff_ffff)), "8 GiB: the same window");
    assert_eq!(gmc::gart_location(0x7fff_0000_0000, 512 << 20), None, "never over graphics memory");
    assert_eq!(gmc::table_bytes(512 << 20), (1 << 20) + 2 * 4096);
}

/// Every register the stage reads or writes, by its generated name at the 780M's bases.
#[test]
fn register_map_is_the_generated_tables() {
    let names = table_names();
    let a = GmcAddrs::from_discovery(&fakechip::discovery_780m()).unwrap();
    let mut n = 0;
    for h in [a.gfx, a.mm] {
        for b in a.readable().into_iter().chain(a.writable(&DisplayGuard::default())) {
            let Some(r) = h.reg_at(b) else { continue };
            let want = r.name(h.hub);
            assert!(names.names(b).iter().any(|x| *x == want), "{want} @{b:#x}: tables say {:?}", names.names(b));
            n += 1;
        }
        for e in 0..gmc11::ENGINES {
            for r in [R::Req(e), R::Ack(e), R::Sem(e), R::RangeLo(e), R::RangeHi(e)] {
                let b = h.byte(r).unwrap();
                assert!(names.names(b).iter().any(|x| *x == r.name(h.hub)), "{} @{b:#x}", r.name(h.hub));
            }
        }
    }
    assert!(n > 300, "{n}");
}

/// The classifier reads VMID 0's table base at the stage's addresses (a Reload hands a GART-enabled hub on).
#[test]
fn classifier_sees_the_table_base() {
    let d = fakechip::discovery_780m();
    let a = GmcAddrs::from_discovery(&d).unwrap();
    let at: std::collections::BTreeMap<_, _> = crate::gpucore::classify::addresses(&d).into_iter().collect();
    for (h, n) in [(a.mm, "MMVM"), (a.gfx, "GCVM")] {
        assert_eq!(at[std::format!("{n}_CONTEXT0_PAGE_TABLE_BASE_ADDR_LO32").as_str()], h.byte(R::PtBaseLo(0)));
        assert_eq!(at[std::format!("{n}_CONTEXT0_PAGE_TABLE_BASE_ADDR_HI32").as_str()], h.byte(R::PtBaseHi(0)));
    }
}

/// The live owner's lists: GFXHUB's whole plan, MMHUB's without one field the display uses, the flush engines.
#[test]
fn live_lists_never_hold_a_display_field() {
    let d = fakechip::discovery_780m();
    let a = GmcAddrs::from_discovery(&d).unwrap();
    let g = DisplayGuard::from_discovery(&d);
    let w = a.writable(&g);
    assert!(w.iter().all(|&b| g.class(b).is_none_or(|c| !c.is_display())), "a display field is writable");
    let mm_req = a.mm.byte(R::Req(gmc11::CPU_ENGINE)).unwrap();
    assert!(w.contains(&mm_req) && w.contains(&a.mm.byte(R::Sem(gmc11::CPU_ENGINE)).unwrap()) && w.contains(&a.mm.byte(R::PtStartLo(0)).unwrap()));
    assert!(!w.contains(&a.mm.byte(R::Ctx(0)).unwrap()) && !w.contains(&a.mm.byte(R::L1Tlb).unwrap()));
    assert_eq!(g.class(mm_req), Some(Class::MmhubFlush("MMVM_INVALIDATE_ENG_REQ")));
    let r = a.readable();
    assert!(!r.contains(&a.gfx.byte(R::Sem(gmc11::CPU_ENGINE)).unwrap()), "GFXHUB's semaphore is never read");
    assert!(r.iter().all(|&b| crate::state::wide_readable(b)));
}

/// The watcher lets an MMHUB flush engine through once per pass, never a display field.
#[test]
fn watcher_passes_only_flush_engines() {
    use crate::gpucore::psp::Hw;
    use crate::gpucore::pspfake::{FakeGpu, Hostile};
    let d = fakechip::discovery_780m();
    let a = GmcAddrs::from_discovery(&d).unwrap();
    let mut w = Watch::new(FakeGpu::new(Hostile::None), DisplayGuard::from_discovery(&d), Mode::Block);
    let (req, l1) = (a.mm.byte(R::Req(17)).unwrap(), a.mm.byte(R::L1Tlb).unwrap());
    assert!(!w.reg_write(req, 1), "a flush without a pass");
    assert!(!w.allow(l1, 0x1859, "test") && !w.reg_write(l1, 0x1859), "a display field cannot be allowed");
    assert!(w.allow(req, 1, "test") && !w.reg_write(req, 2) && w.reg_write(req, 1) && !w.reg_write(req, 1), "one write of that value");
    assert_eq!((w.granted().len(), w.total()), (1, 4));
    assert!(w.hw().display_writes.is_empty(), "flush engines are not display writes");
}

fn p(r: R, before: u32, value: u32, display: bool) -> Planned {
    Planned { r, byte: 0, before, value, display }
}

#[test]
fn mmhub_decision() {
    assert_eq!(gmc::decide(&[p(R::L1Tlb, 1, 1, true), p(R::L2Cntl2, 0, 3, true)]), MmDecision::Equal, "the L2 action alone is no difference");
    assert!(matches!(gmc::decide(&[p(R::L2Cntl, 1, 2, true), p(R::L2Cntl2, 0, 3, true)]), MmDecision::Stop(l) if l.len() == 2));
    let d = gmc::decide(&[p(R::PtStartLo(0), 0, 5, false), p(R::Ctx(3), 0, 7, false), p(R::FaultCntl, 1, 1, false), p(R::RangeLo(1), 0, 9, false)]);
    let MmDecision::Write(l) = d else { panic!("{d:?}") };
    assert_eq!(l.iter().map(|x| x.r).collect::<Vec<_>>(), [R::Ctx(3), R::RangeLo(1), R::PtStartLo(0)], "context 0 last, equal ones left out");
}

#[test]
fn crc_rules() {
    let (ok, l) = crate::psplive::crc_compare(&[(0, Some(0xaabb)), (1, None)], &[(0, Some(0xaabb)), (1, None)]);
    assert!(ok && l.iter().any(|x| x.contains("OTG1: off")), "{l:?}");
    assert!(!crate::psplive::crc_compare(&[(0, Some(1))], &[(0, Some(2))]).0);
    assert!(!crate::psplive::crc_compare(&[(0, Some(1))], &[(0, None)]).0, "a CRC that stopped");
    assert!(crate::psplive::crc_compare(&[], &[]).0);
}

/// The Linux writes of the GMC phases in the 27.10. capture (tests/data, public): both hubs' gart_enable,
/// set_fault_enable_default and the first VMID 0 flush, in order; address values (table base, default and dummy
/// page) written as 0.
fn fixture() -> Vec<(u32, u32)> {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/linux-780m-gmc-writes.txt");
    let text = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
    text.lines().filter_map(crate::ktests_bringup::w_line).collect()
}

fn gmc_phases() -> Vec<&'static regdiff::PhaseDef> {
    regdiff::PHASES.iter().filter(|p| p.name.starts_with("gmc_")).collect()
}

fn any_rule(names: &[String]) -> bool {
    gmc_phases().iter().any(|ph| ph.rules.iter().any(|(p, r)| *r == regdiff::Rule::Any && names.iter().any(|n| n.starts_with(p))))
}

fn fixture_text(linux: &[(u32, u32)], names: &super::regdiff::Names) -> String {
    let mut s = String::from(
        "# Linux amdgpu load on the 780M (27.10.2026 capture): the GMC phases (MMHUB gart_enable with its first VMID 0 flush,\n# GFXHUB gart_enable with set_fault_enable_default and its flush). W <BAR5 byte> <value>; addresses as 0. Test: tests::gmc.\n",
    );
    for ph in gmc_phases() {
        for (_, b, v) in regdiff::select(ph, linux, names) {
            let n = names.names(b);
            let v = if any_rule(n) { 0 } else { v };
            s += &std::format!("W {b:#x} {v:#x} # {}\n", n.first().map_or("?", |x| x.as_str()));
        }
    }
    s
}

#[test]
fn gmc_fixture_is_the_capture() {
    let Some(t) = capture() else {
        std::eprintln!("SKIPPED gmc_fixture_is_the_capture: no capture (plan/ is not public); the fixture stands in");
        return;
    };
    let want = fixture_text(&t.writes(false), &table_names());
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/linux-780m-gmc-writes.txt");
    let have = std::fs::read_to_string(&p).unwrap_or_default();
    if std::env::var_os("GX_WRITE_FIXTURE").is_some() {
        std::fs::write(&p, &want).unwrap();
    }
    assert!(have == want, "{} is not the capture's GMC phases; it should read:\n{want}", p.display());
}

/// Our stage against Linux's GMC phases: the plan of both hubs and the flushes (what BitsOS writes with nothing
/// equal) matches Linux's writes one for one; what the live run actually wrote on GFXHUB too.
#[test]
fn gmc_phases_against_linux() {
    let d = fakechip::discovery_780m();
    let names = table_names();
    let (out, ..) = k::gart(&d, 512 << 20, StartAs::Cold, &|_| {}).unwrap();
    let r = out.gmc.as_ref().unwrap();
    let a = GmcAddrs::from_discovery(&d).unwrap();
    let ours = gmc::linux_sequence(r, &a);
    let linux = fixture();
    let reports = regdiff::diff(PHASES_GMC(), &linux, &ours, &names, &[]);
    std::eprintln!("{}", regdiff::render(&reports));
    for rep in &reports {
        assert!(rep.linux > 100 && rep.matched == rep.linux && rep.diffs.is_empty(), "{}", regdiff::render(core::slice::from_ref(rep)));
    }
    // The GFXHUB writes the run made are Linux's GFXHUB phase exactly.
    let done: Vec<(u32, u32)> = out.entries.iter().map(|e| (e.byte, e.value)).collect();
    let g = regdiff::diff_phase(gmc_phases()[1], &linux, &done, &names, &[]);
    assert!(g.matched == g.linux && g.diffs.is_empty(), "{}", regdiff::render(&[g.clone()]));
    // MMHUB: nothing written on a cold start (display fields differ): every Linux write is missing.
    let m = regdiff::diff_phase(gmc_phases()[0], &linux, &done, &names, &[]);
    assert!(m.ours == 0 && m.count(DiffKind::Missing) == m.linux, "{m:?}");
    // The real capture (where it is) gives the same verdict.
    if let Some(t) = capture() {
        for rep in regdiff::diff(PHASES_GMC(), &t.writes(false), &ours, &names, &[]) {
            assert!(rep.diffs.is_empty(), "{}", regdiff::render(&[rep.clone()]));
        }
    }
}

#[allow(non_snake_case)]
fn PHASES_GMC() -> &'static [regdiff::PhaseDef] {
    let i = regdiff::PHASES.iter().position(|p| p.name == "gmc_mmhub").unwrap();
    &regdiff::PHASES[i..i + 2]
}
