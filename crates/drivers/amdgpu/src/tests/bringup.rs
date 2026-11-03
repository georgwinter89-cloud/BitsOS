//! GX-7d host tests: the PSP run the desk starts on the Mini-PC (`gx_bringup`), through the same engine and owner
//! stack as the live run, against the fakes (the ktests' checks, on the fake's and the real discovery table),
//! the pure parts of the live glue, and our PSP writes against Linux's in the 27.10. capture (cold and stale).

use super::regdiff::{capture, table_names};
use crate::gpucore::classify::Start;
use crate::gpucore::disc;
use crate::gpucore::fakechip::{self, StartAs};
use crate::gpucore::psp::{Prior, PspError, Until};
use crate::gpucore::pspbringup;
use crate::gpucore::pspfake::Hostile;
use crate::gpucore::regdiff::{self, Allow, DiffKind, PHASES};
use crate::gpucore::ucode::ListOpts;
use crate::ktests_bringup as k;
use crate::psplive::{self, Action, Args, CarveWin, CHUNK};
use std::vec::Vec;

fn blob() -> Option<disc::Discovery> {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../host/tests/gx/fixtures/minipc-780m-discovery.bin");
    std::fs::read(p).ok().map(|b| disc::parse(&b).unwrap())
}

#[test]
fn every_start_state_and_size() {
    k::check_starts(&fakechip::discovery_780m()).unwrap();
    if let Some(d) = blob() {
        k::check_starts(&d).unwrap();
    }
}

#[test]
fn m1_kept_quiesce_forbidden_and_blocked() {
    k::check_modes(&fakechip::discovery_780m()).unwrap();
}

#[test]
fn every_failure_answer() {
    k::check_failures(&fakechip::discovery_780m()).unwrap();
}

#[test]
fn admission_and_prior() {
    assert_eq!(pspbringup::admit(Start::Cold), Ok(false));
    for s in [Start::AfterLinux, Start::AfterBitsOs, Start::AfterCrash] {
        assert_eq!(pspbringup::admit(s), Ok(true), "{s:?}");
    }
    assert!(pspbringup::admit(Start::PspDown).is_err() && pspbringup::admit(Start::Unknown).is_err());
    assert_eq!(Prior::of(0, 0, false), Prior::Fresh);
    assert_eq!(Prior::of(0x1000, 0, false), Prior::Stale);
    assert_eq!(Prior::of(0, 0x40, false), Prior::Stale);
    assert_eq!(Prior::of(0, 0, true), Prior::Stale);
}

#[test]
fn reboot_policy() {
    let t = |what| PspError::Timeout { what, last: 0 };
    for e in [t("ring.ready"), t("ring.create"), t("fence"), PspError::Wedged, PspError::RingNotReset { wptr: 0x40 }] {
        assert!(psplive::needs_reboot(&e), "{e:?}");
    }
    for e in [PspError::CmdStatus { cmd: 6, status: 1, ucode: None }, PspError::SosNotRunning, PspError::Forbidden(crate::gpucore::ucode::UcodeId::Dmcub)] {
        assert!(!psplive::needs_reboot(&e), "{e:?}");
    }
    assert!(pspbringup::advice(&t("ring.ready")).contains("reboot"));
}

#[test]
fn carve_window_offsets_and_pieces() {
    // Any size: 512 MiB, 8 GiB, 16 GiB; the top 64 KiB (closed to the CPU) is never reached.
    for size in [512u64 << 20, 8 << 30, 16 << 30] {
        let w = CarveWin { mc_base: 0x80_0000_0000, phys_base: 0x8_4000_0000, size };
        assert_eq!(w.offsets(w.mc_base, 4), Some(0..4));
        assert_eq!(w.offsets(w.mc_base + size - (64 << 10) - 4, 4), Some(size - (64 << 10) - 4..size - (64 << 10)));
        assert_eq!(w.offsets(w.mc_base + size - (64 << 10), 1), None);
        assert_eq!(w.offsets(w.mc_base - 1, 4), None);
        assert_eq!(w.offsets(u64::MAX - 2, 4), None);
    }
    let p: Vec<_> = CarveWin::pieces(CHUNK - 8..CHUNK + 4).collect();
    assert_eq!(p, [(0, (CHUNK - 8) as usize, 8, 0), (1, 0, 4, 8)]);
    let p: Vec<_> = CarveWin::pieces(0..3 * CHUNK).collect();
    assert_eq!(p.len(), 3);
    assert_eq!(CarveWin::pieces(5..5).count(), 0);
}

#[test]
fn tool_arguments() {
    let a = |t: &str| psplive::parse_args(&mcp::Json::parse(t).unwrap());
    assert_eq!(a("{}"), Ok(Args { action: Action::Run(Until::Ring), keep: false, again: false, variant: None, ack_linux: false }));
    assert_eq!(a(r#"{"until":"psp","keep":true}"#), Ok(Args { action: Action::Run(Until::Fw), keep: true, again: false, variant: None, ack_linux: false }));
    assert_eq!(a(r#"{"until":"stop"}"#).unwrap().action, Action::Stop);
    assert_eq!(a(r#"{"until":"m1","again":true}"#).unwrap(), Args { action: Action::Run(Until::Ring), keep: false, again: true, variant: None, ack_linux: false });
    assert!(a(r#"{"until":"dmcub"}"#).is_err() && a(r#"{"keep":3}"#).is_err());
    // GX-7e.
    assert_eq!(a(r#"{"until":"gfx"}"#), Ok(Args { action: Action::Run(Until::Gfx), keep: false, again: false, variant: None, ack_linux: false }));
    assert_eq!(a(r#"{"until":"m3","keep":true,"again":true}"#), Ok(Args { action: Action::Run(Until::Gfx), keep: true, again: true, variant: None, ack_linux: false }));
    assert_eq!(a(r#"{"until":"reset"}"#).unwrap().action, Action::Reset);
    // GX-7h.
    assert_eq!(a(r#"{"until":"gart","again":true}"#), Ok(Args { action: Action::Run(Until::Gart), keep: false, again: true, variant: None, ack_linux: false }));
    assert!(a(r#"{"until":"mode1"}"#).is_err() && a(r#"{"until":"vbios"}"#).is_err());
    // GX-7i.
    assert_eq!(a(r#"{"until":"sdma"}"#), Ok(Args { action: Action::Run(Until::Sdma), keep: false, again: false, variant: None, ack_linux: false }));
    // GX-7j.
    assert_eq!(a(r#"{"until":"gfx_ring"}"#), Ok(Args { action: Action::Run(Until::GfxRing), keep: false, again: false, variant: None, ack_linux: false }));
    assert_eq!(a(r#"{"until":"m5","again":true}"#), Ok(Args { action: Action::Run(Until::GfxRing), keep: false, again: true, variant: None, ack_linux: false }));
    assert!(a(r#"{"until":"gfx-ring"}"#).is_err() && a(r#"{"until":"mes_kiq"}"#).is_err());
    // GX-7k.
    assert_eq!(a(r#"{"until":"mes"}"#), Ok(Args { action: Action::Run(Until::Mes), keep: false, again: false, variant: None, ack_linux: false }));
    assert_eq!(a(r#"{"until":"gfx_mes","keep":true}"#), Ok(Args { action: Action::Run(Until::GfxMes), keep: true, again: false, variant: None, ack_linux: false }));
    assert_eq!(a(r#"{"until":"mes","variant":"e1a"}"#), Err(std::string::String::from("variant (only with until=gfx_ring)")));
    // GX-7j2: the variant, only with until=gfx_ring.
    use crate::gpucore::gfxring::Variant;
    for (t, v) in [("e1a", Variant::E1a), ("e1b", Variant::E1b), ("e1c", Variant::E1c)] {
        let j = std::format!(r#"{{"until":"gfx_ring","again":true,"variant":"{t}"}}"#);
        assert_eq!(a(&j), Ok(Args { action: Action::Run(Until::GfxRing), keep: false, again: true, variant: Some(v), ack_linux: false }));
    }
    assert_eq!(a(r#"{"until":"gfx_ring","variant":null}"#).unwrap().variant, None);
    assert_eq!(a(r#"{"until":"gfx_ring","variant":"e1d"}"#), Err(std::string::String::from("variant (e1a, e1b or e1c)")));
    assert!(a(r#"{"until":"sdma","variant":"e1a"}"#).is_err() && a(r#"{"variant":"e1b"}"#).is_err() && a(r#"{"until":"gfx_ring","variant":1}"#).is_err());
    assert_eq!(Variant::DEFAULT, Variant::E1b);
    assert_eq!(a(r#"{"until":"m4","again":true}"#).unwrap().action, Action::Run(Until::Sdma));
}

/// Our PSP phase against Linux's (the capture's, from the fixture), through the live owner stack: cold (as the fake run, GX-7c's numbers less
/// VCN) and after Linux (DESTROY_RINGS first and DESTROY_TMR: one extra C2PMSG_64 write, one extra frame).
#[test]
fn psp_phase_against_the_linux_capture() {
    // From the public fixture (the capture's PSP phase, `psp_fixture_is_the_capture`): this never skips.
    let names = table_names();
    let linux = linux_psp_writes();
    assert_eq!(linux.len(), 31, "the fixture's PSP-phase writes");
    let d = fakechip::discovery_780m();
    let opts = ListOpts::PSP13_0_4;
    let frames = Allow {
        phase: "psp",
        name: "MP0_SMN_C2PMSG_67",
        kind: Some(DiffKind::Missing),
        count: 7,
        why: "Linux sends 27 KM-ring frames, BitsOS 20 cold: no DMCUB (GX-D9), no VCN, no TA loads, two late commands",
    };
    // The run's writes before the undo (the trace ends with the driver loaded: no DESTROY in it).
    let ours = |s: StartAs| {
        let (out, ..) = k::one(&d, 512 << 20, s, Until::Fw, true, Hostile::None, &opts, &[]).unwrap();
        assert!(out.passed(), "{s:?}: {:?}", out.error);
        out.entries.iter().map(|e| (e.byte, e.value)).collect::<Vec<_>>()
    };
    let cold = regdiff::diff_phase(&PHASES[0], &linux, &ours(StartAs::Cold), &names, core::slice::from_ref(&frames));
    std::eprintln!("{}", regdiff::render(core::slice::from_ref(&cold)));
    assert_eq!((cold.linux, cold.ours, cold.matched), (31, 24, 24));
    assert_eq!(cold.failures().count(), 0, "{}", regdiff::render(core::slice::from_ref(&cold)));
    let frames = Allow { count: 6, why: "the stale path's DESTROY_TMR is one more frame: 21 of Linux's 27", ..frames };
    let destroy = Allow {
        phase: "psp",
        name: "MP0_SMN_C2PMSG_64",
        kind: Some(DiffKind::Extra),
        count: 1,
        why: "after Linux the old ring is destroyed first (DESTROY_RINGS 0x30000), Linux's own load met a cold PSP",
    };
    let stale = regdiff::diff_phase(&PHASES[0], &linux, &ours(StartAs::AfterLinux), &names, &[frames, destroy]);
    std::eprintln!("{}", regdiff::render(core::slice::from_ref(&stale)));
    assert_eq!((stale.linux, stale.ours, stale.matched), (31, 26, 25));
    assert_eq!(stale.failures().count(), 0, "{}", regdiff::render(core::slice::from_ref(&stale)));
    assert_eq!(stale.count(DiffKind::Extra), 1);
}

/// GX-7d fixer: no PSP buffer shares a 2 MiB CPU piece with anything in use before the run, also when the GOP
/// surface is not a whole number of pieces (1920x1080x4 ends at 0x7E9000): the piece `psplive` maps uncached
/// then holds only PSP pages, never a page of a write-combining mapping.
#[test]
fn psp_pieces_never_meet_another_mapping() {
    use crate::gpucore::pspbringup::{PIECE, piece_range};
    let d = fakechip::discovery_780m();
    let opts = ListOpts::PSP13_0_4;
    for gop in [1920 * 1080 * 4, 1366 * 768 * 4 + 0x1000 - (1366 * 768 * 4) % 0x1000, 8 << 20] {
        for size in [512u64 << 20, 8 << 30] {
            let (mut out, _owner, bo, _) = k::one_gop(&d, size, StartAs::Cold, Until::Fw, true, Hostile::None, &opts, &[], gop).unwrap();
            assert!(out.passed(), "{gop:#x}: {:?}", out.error);
            let p = out.kept.take().unwrap();
            let bufs: Vec<_> = p.buffers().collect();
            assert_eq!(bufs.len(), 6, "ring, fence, cmd, pri, TMR, firmware");
            let others: Vec<_> = bo.used().filter(|r| !bufs.iter().any(|b| bo.get(b.handle).is_some_and(|x| x.range == *r))).collect();
            assert_eq!(others, [0..gop, size - (64 << 10)..size], "{gop:#x}");
            for b in &bufs {
                let piece = piece_range(b.offset, b.size);
                assert_eq!(b.offset % PIECE, 0, "{b:x?}");
                assert_eq!(psplive::meets(piece.clone(), &others), None, "{gop:#x}: {b:x?} piece {piece:x?}");
                // Its BO holds the whole pieces: no other BO can come into them later.
                assert_eq!(bo.get(b.handle).unwrap().range, piece, "{b:x?}");
            }
            // The PSP is told what it asked for (the TOC's TMR size, Linux's buffer sizes), not the rounding.
            assert_eq!(p.tmr.unwrap().size, u64::from(crate::gpucore::pspfake::TOC_TMR));
            assert_eq!(p.ring.buf.size, u64::from(crate::gpucore::psp::RING_BYTES));
        }
    }
    let w = CarveWin { mc_base: 0x80_0000_0000, phys_base: 0x8_4000_0000, size: 512 << 20 };
    assert_eq!(w.piece_len(0), Some(CHUNK));
    assert_eq!(w.piece_len(255), Some(CHUNK - (64 << 10)), "the top piece stops below the CPU-closed 64 KiB");
    assert_eq!(w.piece_len(256), None);
    let next = std::vec![CHUNK..2 * CHUNK];
    assert_eq!(psplive::meets(0..CHUNK, &next), None);
    assert_eq!(psplive::meets(0..CHUNK + 1, &next), Some(CHUNK..2 * CHUNK));
}

/// GX-7d fixer: the tool's refusals before anything is written, in order; the checks run under the PSP's lock.
#[test]
fn preflight_refusals() {
    use psplive::{Pre, preflight};
    let ok = Pre { reboot_needed: false, quarantined: false, display: Ok(()), has_bos: true, until: Until::Fw, m2_runs: 0, again: false, kept: false, gfx_runs: 0, gc_failed: false, reset: false, gmc_runs: 0, kept_gc_booted: false, sdma_runs: 0, gart_up: false, gfx_ring_runs: 0, mes_runs: 0, mes_up: false, mes_stranded: false, mes_queues: false };
    assert_eq!(preflight(&ok), Ok(()));
    let why = |p: Pre| preflight(&p).unwrap_err();
    assert!(why(Pre { reboot_needed: true, ..ok.clone() }).contains("reboot"));
    assert!(why(Pre { quarantined: true, ..ok.clone() }).contains("reboot"));
    assert!(why(Pre { display: Err("gates".into()), ..ok.clone() }).contains("display refused (gates)"));
    assert!(why(Pre { has_bos: false, ..ok.clone() }).contains("buffer-object manager"));
    assert!(why(Pre { kept: true, ..ok.clone() }).contains("until=stop"));
    assert!(why(Pre { m2_runs: 1, ..ok.clone() }).contains("again=true"));
    assert_eq!(preflight(&Pre { m2_runs: 1, again: true, ..ok.clone() }), Ok(()));
    assert_eq!(preflight(&Pre { m2_runs: 3, until: Until::Ring, ..ok.clone() }), Ok(()), "M1 needs no again");
    // GX-7e: a GC stage also loads the firmware (the M2 rule), and once per boot without again.
    let gfx = Pre { until: Until::Gfx, ..ok.clone() };
    assert_eq!(preflight(&gfx), Ok(()));
    assert!(why(Pre { m2_runs: 1, ..gfx.clone() }).contains("again=true"));
    assert!(why(Pre { gfx_runs: 1, m2_runs: 0, ..gfx.clone() }).contains("GC stage ran"));
    assert_eq!(preflight(&Pre { gfx_runs: 1, m2_runs: 1, again: true, ..gfx.clone() }), Ok(()));
    // GX-7h: until=gart runs the GC stage too (fresh), continues on a kept GC-booted run, once per boot.
    let gart = Pre { until: Until::Gart, ..ok.clone() };
    assert_eq!(preflight(&gart), Ok(()));
    assert!(why(Pre { gfx_runs: 1, ..gart.clone() }).contains("GC stage ran"));
    assert!(why(Pre { gmc_runs: 1, ..gart.clone() }).contains("GMC stage ran"));
    assert_eq!(preflight(&Pre { kept: true, kept_gc_booted: true, m2_runs: 1, gfx_runs: 1, ..gart.clone() }), Ok(()), "continues on the kept run");
    assert!(why(Pre { kept: true, kept_gc_booted: true, gmc_runs: 1, ..gart.clone() }).contains("GMC stage ran"));
    assert_eq!(preflight(&Pre { kept: true, kept_gc_booted: true, gmc_runs: 1, again: true, ..gart.clone() }), Ok(()));
    assert!(why(Pre { kept: true, kept_gc_booted: false, ..gart.clone() }).contains("GC did not boot"));
    assert!(why(Pre { kept: true, kept_gc_booted: true, ..gfx.clone() }).contains("until=stop"), "only gart and sdma continue");
    // GX-7i: until=sdma continues on a kept GC-booted run; the GMC stage again only without a passed GART; once
    // per boot without again.
    let sd = Pre { until: Until::Sdma, kept: true, kept_gc_booted: true, m2_runs: 1, gfx_runs: 1, ..ok.clone() };
    assert_eq!(preflight(&sd), Ok(()));
    assert_eq!(preflight(&Pre { gmc_runs: 1, gart_up: true, ..sd.clone() }), Ok(()), "the GART stands: no GMC stage again");
    assert!(why(Pre { gmc_runs: 1, gart_up: false, ..sd.clone() }).contains("GMC stage ran"));
    assert!(why(Pre { sdma_runs: 1, gmc_runs: 1, gart_up: true, ..sd.clone() }).contains("SDMA stage ran"));
    assert_eq!(preflight(&Pre { sdma_runs: 1, gmc_runs: 1, gart_up: true, again: true, ..sd.clone() }), Ok(()));
    assert!(why(Pre { kept_gc_booted: false, ..sd.clone() }).contains("until=sdma continues only"));
    assert_eq!(preflight(&Pre { until: Until::Sdma, ..ok.clone() }), Ok(()), "fresh: PSP, GC, GMC, SDMA in one run");
    assert!(Until::Sdma.loads_fw() && Until::Sdma.runs_gc() && Until::Sdma.runs_gmc() && !Until::Gfx.runs_gmc());
    // GX-7j: until=gfx_ring as until=sdma: continues on a kept GC-booted run, the GMC stage again only without a
    // passed GART, once per boot without again; an SDMA stage of the boot does not matter.
    let gr = Pre { until: Until::GfxRing, ..sd.clone() };
    assert_eq!(preflight(&gr), Ok(()));
    assert_eq!(preflight(&Pre { gmc_runs: 1, gart_up: true, sdma_runs: 1, ..gr.clone() }), Ok(()), "after sdma: the GART stands");
    assert!(why(Pre { gmc_runs: 1, gart_up: false, ..gr.clone() }).contains("GMC stage ran"));
    assert!(why(Pre { gfx_ring_runs: 1, gmc_runs: 1, gart_up: true, ..gr.clone() }).contains("GFX ring stage ran"));
    assert_eq!(preflight(&Pre { gfx_ring_runs: 1, gmc_runs: 1, gart_up: true, again: true, ..gr.clone() }), Ok(()));
    assert!(why(Pre { kept_gc_booted: false, ..gr.clone() }).contains("until=gfx_ring continues only"));
    assert_eq!(preflight(&Pre { until: Until::GfxRing, ..ok.clone() }), Ok(()), "fresh: PSP, GC, GMC, GFX ring in one run");
    assert!(why(Pre { until: Until::Sdma, gfx_ring_runs: 1, sdma_runs: 1, gmc_runs: 1, gart_up: true, ..sd.clone() }).contains("SDMA stage ran"));
    // GX-7k: until=mes and gfx_mes as gfx_ring (continue on a kept run, after sdma and after gfx_ring without
    // again), once per boot without again; gfx_ring is refused while MES runs.
    for u in [Until::Mes, Until::GfxMes] {
        let m = Pre { until: u, gmc_runs: 1, gart_up: true, ..sd.clone() };
        assert_eq!(preflight(&m), Ok(()));
        assert_eq!(preflight(&Pre { gfx_ring_runs: 1, sdma_runs: 1, ..m.clone() }), Ok(()), "{u:?} after gfx_ring and sdma");
        assert!(why(Pre { mes_runs: 1, ..m.clone() }).contains("MES stage ran"));
        assert_eq!(preflight(&Pre { mes_runs: 1, mes_up: true, again: true, ..m.clone() }), Ok(()));
        assert!(why(Pre { kept_gc_booted: false, ..m.clone() }).contains("until=mes and gfx_mes continue only"));
        assert_eq!(preflight(&Pre { until: u, ..ok.clone() }), Ok(()), "fresh: PSP, GC, GMC, the GFX stage and MES in one run");
        assert!(u.loads_fw() && u.runs_gc() && u.runs_gmc() && u.after_gart() && u.runs_mes() && u.runs_gfx_stage());
    }
    assert!(why(Pre { mes_up: true, mes_runs: 1, gmc_runs: 1, gart_up: true, again: true, ..gr.clone() }).contains("MES runs"));
    assert!(!Until::GfxRing.runs_mes() && Until::GfxRing.runs_gfx_stage() && !Until::Sdma.runs_gfx_stage());
    // GX-7k: queues MES did not unmap: every stage that starts the CP or its firmware is refused, even with again.
    for u in [Until::Fw, Until::Gfx, Until::Sdma, Until::GfxRing, Until::Mes, Until::GfxMes] {
        assert!(why(Pre { until: u, mes_stranded: true, again: true, ..ok.clone() }).contains("STRANDED"), "{u:?}");
    }
    assert_eq!(preflight(&Pre { until: Until::Ring, mes_stranded: true, ..ok.clone() }), Ok(()), "M1 touches no CP");
    // GX-7f: mode 2 is refused always (the 28.10. hang), before every other check.
    let reset = Pre { reset: true, ..gfx.clone() };
    let m2 = crate::gpucore::gfxup::MODE2_REFUSED;
    assert!(why(reset.clone()).starts_with(m2) && !why(reset.clone()).contains("reboot now"));
    assert!(why(Pre { gc_failed: true, gfx_runs: 1, ..reset.clone() }).ends_with("(a GC stage failed in this boot: reboot now)"));
    assert!(why(Pre { gc_failed: true, reboot_needed: true, ..reset.clone() }).starts_with(m2));
    assert!(why(Pre { gc_failed: true, kept: true, ..reset }).starts_with(m2));
    // The reboot refusal wins over every other.
    assert!(why(Pre { reboot_needed: true, has_bos: false, kept: true, ..ok }).contains("reboot"));
}

/// GX-7d fixer: bus master goes off only when the PSP provably holds nothing.
#[test]
fn quiesce_bus_master_policy() {
    use psplive::{QuiesceSeen as S, quiesce_bus_master_off as off};
    assert!(!off(S::Busy, false), "a run in progress: the PSP is mid-command");
    assert!(off(S::Idle, false));
    assert!(!off(S::Idle, true), "a quarantined PSP may still use its buffers");
    assert!(off(S::Stopped(true), false));
    assert!(!off(S::Stopped(false), false), "a failed undo");
    assert!(!off(S::Stopped(true), true));
    assert!(!off(S::Left, false), "GX-7f: a ring held for an unsettled GC stays with bus master on");
}

/// GX-7f fixer: every stop prints a bus master line, one per case, the 'off again' line the check list expects.
#[test]
fn bus_master_line_per_case() {
    use psplive::{BusChoice as C, bus_choice, bus_line};
    assert_eq!(bus_choice(true, true, true), C::Quarantined, "quarantine wins over ours");
    assert_eq!(bus_choice(true, false, false), C::Quarantined);
    assert_eq!(bus_choice(false, true, true), C::SwitchOff);
    assert_eq!(bus_choice(false, true, false), C::SwitchOff, "ours even if read off now: switched off again (idempotent)");
    assert_eq!(bus_choice(false, false, true), C::NotOurs);
    assert_eq!(bus_choice(false, false, false), C::AsFound);
    assert_eq!(bus_line(C::SwitchOff, Some((true, false, false))), "bus master: off again on the function (was on: true, refused false, incomplete false)");
    assert!(bus_line(C::SwitchOff, None).ends_with("incomplete true)"), "no report: says incomplete");
    assert_eq!(bus_line(C::Quarantined, None), "bus master: left on (a quarantined PSP may still use its buffers; reboot)");
    assert!(bus_line(C::NotOurs, None).starts_with("bus master: left on, as found (it was on before this driver switched anything"));
    assert!(bus_line(C::NotOurs, None).contains("the engines this driver started are stopped"), "GX-7i2: says what the stop did");
    assert_eq!(bus_line(C::AsFound, None), "bus master: off (as found)");
    assert_eq!(bus_line(C::Kept, None), "bus master: stays on (TMR and ring are kept)");
    for c in [C::Quarantined, C::SwitchOff, C::NotOurs, C::AsFound, C::Kept] {
        assert!(bus_line(c, Some((true, true, true))).starts_with("bus master: "), "{c:?}");
    }
}

/// GX-7f fixer: a Reload after a clean stop classifies as clean, end to end through the note slot (the ktest's check).
#[test]
fn reload_after_a_clean_stop_is_clean() {
    k::check_reload_note(&fakechip::discovery_780m()).unwrap();
    if let Some(b) = blob() {
        k::check_reload_note(&b).unwrap();
    }
}

/// GX-7d fixer: the picture check around the run: the DCN snapshot unchanged and no display write counted.
#[test]
fn picture_before_and_after() {
    use crate::dcn::Marks;
    let regs = crate::snapshot::registers();
    let before: std::collections::BTreeMap<u32, u32> = regs.iter().take(40).enumerate().map(|(i, e)| (e.byte, i as u32 * 3)).collect();
    let m = |t: u64| Marks { total: t, total_after: t, ..Marks::default() };
    let (same, lines) = psplive::picture(before.clone(), &before, &m(7), &m(7));
    assert!(same, "{lines:?}");
    assert!(lines[0].contains("display writes during the run: 0"));
    let (same, lines) = psplive::picture(before.clone(), &before, &m(7), &m(8));
    assert!(!same && lines[0].contains(": 1 (must be 0)"));
    // A write in flight at the start (the total read after the owners' counts moved) still counts.
    let (same, lines) = psplive::picture(before.clone(), &before, &Marks { total: 7, total_after: 8, ..Marks::default() }, &m(8));
    assert!(!same && lines[0].contains(": 1 (must be 0)"), "{lines:?}");
    let mut moved = before.clone();
    // The lowest bit the register's rule compares (not a volatile field) flips.
    let (b, v) = moved
        .iter_mut()
        .find_map(|(b, v)| match crate::snapshot::rule(*b) {
            crate::snapshot::Rule::Bits { ignore } if ignore != u32::MAX => Some((*b, v, ignore.wrapping_add(1) & !ignore)),
            _ => None,
        })
        .map(|(b, v, bit)| {
            *v ^= bit;
            (b, *v)
        })
        .expect("a compared register");
    let (same, lines) = psplive::picture(before, &moved, &m(7), &m(7));
    assert!(!same, "{b:#x} moved to {v:#x}: {lines:?}");
}

/// GX-7m2: the 29.10. run (r3-ring.txt): one bitscomp flip of HUBP 0 during the run, 4 writes, the surface address
/// 0x0 -> 0x2000000 and EARLIEST_INUSE after it, HUBP_IN_BLANK 0 -> 1. The flip's writes and registers are the
/// compositor's: listed, not counted; a write by anyone else, a flipped register of another pipe or any other
/// register still fails (GX-D9).
#[test]
fn picture_attributes_the_compositors_flips() {
    use crate::dcn::Marks;
    use crate::layout as l;
    let before: std::collections::BTreeMap<u32, u32> = crate::snapshot::registers().iter().map(|e| (e.byte, 0)).collect();
    let mut after = before.clone();
    after.insert(l::hubp(0, l::HUBP_CNTL), 0x100f_100a ^ 0x100f_1002); // HUBP_IN_BLANK only
    after.insert(l::hubp(0, l::HUBP_ADDR), 0x0200_0000);
    after.insert(l::hubp(0, l::HUBP_INUSE), 0x0200_0000);
    let m0 = Marks { total: 100, total_after: 100, ..Marks::default() };
    let mut fp = [0u64; l::PIPES as usize];
    fp[0] = 1;
    let m1 = Marks { total: 104, total_after: 104, flips: 4, flip_pipes: fp, ..Marks::default() };
    let (same, lines) = psplive::picture(before.clone(), &after, &m0, &m1);
    assert!(same, "{lines:#?}");
    assert!(lines[0] == "display writes during the run: 0 (must be 0)", "{lines:?}");
    assert!(lines[1].contains("4 by its page flips (pipe 0)") && lines[1].contains("0 by its cursor updates"), "{lines:?}");
    assert!(lines[2].contains("equal"), "{lines:?}");
    assert!(lines.iter().any(|x| x.contains("changed by the compositor") && x.contains("PRIMARY_SURFACE_ADDRESS")), "{lines:?}");
    // One more write by someone else: counted.
    let (same, lines) = psplive::picture(before.clone(), &after, &m0, &Marks { total: 105, total_after: 105, ..m1 });
    assert!(!same && lines[0].contains(": 1 (must be 0)"), "{lines:?}");
    // The same address change on pipe 1, which the compositor did not flip: a difference.
    let mut other = before.clone();
    other.insert(l::hubp(1, l::HUBP_ADDR), 0x0200_0000);
    let (same, lines) = psplive::picture(before.clone(), &other, &m0, &m1);
    assert!(!same && lines[2].contains("differ"), "{lines:?}");
    // Any other register of the flipped pipe: a difference.
    let mut vp = after.clone();
    vp.insert(l::hubp(0, l::HUBP_VIEWPORT), 0x0438_0780);
    let (same, _) = psplive::picture(before.clone(), &vp, &m0, &m1);
    assert!(!same);
    // A cursor update on pipe 0: its CURSOR0_0_ registers are the compositor's.
    let mut cur = before.clone();
    let cb = *before.keys().find(|&&b| crate::snapshot::cursor_pipe(b) == Some(0)).expect("a cursor register of pipe 0 in the snapshot");
    cur.insert(cb, 0x0010_0020);
    let mut cp = [0u64; l::PIPES as usize];
    cp[0] = 1;
    let mc = Marks { total: 105, total_after: 105, cursor: 5, cursor_pipes: cp, ..Marks::default() };
    let (same, lines) = psplive::picture(before.clone(), &cur, &m0, &mc);
    assert!(same, "{cb:#x}: {lines:?}");
    // The same change with no cursor update counted: a difference.
    let (same, _) = psplive::picture(before.clone(), &cur, &m0, &m0);
    assert!(!same);
    assert!(lines[0].contains(": 0 (must be 0)") && lines[1].contains("5 by its cursor updates (pipe 0)"), "{lines:?}");
}

/// A device for the Quiesce path: every delay is one of the bounded waits (no long sleep), the total is
/// counted; reads, writes and memory go to the fake behind the watcher.
struct QuiesceHw<'a> {
    inner: &'a mut crate::gpucore::watch::Watch<crate::gpucore::pspfake::FakeGpu>,
    waited: u64,
    max_one: u64,
}

impl crate::gpucore::psp::Hw for QuiesceHw<'_> {
    fn reg_read(&mut self, byte: u32) -> Option<u32> {
        self.inner.reg_read(byte)
    }
    fn reg_write(&mut self, byte: u32, value: u32) -> bool {
        self.inner.reg_write(byte, value)
    }
    fn mem_read(&mut self, mc: u64, buf: &mut [u8]) -> bool {
        self.inner.mem_read(mc, buf)
    }
    fn mem_write(&mut self, mc: u64, data: &[u8]) -> bool {
        self.inner.mem_write(mc, data)
    }
    fn now_us(&mut self) -> u64 {
        self.inner.now_us()
    }
    fn delay_us(&mut self, us: u64) {
        assert!(us <= self.max_one, "a wait of {us} us on the Quiesce path");
        self.waited += us;
        self.inner.delay_us(us);
    }
}

/// GX-7d fixer: the Quiesce hook's stop of a kept ring, past the owner's log (which may allocate), against a
/// good, a silent and a fence-less PSP: bounded (each wait at most the settle, the whole well under a second),
/// and a failed stop leaves the PSP not quiet (bus master stays on, the buffers stay allocated).
#[test]
fn quiesce_stop_is_bounded_and_honest() {
    use crate::gpucore::psp::{Hw, Timeouts};
    use crate::gpucore::psp::cmd;
    let d = fakechip::discovery_780m();
    let opts = ListOpts::PSP13_0_4;
    for (h, quiet) in [(Hostile::None, true), (Hostile::StopSilent, false), (Hostile::NoFence, false), (Hostile::CmdStatus { id: cmd::DESTROY_TMR, status: 2 }, false)] {
        let (mut out, mut owner, bo, _) = k::one(&d, 512 << 20, StartAs::Cold, Until::Fw, true, Hostile::None, &opts, &[]).unwrap();
        let mut p = out.kept.take().unwrap();
        let logged = owner.log().totals().writes;
        let held = bo.count();
        owner.hw().hw().hostile = h;
        p.t = Timeouts::QUIESCE;
        let t0 = owner.hw().now_us();
        let mut q = QuiesceHw { inner: owner.hw(), waited: 0, max_one: Timeouts::QUIESCE.settle_us };
        let r = p.stop(&mut q);
        let waited = q.waited;
        let took = owner.hw().now_us() - t0;
        assert_eq!(r.is_ok() && p.quiet(), quiet, "{h:?}: {r:?}");
        assert!(waited <= 500_000 && took <= 500_000, "{h:?}: waited {waited} us, took {took} us");
        assert_eq!(owner.log().totals().writes, logged, "{h:?}: the Quiesce path went through the owner's log");
        assert_eq!(psplive::quiesce_bus_master_off(psplive::QuiesceSeen::Stopped(r.is_ok() && p.quiet()), false), quiet);
        assert_eq!(bo.count(), held, "{h:?}: Quiesce gives nothing back");
    }
}

/// The Linux PSP-phase writes of the 27.10. capture (tests/data, public): `MP0_SMN_C2PMSG_*` writes in order,
/// the ring address (69/70, compared as "any") written as 0. Kept in step with the capture when it is here.
fn linux_psp_writes() -> Vec<(u32, u32)> {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/linux-780m-psp-writes.txt");
    let text = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
    text.lines().filter_map(k::w_line).collect()
}

fn psp_fixture_text(linux: &[(u32, u32)], names: &super::regdiff::Names) -> std::string::String {
    use crate::gpucore::regdiff::Names as _;
    let mut s = std::string::String::from(
        "# Linux 6.6 amdgpu load on the 780M (27.10.2026 capture): its MP0_SMN_C2PMSG_* writes in order.\n# W <BAR5 byte> <value>; the ring address (C2PMSG_69/70) written as 0. Test: tests::bringup.\n",
    );
    for &(b, v) in linux {
        let n = names.names(b);
        let Some(name) = n.iter().find(|x| x.starts_with("MP0_SMN_C2PMSG_")) else { continue };
        let v = if name == "MP0_SMN_C2PMSG_69" || name == "MP0_SMN_C2PMSG_70" { 0 } else { v };
        s += &std::format!("W {b:#x} {v:#x} # {name}\n");
    }
    s
}

/// The fixture is the capture's PSP phase (runs where plan/ and zstd are; the comparison below never skips).
#[test]
fn psp_fixture_is_the_capture() {
    let Some(t) = capture() else {
        std::eprintln!("SKIPPED psp_fixture_is_the_capture: no capture (plan/ is not public); the fixture stands in");
        return;
    };
    let want = psp_fixture_text(&t.writes(false), &table_names());
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/linux-780m-psp-writes.txt");
    let have = std::fs::read_to_string(&p).unwrap_or_default();
    assert!(have == want, "{} is not the capture's PSP phase; it should read:\n{want}", p.display());
}

/// GX-7d fixer: the boot's PSP pool takes a BO from the manager once and reuses it; a released buffer goes back to
/// the pool, never to the manager (a piece once mapped uncached stays mapped: no display BO may come into it).
#[test]
fn psp_pool_reuses_and_never_gives_back() {
    use crate::bo::{BoManager, Placement};
    use crate::gpucore::psp::BoAlloc;
    use crate::gpucore::pspbringup::{PIECE, Pieces, Pool, Pooled};
    let mut bo = BoManager::new(64 << 20, Vec::new());
    let mut pool = Pool::new();
    let (h1, r1) = Pieces(&mut Pooled(&mut pool, &mut bo)).alloc(0x1000, 0x1000).unwrap();
    assert_eq!(r1, 0..0x1000, "the PSP is told the size it asked for");
    let (h2, r2) = Pieces(&mut Pooled(&mut pool, &mut bo)).alloc(PIECE + 1, 0x1000).unwrap();
    assert_eq!((r2.start, bo.get(h2).unwrap().range.clone()), (PIECE, PIECE..3 * PIECE));
    Pieces(&mut Pooled(&mut pool, &mut bo)).release(h1);
    Pieces(&mut Pooled(&mut pool, &mut bo)).release(h2);
    assert_eq!(bo.count(), 2, "released into the pool, not the manager");
    // The display asks the manager for a BO: it never lands in the pool's pieces.
    let d = bo.create(PIECE, Placement::Vram, PIECE, None).unwrap();
    assert_eq!(bo.get(d).unwrap().range, 3 * PIECE..4 * PIECE);
    // The next run gets the same buffers back (the smallest that fits first), no new BO.
    let (a, _) = Pieces(&mut Pooled(&mut pool, &mut bo)).alloc(0x1000, 0x1000).unwrap();
    let (b, _) = Pieces(&mut Pooled(&mut pool, &mut bo)).alloc(PIECE, 0x1000).unwrap();
    assert_eq!((a, b, bo.count()), (h1, h2, 3));
    // Both busy: a third asks the manager.
    let (c, rc) = Pieces(&mut Pooled(&mut pool, &mut bo)).alloc(0x1000, 0x1000).unwrap();
    assert!(c != h1 && c != h2 && rc.start % PIECE == 0 && bo.count() == 4);
    assert!(pool.holds(&(0..PIECE)) && !pool.holds(&(3 * PIECE..4 * PIECE)));
}

/// GX-7e: M3 on every start state and size, every PMFW/GC timeout and wrong answer, the forbidden messages and
/// the mode-2 recovery, on the fake's and the real discovery table (the ktests' checks).
#[test]
fn gfx_stage_starts_failures_recovery() {
    let d = fakechip::discovery_780m();
    k::check_gfx(&d).unwrap();
    k::check_gfx_failures(&d).unwrap();
    k::check_forbidden().unwrap();
    k::check_recovery(&d).unwrap();
    if let Some(b) = blob() {
        k::check_gfx(&b).unwrap();
        k::check_gfx_failures(&b).unwrap();
        k::check_recovery(&b).unwrap();
    }
}

/// GX-7e: the answer of a cold M3 run as the desk reads it: the stage's steps in Linux's order, each message with
/// its answer, the M3 checks, the note that the boot is ours, and the SMU writes as W lines with their names.
#[test]
fn gfx_answer_lines() {
    let d = fakechip::discovery_780m();
    let (out, ..) = k::one(&d, 512 << 20, StartAs::Cold, Until::Gfx, false, Hostile::None, &ListOpts::PSP13_0_4, &[]).unwrap();
    let fw = crate::gpucore::ucode::FwSet::default();
    let inp = pspbringup::Inputs {
        addrs: crate::gpucore::pspfake::FakeGpu::new(Hostile::None).a,
        carve: crate::gpucore::psp::Carve { mc_base: 0, phys_base: 0 },
        start: Start::Cold,
        until: Until::Gfx,
        fw: &fw,
        list: &[],
        autoload: true,
        keep: false,
        t: crate::gpucore::psp::Timeouts::LINUX,
        gfx: pspbringup::GfxInputs::from_discovery(&d),
        gmc: None,
        sdma: None,
        gfx_ring: None,
        mes: None,
        vm: None,
        unload_first: false,
    };
    let text = pspbringup::render(&out, &inp);
    for l in &text {
        std::eprintln!("{l}");
    }
    let find = |p: &str| text.iter().position(|l| l.contains(p)).unwrap_or_else(|| panic!("no line {p:?}"));
    let order = [
        "run: until gfx (M3)",
        "gfx step 1: FIRMWARE_FLAGS 0x1",
        "gfx step 2: GC before: GFX_IMU_MSG_FLAGS=0x80000001 (recorded only) SMUIO_GFX_MISC_CNTL=0x0 (PWR_GFXOFF_STATUS 0) RLC_RLCS_BOOTLOAD_STATUS=- CP_STAT=-",
        "gfx step 3: GetDriverIfVersion: 8",
        "gfx step 4: GetPmfwVersion: 0x4c5700",
        "gfx step 5: EnableGfxImu(1) sent, not waited for",
        "gfx step 6: GFX_IMU_MSG_FLAGS 0x80000001 after 0 us (recorded only, nothing waits on it: the value before EnableGfxImu on 28.10.)",
        "gfx step 7: EnableGfxImu answered 0x1 (OK) after 30",
        "GFX_IMU_MSG_FLAGS 0x8000000d after",
        "GC booted after",
        "smu: 0x3 GetDriverIfVersion arg 0x0 -> 0x1 OK, argument register 0x8",
        "smu: 0x16 EnableGfxImu arg 0x1 -> 0x1 OK",
        "note: GC was not booted before EnableGfxImu: this boot is ours",
        "timing (judged on the Mini-PC only): booted",
        "gfx: last GC state",
        "M3 EnableGfxImu answered: PASS",
        "M3 CP_STAT 0: PASS",
        "M3 RLC bootload complete: PASS",
        "M3 GFXOFF not allowed: PASS (messages sent: 0x3 GetDriverIfVersion, 0x2 GetPmfwVersion, 0x16 EnableGfxImu)",
        "undo: none: EnableGfxImu reached PMFW, so TMR and ring stay",
        "GC: stays on and booted",
        "verdict: PASS",
    ];
    let at: Vec<usize> = order.iter().map(|p| find(p)).collect();
    assert!(at.windows(2).all(|w| w[0] < w[1]), "{at:?}");
    assert!(text.iter().any(|l| l.starts_with("W 0x58a08 0x16 # MP1_SMN_C2PMSG_66")), "the EnableGfxImu write named");
    assert!(text.iter().any(|l| l.contains("# BIF_BX0_PCIE_INDEX2")), "the SMN index write named");
}

/// GX-7e: the capture's SMU-phase writes (the public fixture; `smu_fixture_is_the_capture` keeps it in step).
fn linux_smu_writes() -> Vec<(u32, u32)> {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/linux-780m-smu-writes.txt");
    let text = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
    text.lines().filter_map(k::w_line).collect()
}

/// The driver mailbox's writes of the capture, in order; the DRAM table address (the arguments of
/// SetDriverDramAddrHigh/Low, C2PMSG_82 right before message 0x0D/0x0E) written as 0.
fn smu_fixture_text(linux: &[(u32, u32)], names: &super::regdiff::Names) -> std::string::String {
    use crate::gpucore::regdiff::Names as _;
    let mut s = std::string::String::from(
        "# Linux 6.6 amdgpu load on the 780M (27.10.2026 capture): its driver-mailbox writes (MP1_SMN_C2PMSG_66/82/90) in order.\n# W <BAR5 byte> <value>; the DRAM table address (argument of messages 0x0D/0x0E) written as 0. Test: tests::bringup.\n",
    );
    let rows: Vec<(u32, u32, &str)> = linux
        .iter()
        .filter_map(|&(b, v)| names.names(b).iter().find(|x| ["MP1_SMN_C2PMSG_66", "MP1_SMN_C2PMSG_82", "MP1_SMN_C2PMSG_90"].contains(&x.as_str())).map(|n| (b, v, n.as_str())))
        .collect();
    for (i, &(b, v, n)) in rows.iter().enumerate() {
        let dram = n == "MP1_SMN_C2PMSG_82" && rows.get(i + 1).is_some_and(|&(_, m, nn)| nn == "MP1_SMN_C2PMSG_66" && (m == 0x0D || m == 0x0E));
        s += &std::format!("W {b:#x} {:#x} # {n}\n", if dram { 0 } else { v });
    }
    s
}

#[test]
fn smu_fixture_is_the_capture() {
    let Some(t) = capture() else {
        std::eprintln!("SKIPPED smu_fixture_is_the_capture: no capture (plan/ is not public); the fixture stands in");
        return;
    };
    let want = smu_fixture_text(&t.writes(false), &table_names());
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/linux-780m-smu-writes.txt");
    let have = std::fs::read_to_string(&p).unwrap_or_default();
    assert!(have == want, "{} is not the capture's SMU phase; it should read:\n{want}", p.display());
}

/// GX-7e: our SMU phase against Linux's (the fixture; never skips), through the live owner stack. Cold: the
/// capture's first three messages exactly (9 writes), the 18 later ones (VCN/JPEG power, the DRAM table, the
/// feature queries and transfers, the power-downs) missing by design. After Linux: DisallowGfxOff as well.
/// The IMU/RLC phase stays empty: M3 only reads GC (its RLC programming belongs to the later items).
#[test]
fn smu_phase_against_the_linux_capture() {
    let names = table_names();
    let linux = linux_smu_writes();
    assert_eq!(linux.len(), 63, "the fixture's SMU-phase writes");
    let d = fakechip::discovery_780m();
    let opts = ListOpts::PSP13_0_4;
    let later = |name: &'static str| Allow {
        phase: "smu",
        name,
        kind: Some(DiffKind::Missing),
        count: 18,
        why: "GX-7e sends three messages; VCN/JPEG power, the DRAM table, feature queries, transfers, power-downs: later or never",
    };
    let missing = [later("MP1_SMN_C2PMSG_66"), later("MP1_SMN_C2PMSG_82"), later("MP1_SMN_C2PMSG_90")];
    let ours = |s: StartAs| {
        let (out, ..) = k::one(&d, 512 << 20, s, Until::Gfx, false, Hostile::None, &opts, &[]).unwrap();
        assert!(out.passed(), "{s:?}: {:?}", out.gfx.as_ref().map(|g| g.error));
        out.entries[..out.undo_from.unwrap_or(out.entries.len())].iter().map(|e| (e.byte, e.value)).collect::<Vec<_>>()
    };
    let cold = regdiff::diff_phase(&PHASES[1], &linux, &ours(StartAs::Cold), &names, &missing);
    std::eprintln!("{}", regdiff::render(core::slice::from_ref(&cold)));
    assert_eq!((cold.name, cold.linux, cold.ours, cold.matched), ("smu", 63, 9, 9));
    assert_eq!(cold.failures().count(), 0, "{}", regdiff::render(core::slice::from_ref(&cold)));
    assert_eq!((cold.count(DiffKind::Missing), cold.count(DiffKind::Extra), cold.count(DiffKind::Reordered)), (54, 0, 0));
    // The first nine of Linux's writes are exactly ours.
    assert_eq!(&linux[..9], &ours(StartAs::Cold).into_iter().filter(|&(b, _)| names.get(&b).is_some_and(|n| n.iter().any(|x| x.starts_with("MP1_SMN_C2PMSG_")))).collect::<Vec<_>>()[..]);
    let extra = |name: &'static str| Allow { phase: "smu", name, kind: Some(DiffKind::Extra), count: 1, why: "after Linux DisallowGfxOff (0x1A) too: an earlier kernel may have allowed GFXOFF" };
    let mut allow: Vec<Allow> = missing.into_iter().collect();
    allow.extend([extra("MP1_SMN_C2PMSG_66"), extra("MP1_SMN_C2PMSG_82"), extra("MP1_SMN_C2PMSG_90")]);
    let stale = regdiff::diff_phase(&PHASES[1], &linux, &ours(StartAs::AfterLinux), &names, &allow);
    std::eprintln!("{}", regdiff::render(core::slice::from_ref(&stale)));
    assert_eq!((stale.linux, stale.ours), (63, 12));
    assert_eq!(stale.failures().count(), 0, "{}", regdiff::render(core::slice::from_ref(&stale)));
    let imu = PHASES.iter().find(|p| p.name == "imu_rlc").unwrap();
    assert!(regdiff::select(imu, &ours(StartAs::Cold), &names).is_empty(), "M3 writes no IMU/RLC register");
}

/// GX-7e: what the live owner may write: the PSP's five, the SMU driver mailbox (66/82/90), the SMN index with
/// FIRMWARE_FLAGS' address only; never the display's mailbox (67/83/91), never another SMN target.
#[test]
fn live_write_list() {
    use crate::gpucore::pspbringup::GfxInputs;
    use crate::ipmod::smu13;
    let d = fakechip::discovery_780m();
    let g = GfxInputs::from_discovery(&d).unwrap();
    let a = crate::ipmod::psp13::Addrs::from_discovery(&d).unwrap();
    let mut ok: Vec<u32> = a.writable().to_vec();
    ok.extend(g.smu.writable());
    // GX-7j3: as `PspHw::with_gmc`: the unload's RLC_CNTL (GC segment 1 dword 0x4c00, the probe's 0x3b000).
    ok.extend(g.gc.writable());
    assert_eq!(g.gc.rlc_cntl, 0x3b000);
    let may = |b, v| psplive::PspHw::may_write(&ok, Some(&g.smu), b, v);
    assert_eq!((g.smu.msg, g.smu.arg, g.smu.resp), (0x58a08, 0x58a48, 0x58a68), "MP1 C2PMSG_66/82/90");
    for b in [g.smu.msg, g.smu.arg, g.smu.resp, a.c2pmsg_64, a.c2pmsg_67] {
        assert!(may(b, 1), "{b:#x}");
    }
    for b in [g.smu.dcn_msg, g.smu.dcn_resp, g.smu.dcn_msg + 0x40, g.smu.smn_data, g.gc.imu_flags, g.gc.bootload, g.gc.cp_stat] {
        assert!(!may(b, 1), "{b:#x} must not be writable");
    }
    assert!(may(g.gc.rlc_cntl, 0), "GX-7j3: the unload's RLC stop");
    assert_eq!((g.smu.smn_index, g.smu.smn_data), (dcn314_regs::bar5::PCIE_INDEX2, dcn314_regs::bar5::PCIE_DATA2), "the SMN pair the raw tool refuses");
    assert!(may(g.smu.smn_index, smu13::SMN_FIRMWARE_FLAGS));
    assert!(!may(g.smu.smn_index, smu13::SMN_FIRMWARE_FLAGS + 4) && !may(g.smu.smn_index, 0));
    // Without the GC stage's inputs (M1/M2), nothing of the SMU.
    assert!(!psplive::PspHw::may_write(&a.writable(), None, g.smu.msg, 1) && !psplive::PspHw::may_write(&a.writable(), None, g.smu.smn_index, smu13::SMN_FIRMWARE_FLAGS));
    // The client's list: no message that changes clocks, Z-states, VCN/JPEG power or tables; mode 2 only.
    use crate::gpucore::smu::allowed;
    use smu13::msg;
    for id in 0..0x40u32 {
        let want = matches!(id, msg::GET_DRIVER_IF_VERSION | msg::GET_PMFW_VERSION | msg::ENABLE_GFX_IMU | msg::DISALLOW_GFXOFF | msg::PREPARE_MP1_FOR_UNLOAD);
        assert_eq!(allowed(id, 0), want, "{id:#x}");
    }
    assert!(allowed(msg::GFX_DEVICE_DRIVER_RESET, smu13::RESET_MODE_2) && !allowed(msg::GFX_DEVICE_DRIVER_RESET, 1));
}

/// GX-7e: which GC-stage failures need a reboot (PMFW may still work on our message), which are errnos.
#[test]
fn gfx_reboot_policy() {
    use crate::gpucore::gfxup::{GcSnap, GfxError};
    use crate::gpucore::smu::SmuError;
    let last = GcSnap::default();
    // GX-7f: a boot that never completed is a reboot too (mode 2 is refused on 13.0.4).
    for e in [GfxError::Smu(SmuError::PrePoll { last: 0x16 }), GfxError::Smu(SmuError::Timeout { id: 3 }), GfxError::Gone { byte: 0 }, GfxError::BootTimeout { last }] {
        assert!(e.needs_reboot(), "{e:?}");
    }
    for e in [GfxError::Interface { got: 9 }, GfxError::Smu(SmuError::Answer { id: 0x16, code: 0xFF }), GfxError::Smu(SmuError::NotRunning { flags: 0 })] {
        assert!(!e.needs_reboot(), "{e:?}");
    }
    let errno = |code| GfxError::Smu(SmuError::Answer { id: 3, code }).errno();
    assert_eq!([errno(0xFF), errno(0xFE), errno(0xFD), errno(0xFC), errno(0x42)], [5, 95, 22, 16, 121], "__smu_cmn_reg2errno");
    assert_eq!(GfxError::Smu(SmuError::PrePoll { last: 0 }).errno(), 62);
    assert!(GfxError::BootTimeout { last }.advice().contains("reboot") && !GfxError::BootTimeout { last }.advice().contains("until=reset"));
}

/// GX-7j3: the 780M started again without a reboot (stop and Reload, three times; the 29.10. failure model, the
/// catch-up, a busy mailbox, an unanswered unload): the ktest's check on the fake's table.
#[test]
fn reinit_without_reboot() {
    crate::ktests_reinit::check_reinit(&fakechip::discovery_780m()).unwrap();
}
