//! GX-7c: Linux's register trace (`regtrace`) and the diff harness (`regdiff`): the parser on hand-made lines
//! (home values removed), the diff's missing/extra/reordered verdicts, and, when the 27.10. capture is in
//! plan/hw (never in the public tree; skipped otherwise), our PSP bring-up against the fakes compared with
//! Linux's writes phase by phase, the SMU fake's table against Linux's real messages, and the watcher's view.

use crate::gpucore::fakechip;
use crate::gpucore::hwid;
use crate::gpucore::pspfake::{self, FakeGpu, Hostile, MapSource};
use crate::gpucore::regdiff::{self, Allow, DiffKind, PHASES};
use crate::gpucore::regtrace::{self, Kind, Trace};
use crate::gpucore::smufake;
use crate::gpucore::watch::{Class, DisplayGuard};
use std::collections::BTreeMap;
use std::string::{String, ToString};
use std::vec::Vec;

pub(super) type Names = BTreeMap<u32, Vec<String>>;

const LINES: &str = "# tracer: nop\n\
    bash-1 [004] .....    31.765661: amdgpu_device_rreg: 0x1900, 0x00016091, 0x000000b1\n\
    bash-1 [004] d..1.    31.982983: amdgpu_device_wreg: 0x1900, 0x00016085, 0x00339000\n\
    bash-1 [003] .....    32.611915: amdgpu_device_wreg: 0x1900, 0x00003846, 0x00000056\n\
    bash-1 [003] .....    32.611916: amdgpu_dc_wreg: reg=0x00003846, value=0x00000056\n\
    bash-1 [003] .....    32.611918: amdgpu_device_rreg: 0x1900, 0x00003847, 0x185600f0\n\
    bash-1 [003] .....    32.611918: amdgpu_dc_rreg: reg=0x00003847, value=0x185600f0\n\
    bash-1 [001] .....    31.977010: gx_ind_wreg: (amdgpu_device_indirect_wreg+0x4/0x80 [amdgpu]) addr=0x11180718 val=0x18000000\n\
    bash-1 [003] .....    32.578298: gx_ind_rreg: (amdgpu_device_indirect_rreg+0x4/0x80 [amdgpu]) addr=0x3b10028\n\
    bash-1 [003] .....    32.578303: gx_ind_rreg_ret: (smu_v13_0_check_fw_status+0x3a/0x50 [amdgpu] <- amdgpu_device_indirect_rreg) val=0x1\n\
    bash-1 [001] .....    32.786193: gx_port_rreg: (amdgpu_device_pcie_port_rreg+0x4/0xa0 [amdgpu]) reg=0x500efb\n\
    bash-1 [001] .....    32.786198: gx_port_rreg_ret: (nbio_v7_7_sdma_doorbell_range+0x33/0x70 [amdgpu] <- amdgpu_device_pcie_port_rreg) val=0x0\n\
    bash-1 [001] .....    32.786200: gx_port_wreg: (amdgpu_device_pcie_port_wreg+0x4/0xc0 [amdgpu]) reg=0x500efb val=0x140800\n\
    <idle>-0 [010] d.h1.    32.786725: amdgpu_iv: ih:0 client_id:10 src_id:51 ring:0 vmid:0\n\
    bash-1 [003] .....    32.9: amdgpu_device_wreg: 0x1900, 0xnothex, 0x1\n";

#[test]
fn parse_every_event_kind() {
    let t = regtrace::parse(LINES);
    let k: Vec<(Kind, u32, u32, bool)> = t.events.iter().map(|e| (e.kind, e.reg, e.value, e.dc)).collect();
    assert_eq!(
        k,
        [
            (Kind::Read, 0x16091, 0xb1, false),
            (Kind::Write, 0x16085, 0x339000, false),
            (Kind::Write, 0x3846, 0x56, true),
            (Kind::Read, 0x3847, 0x185600f0, true),
            (Kind::SmnWrite, 0x11180718, 0x18000000, false),
            (Kind::SmnRead, 0x3b10028, 1, false),
            (Kind::PortRead, 0x500efb, 0, false),
            (Kind::PortWrite, 0x500efb, 0x140800, false),
        ]
    );
    assert_eq!(t.events[0].t_us, 31_765_661);
    assert_eq!(t.events[5].line, 9, "an indirect read keeps its first line");
    assert_eq!((t.other, t.bad.as_slice(), t.dc_unpaired), (2, &[15u32][..], 0));
    assert_eq!(t.writes(false), [(0x16085 * 4, 0x339000)]);
    assert_eq!(t.writes(true).len(), 2);
}

#[test]
fn parse_never_panics() {
    // Every cut of the text, odd bytes, half lines: counted, never a panic.
    for i in 0..LINES.len() {
        let _ = regtrace::parse(&LINES[..i]);
        let _ = regtrace::parse(&LINES[i..]);
    }
    let t = regtrace::parse("x: amdgpu_device_wreg: \nä 1.2: amdgpu_dc_wreg: reg=0x, value=\n 9.: gx_ind_rreg_ret: val=0x1\n 1.0: gx_ind_rreg: addr=0x1\n");
    assert!(t.events.is_empty() && t.bad.len() == 4, "{:?}", t.bad);
    let t = regtrace::parse(" 1.000001: amdgpu_dc_wreg: reg=0x10, value=0x1\n");
    assert_eq!(t.dc_unpaired, 1);
}

fn n(rows: &[(u32, &str)]) -> Names {
    rows.iter().map(|&(b, s)| (b, std::vec![s.to_string()])).collect()
}

#[test]
fn diff_missing_extra_reordered() {
    let names = n(&[(0x10, "RLC_CNTL"), (0x14, "RLC_SRM_CNTL"), (0x18, "RLC_CSIB_ADDR_LO"), (0x20, "SDMA0_CNTL"), (0x24, "SDMA0_F32_CNTL")]);
    let ph = &PHASES[3];
    assert_eq!(ph.name, "imu_rlc");
    let linux = [(0x10, 1), (0x14, 3), (0x18, 0xAAA), (0x20, 7)];
    // Same writes, a different CSIB address (Rule::Any): equal.
    let r = regdiff::diff_phase(ph, &linux, &[(0x10, 1), (0x14, 3), (0x18, 0xBBB)], &names, &[]);
    assert_eq!((r.linux, r.ours, r.matched, r.diffs.len()), (3, 3, 3, 0), "{r:?}");
    // One missing, one extra, one value wrong (missing + extra), one out of place (reordered).
    let r = regdiff::diff_phase(ph, &linux, &[(0x14, 3), (0x10, 1), (0x14, 4), (0x18, 1)], &names, &[]);
    let kinds: Vec<(DiffKind, &str, u32)> = r.diffs.iter().map(|d| (d.kind, d.name.as_str(), d.value)).collect();
    assert_eq!(kinds, [(DiffKind::Reordered, "RLC_CNTL", 1), (DiffKind::Extra, "RLC_SRM_CNTL", 4)], "{}", regdiff::render(&[r.clone()]));
    let r = regdiff::diff_phase(ph, &linux, &[(0x10, 1), (0x18, 5), (0x14, 9)], &names, &[]);
    assert_eq!((r.count(DiffKind::Missing), r.count(DiffKind::Extra), r.count(DiffKind::Reordered)), (1, 1, 0));
    // An allowed difference stays in the report, marked, and does not fail; the count is honoured.
    let allow = [Allow { phase: "imu_rlc", name: "RLC_SRM", kind: Some(DiffKind::Missing), count: 1, why: "test" }];
    let r = regdiff::diff_phase(ph, &linux, &[(0x10, 1), (0x18, 5)], &names, &allow);
    assert_eq!(r.failures().count(), 0);
    assert_eq!(r.diffs[0].allowed, Some("test"));
    let r = regdiff::diff_phase(ph, &[(0x14, 3), (0x14, 3)], &[], &names, &allow);
    assert_eq!(r.failures().count(), 1);
    // SDMA writes are not this phase's.
    assert!(regdiff::select(ph, &linux, &names).iter().all(|&(_, b, _)| b != 0x20));
}

/// GX-7c fixer: the common prefix and suffix are matched without the table; the table is bounded, and a
/// middle over the bound falls back to a greedy match marked approximate (no huge allocation).
#[test]
fn lcs_is_bounded() {
    let k = |v: &[u32]| -> Vec<(u32, u32)> { v.iter().map(|&x| (x, 0)).collect() };
    let (a, b) = (k(&[1, 2, 3, 9, 4, 5, 6]), k(&[1, 2, 4, 3, 5, 6]));
    let (exact, ok) = regdiff::lcs(&a, &b, regdiff::LCS_CELLS);
    assert!(ok);
    assert_eq!(exact.len(), 5, "{exact:?}");
    assert!(exact.windows(2).all(|p| p[0].0 < p[1].0 && p[0].1 < p[1].1));
    // Middle [3, 9, 4] x [4, 3] needs 12 cells: with 11 the match is greedy, still a valid common subsequence.
    let (greedy, ok) = regdiff::lcs(&a, &b, 11);
    assert!(!ok);
    assert!(greedy.iter().all(|&(i, j)| a[i] == b[j]) && greedy.windows(2).all(|p| p[0].0 < p[1].0 && p[0].1 < p[1].1));
    assert_eq!(regdiff::lcs(&a, &b, 12).1, true);
    // A whole-trace sized phase-less diff: 40 000 x 40 000 writes with a different middle would need 6.4 GB.
    let names: Names = (0..64u32).map(|i| (i * 4, std::vec![std::format!("RLC_R{i}")])).collect();
    let linux: Vec<(u32, u32)> = (0..40_000u32).map(|i| ((i % 64) * 4, i)).collect();
    let mut ours = linux.clone();
    for w in &mut ours[100..39_900] {
        w.1 ^= 1;
    }
    let r = regdiff::diff_phase(&PHASES[3], &linux, &ours, &names, &[]);
    assert!(r.approximate && r.matched == 200, "{} matched", r.matched);
    assert!(regdiff::render(&[r]).contains("approximate"));
    // Equal sequences of any length: all prefix, exact.
    let r = regdiff::diff_phase(&PHASES[3], &linux, &linux, &names, &[]);
    assert!(!r.approximate && r.matched == 40_000 && r.diffs.is_empty());
}

#[test]
fn diff_window_picks_the_mes_part_only() {
    let names = n(&[(0x10, "GRBM_GFX_CNTL"), (0x14, "CP_MES_CNTL"), (0x18, "CP_HQD_ACTIVE"), (0x1c, "CP_HQD_VMID")]);
    let mes = &PHASES[2];
    let seq = [(0x10, 0x10), (0x14, 0x4003_0000), (0x10, 0xc), (0x18, 1), (0x10, 0x0), (0x1c, 0)];
    let sel: Vec<usize> = regdiff::select(mes, &seq, &names).into_iter().map(|(i, _, _)| i).collect();
    assert_eq!(sel, [1, 2, 3], "from CP_MES_CNTL through CP_HQD_ACTIVE");
    assert!(regdiff::select(mes, &[(0x10, 1)], &names).is_empty(), "no window without its start");
}

/// The generated tables (host/gen/dcn314/tables) at the fake's addresses: BAR5 byte -> names.
pub(super) fn table_names() -> Names {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../host/gen/dcn314/tables");
    let d = fakechip::discovery_780m();
    let mut out = Names::new();
    for (hw, t) in [
        (hwid::GC, "gc_11_0_0"),
        (hwid::MP0, "mp_13_0_4"),
        (hwid::MMHUB, "mmhub_3_0_1"),
        (hwid::OSSSYS, "osssys_6_0_0"),
        (hwid::NBIF, "nbio_7_7_0"),
        (hwid::SMUIO, "smuio_13_0_6"),
        (hwid::HDP, "hdp_5_2_1"),
    ] {
        let text = std::fs::read_to_string(dir.join(std::format!("{t}.tsv"))).unwrap();
        for l in text.lines().filter(|l| !l.starts_with('#')) {
            let f: Vec<&str> = l.split('\t').collect();
            let (name, seg, dw) = (f[0], f[1].parse::<usize>().unwrap(), u32::from_str_radix(f[2].trim_start_matches("0x"), 16).unwrap());
            let owner = if name.starts_with("MP1_") { hwid::MP1 } else { hw };
            if let Some(b) = d.reg(owner, 0, seg, dw).and_then(|x| x.checked_mul(4)) {
                let v = out.entry(b).or_default();
                if !v.iter().any(|x| x == name) {
                    v.push(name.to_string());
                }
            }
        }
    }
    out
}

/// The 27.10. capture, when the private plan/hw is there and zstd is installed (decompressed once).
pub(super) fn capture() -> Option<&'static Trace> {
    static T: std::sync::OnceLock<Option<Trace>> = std::sync::OnceLock::new();
    T.get_or_init(|| {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../plan/hw/gx-780m-linux-trace-2026-10-27/gx-6d9cae1d/trace-load.txt.zst");
        if !p.exists() {
            std::eprintln!("skip: {} missing (plan/ is not public)", p.display());
            return None;
        }
        match std::process::Command::new("zstd").arg("-dc").arg(&p).output() {
            Ok(o) if o.status.success() => Some(regtrace::parse(&String::from_utf8_lossy(&o.stdout))),
            r => {
                std::eprintln!("skip: zstd -dc failed: {:?}", r.map(|o| o.status));
                None
            }
        }
    })
    .as_ref()
}

/// Known differences of our PSP phase, with the reason.
const ALLOW_PSP: &[Allow] = &[Allow {
    phase: "psp",
    name: "MP0_SMN_C2PMSG_67",
    kind: Some(DiffKind::Missing),
    count: 7,
    why: "Linux sends 27 KM-ring frames, BitsOS 20 on a cold start (GX-7k: MES's KIQ loaded as Linux): DMCUB (kept, GX-D9), VCN (GX-7d loads none), the TA loads and two late commands (the trace does not show which)",
}];

#[test]
fn linux_capture_whole_trace_reads() {
    let Some(t) = capture() else { return };
    assert_eq!(t.events.len(), 67_850);
    assert!(t.bad.is_empty() && t.dc_unpaired == 0, "{:?}", &t.bad[..t.bad.len().min(5)]);
    assert_eq!(t.events.iter().filter(|e| e.kind == Kind::Write).count(), 14_754);
    assert_eq!(t.events.iter().filter(|e| e.kind == Kind::Write && e.dc).count(), 11_337, "DC's writes, each logged twice");
}

#[test]
fn linux_capture_against_our_bringup() {
    let Some(t) = capture() else { return };
    let names = table_names();
    // GX-7e: the engine the desk runs (`pspbringup::run`, Until::Gfx, kept: no undo), not the 7c test driver.
    let d = fakechip::discovery_780m();
    let opts = crate::gpucore::ucode::ListOpts::PSP13_0_4;
    let (out, ..) = crate::ktests_bringup::one(&d, 512 << 20, crate::gpucore::fakechip::StartAs::Cold, crate::gpucore::psp::Until::Gfx, true, Hostile::None, &opts, &[]).unwrap();
    assert!(out.passed(), "{:?}", out.gfx.as_ref().map(|g| g.error));
    let ours: Vec<(u32, u32)> = out.entries.iter().map(|e| (e.byte, e.value)).collect();
    let linux = t.writes(false);
    let smu = |name| Allow { phase: "smu", name, kind: Some(DiffKind::Missing), count: 18, why: "GX-7e sends three messages (the rest: later or never)" };
    let allow = [
        Allow { count: 7, ..ALLOW_PSP[0] },
        smu("MP1_SMN_C2PMSG_66"),
        smu("MP1_SMN_C2PMSG_82"),
        smu("MP1_SMN_C2PMSG_90"),
    ];
    let reports = regdiff::diff(PHASES, &linux, &ours, &names, &allow);
    std::eprintln!("{}", regdiff::render(&reports));
    let got: Vec<(&str, usize, usize, usize)> = reports.iter().map(|r| (r.name, r.linux, r.ours, r.matched)).collect();
    assert_eq!(got, [("psp", 31, 24, 24), ("smu", 63, 9, 9), ("mes", 29, 0, 0), ("imu_rlc", 18, 0, 0), ("sdma", 26, 0, 0), ("gmc_mmhub", 143, 0, 0), ("gmc_gfxhub", 143, 0, 0), ("gfx_tail", 165, 0, 0), ("gfx_ring", 6, 0, 0)]);
    for r in &reports[..2] {
        assert_eq!(r.failures().count(), 0, "{} differs from Linux beyond the known: {}", r.name, regdiff::render(core::slice::from_ref(r)));
        assert_eq!((r.count(DiffKind::Extra), r.count(DiffKind::Reordered)), (0, 0), "{}", r.name);
    }
    // The phases this run does not reach (M3 only reads IMU/RLC; GMC is until=gart, tests::gmc): every Linux write
    // missing, none of ours.
    for r in &reports[2..] {
        assert!(r.linux > 0 && r.ours == 0 && r.count(DiffKind::Missing) == r.linux, "{}: {r:?}", r.name);
    }
}

#[test]
fn linux_capture_smu_messages_are_the_fakes_table() {
    let Some(t) = capture() else { return };
    let f = FakeGpu::new(Hostile::None);
    let m = f.smu.drv;
    let (msg, arg) = (m.msg_at / 4, m.arg_at / 4);
    let mut got: Vec<(u32, u32, Option<u32>)> = Vec::new();
    let mut last_arg = 0;
    let mut answered = false;
    for e in &t.events {
        match e.kind {
            Kind::Write if e.reg == arg => last_arg = e.value,
            Kind::Write if e.reg == msg => {
                got.push((e.value, last_arg, None));
                answered = false;
            }
            Kind::Read if e.reg == m.resp_at / 4 && e.value != 0 => answered = true,
            Kind::Read if e.reg == arg && answered => {
                if let Some(g) = got.last_mut().filter(|g| g.2.is_none()) {
                    g.2 = Some(e.value);
                }
            }
            _ => {}
        }
    }
    assert_eq!(got, smufake::LINUX_LOAD);
}

#[test]
fn linux_capture_through_the_watcher() {
    let Some(t) = capture() else { return };
    let g = DisplayGuard::from_discovery(&fakechip::discovery_780m());
    let mut dcn_dc = 0;
    let mut mmhub = BTreeMap::new();
    for e in t.events.iter().filter(|e| e.kind == Kind::Write) {
        let Some(b) = e.byte() else { continue };
        match g.class(b) {
            Some(Class::Dcn) if e.dc => dcn_dc += 1,
            Some(Class::Mmhub(n)) if !e.dc => *mmhub.entry(n).or_insert(0) += 1,
            _ => {}
        }
    }
    // Linux's own DC writes the display (it owns it), and its GMC init rewrites the fetch path (the APU's FB
    // location is the BIOS's and stays): exactly what a BitsOS bring-up must not do under the firmware's picture.
    assert!(dcn_dc > 10_000, "{dcn_dc}");
    assert!(!mmhub.contains_key("MMMC_VM_FB_LOCATION_BASE") && !mmhub.contains_key("MMMC_VM_FB_OFFSET"));
    for n in ["MMMC_VM_SYSTEM_APERTURE_LOW_ADDR", "MMVM_CONTEXT0_CNTL", "MMMC_VM_MX_L1_TLB_CNTL", "MMVM_L2_CNTL"] {
        assert!(mmhub.get(n).is_some_and(|&c| c > 0), "{n}: {mmhub:?}");
    }
    // Our PSP bring-up: none.
    let mut f = FakeGpu::new(Hostile::None);
    pspfake::run(&mut f, &mut MapSource(pspfake::synthetic_780m())).unwrap();
    assert!(f.writes.iter().all(|&(b, _)| g.class(b).is_none()));
}

/// The harness over a whole phase: Linux's protocol (`smufake::linux_send`) with Linux's messages against the
/// PMFW fake writes exactly what Linux wrote in the SMU phase (the template GX-7e's client is diffed against).
#[test]
fn linux_capture_smu_phase_replayed_on_the_fake() {
    let Some(t) = capture() else { return };
    let mut f = FakeGpu::new(Hostile::None);
    let m = f.smu.drv;
    for &(id, arg, _) in smufake::LINUX_LOAD {
        smufake::linux_send(&mut f, &m, id, arg, id != smufake::msg::ENABLE_GFX_IMU, 10, 2_000_000).unwrap();
    }
    let r = regdiff::diff_phase(&PHASES[1], &t.writes(false), &f.writes, &table_names(), &[]);
    assert_eq!((r.linux, r.ours, r.matched, r.diffs.len()), (63, 63, 63, 0), "{}", regdiff::render(&[r.clone()]));
}
