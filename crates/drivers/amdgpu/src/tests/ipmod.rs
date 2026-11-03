//! GX-6m module choice: the 780M's versions pick the right modules and firmware; an RDNA 4 table reports exactly
//! which modules are missing; a headless or harvested IP needs none; the probe's report and register addresses.

use super::blob::{Blob, build, ip, minipc_ips};
use crate::gpucore::disc::parse;
use crate::gpucore::hwid as h;
use crate::ip::{self, IpKind, Source};
use crate::ipmod::{self, Choice, MODULES, Role};
use crate::probe::{self, Found, Via};
use std::string::String;
use std::vec;
use std::vec::Vec;

fn names(p: &ipmod::Plan) -> Vec<(Role, String)> {
    p.choices
        .iter()
        .map(|c| {
            let what = match *c {
                Choice::Chosen { module, .. } => String::from(module.name),
                Choice::Missing { version, .. } => std::format!("MISSING {version}"),
                Choice::Harvested { .. } => String::from("harvested"),
                Choice::Absent { .. } => String::from("absent"),
            };
            (c.role(), what)
        })
        .collect()
}

#[test]
fn minipc_versions_pick_the_780m_modules() {
    let d = parse(&build(&Blob::new(minipc_ips())).0).unwrap();
    let p = ipmod::plan(&d);
    let got = names(&p);
    let want = [
        (Role::Psp, "psp13_0_4"),
        (Role::Smu, "smu13_0_4"),
        (Role::Nbio, "nbio7_7"),
        (Role::Gmc, "gmc11"),
        (Role::Gfxhub, "gfxhub3_0"),
        (Role::Mmhub, "mmhub3_0_1"),
        (Role::Ih, "ih6_0"),
        (Role::Gfx, "gfx11_0_1"),
        (Role::Mes, "mes11"),
        (Role::Sdma, "sdma6_0_1"),
        (Role::Display, "dcn3_1_4"),
        (Role::Vcn, "vcn4_0_2"),
    ];
    assert_eq!(got, want.iter().map(|&(r, n)| (r, String::from(n))).collect::<Vec<_>>());
    assert_eq!(p.missing().count(), 0);
    // Firmware: exactly the files linux-firmware has for this GPU (fw_files.txt of the Mini-PC, 27.10.); MES's
    // `_mes` is the fallback of `_mes_2`; no SMU file (BIOS).
    let fw = p.firmware();
    assert_eq!(
        fw,
        vec![
            "amdgpu/psp_13_0_4_toc.bin",
            "amdgpu/psp_13_0_4_ta.bin",
            "amdgpu/gc_11_0_1_pfp.bin",
            "amdgpu/gc_11_0_1_me.bin",
            "amdgpu/gc_11_0_1_mec.bin",
            "amdgpu/gc_11_0_1_rlc.bin",
            "amdgpu/gc_11_0_1_imu.bin",
            "amdgpu/gc_11_0_1_mes_2.bin|amdgpu/gc_11_0_1_mes.bin",
            "amdgpu/gc_11_0_1_mes1.bin",
            "amdgpu/sdma_6_0_1.bin",
            "amdgpu/dcn_3_1_4_dmcub.bin",
            "amdgpu/vcn_4_0_2.bin",
        ]
    );
    let on_minipc = [
        "dcn_3_1_4_dmcub", "gc_11_0_1_imu", "gc_11_0_1_me", "gc_11_0_1_mec", "gc_11_0_1_mes", "gc_11_0_1_mes1",
        "gc_11_0_1_mes_2", "gc_11_0_1_pfp", "gc_11_0_1_rlc", "psp_13_0_4_ta", "psp_13_0_4_toc", "sdma_6_0_1",
        "vcn_4_0_2",
    ];
    let listed: Vec<&str> = fw.iter().flat_map(|f| f.split('|')).collect();
    for f in on_minipc {
        assert!(listed.contains(&std::format!("amdgpu/{f}.bin").as_str()), "{f} not needed by any module");
    }
    assert_eq!(listed.len(), on_minipc.len());
}

/// RDNA 4 (RX 9070 XT, gfx1201). Linux 6.6 has none of these; the versions are the ones Linux 6.12's
/// amdgpu_discovery.c switches on for that family (GC 12.0.1 soc24/gfx_v12_0/gmc_v12_0/mes_v12_0, SDMA 7.0.1
/// sdma_v7_0, MP0/MP1 14.0.3 psp_v14_0/smu_v14_0, DCN reported 4.1.0, VCN 5.0.0, OSSSYS 7.0.0 ih_v7_0, MMHUB
/// 4.1.0, NBIF 6.3.1). Synthetic until the Gaming-PC's real dump replaces it (GX-10).
#[test]
fn rdna4_reports_exactly_the_missing_modules() {
    let ips = vec![
        ip(h::GC, 0, (12, 0, 1), &[0x1260, 0xA000]),
        ip(h::SDMA0, 0, (7, 0, 1), &[0x1260, 0xA000]),
        ip(h::SDMA1, 0, (7, 0, 1), &[0x1260, 0xA000]),
        ip(h::MP0, 0, (14, 0, 3), &[0x4000, 0x16000]),
        ip(h::MP1, 0, (14, 0, 3), &[0x4000, 0x16000]),
        ip(h::DMU, 0, (4, 1, 0), &[0x12, 0xC0, 0x34C0]),
        ip(h::VCN, 0, (5, 0, 0), &[0x7800]),
        ip(h::OSSSYS, 0, (7, 0, 0), &[0x10A0]),
        ip(h::MMHUB, 0, (4, 1, 0), &[0x13200, 0x1A000]),
        ip(h::NBIF, 0, (6, 3, 1), &[0, 0x14]),
        ip(h::HDP, 0, (7, 0, 0), &[0xF20]),
    ];
    let p = ipmod::plan(&parse(&build(&Blob::new(ips)).0).unwrap());
    let missing: Vec<(Role, String)> = p
        .missing()
        .map(|c| match *c {
            Choice::Missing { role, version, candidate } => {
                assert!(candidate.is_none(), "{}: no BitsOS module's Linux block serves {version}", role.name());
                (role, std::format!("{version}"))
            }
            _ => unreachable!(),
        })
        .collect();
    let want = [
        (Role::Psp, "14.0.3"),
        (Role::Smu, "14.0.3"),
        (Role::Nbio, "6.3.1"),
        (Role::Gmc, "12.0.1"),
        (Role::Gfxhub, "12.0.1"),
        (Role::Mmhub, "4.1.0"),
        (Role::Ih, "7.0.0"),
        (Role::Gfx, "12.0.1"),
        (Role::Mes, "12.0.1"),
        (Role::Sdma, "7.0.1"),
        (Role::Display, "4.1.0"),
        (Role::Vcn, "5.0.0"),
    ];
    assert_eq!(missing, want.iter().map(|&(r, v)| (r, String::from(v))).collect::<Vec<_>>());
    assert!(p.firmware().is_empty(), "no firmware for modules not chosen");
}

#[test]
fn neighbour_versions_are_candidates_not_choices() {
    // An RX 7600-class GC 11.0.3 with SDMA 6.0.3: Linux serves both with the same blocks; BitsOS's modules are
    // not checked for them, so they are missing with the candidate named.
    let ips = vec![ip(h::GC, 0, (11, 0, 3), &[1]), ip(h::SDMA0, 0, (6, 0, 3), &[1]), ip(h::MP1, 0, (13, 0, 7), &[1])];
    let p = ipmod::plan(&parse(&build(&Blob::new(ips)).0).unwrap());
    for (role, cand) in [(Role::Gfx, "gfx11_0_1"), (Role::Sdma, "sdma6_0_1"), (Role::Smu, "smu13_0_4"), (Role::Gfxhub, "")] {
        let c = p.choices.iter().find(|c| c.role() == role).unwrap();
        match *c {
            Choice::Missing { candidate, .. } => assert_eq!(candidate.map_or("", |m| m.name), cand, "{role:?}"),
            other => panic!("{role:?}: {other:?}"),
        }
    }
    // Headless: no DMU in the table, nothing needed for the display.
    assert!(matches!(p.choices.iter().find(|c| c.role() == Role::Display), Some(Choice::Absent { .. })));
}

#[test]
fn harvested_ip_needs_no_module() {
    let mut b = Blob::new(minipc_ips());
    b.harvest = Some(vec![(h::VCN, 0)]);
    let p = ipmod::plan(&parse(&build(&b).0).unwrap());
    assert!(matches!(p.choices.iter().find(|c| c.role() == Role::Vcn), Some(Choice::Harvested { .. })));
    assert!(!p.firmware().iter().any(|f| f.contains("vcn")));
}

#[test]
fn registry_is_unambiguous() {
    for (i, a) in MODULES.iter().enumerate() {
        assert!(!a.versions.is_empty(), "{}", a.name);
        for v in a.versions {
            assert!(!a.linux_also.contains(v), "{} {v}", a.name);
            for b in &MODULES[i + 1..] {
                assert!(!(a.role == b.role && b.versions.contains(v)), "{} / {} {v}", a.name, b.name);
            }
        }
    }
    for r in Role::ALL {
        assert!(MODULES.iter().any(|m| m.role == r), "no module for {r:?}");
    }
}

#[test]
fn blocks_from_discovery_keep_every_ip() {
    let d = parse(&build(&Blob::new(minipc_ips())).0).unwrap();
    let b = ip::from_discovery(&d);
    assert_eq!(b.len(), 39);
    assert!(b.iter().all(|x| x.source == Source::Discovery));
    assert_eq!(ip::block(&b, IpKind::Gc).unwrap().version, ipmod::v(11, 0, 1));
    assert_eq!(ip::block(&b, IpKind::Sdma).unwrap().version, ipmod::v(6, 0, 1));
    assert!(b.iter().any(|x| x.kind == IpKind::Other(282)));
    assert_eq!(b.iter().filter(|x| x.kind == IpKind::Umc).count(), 2);
}

#[test]
fn probe_report_and_register_addresses() {
    let (buf, _) = build(&Blob::new(minipc_ips()));
    let d = parse(&buf).unwrap();
    let regs = probe::reg_reads(&d, |b| Some(b ^ 0x5A5A));
    // PSP's status register lands where dcn314-regs' measured BAR5 constant says.
    assert_eq!(regs[0].byte, Some(dcn314_regs::bar5::MP0_C2PMSG_81));
    assert_eq!(regs[2].byte, Some((0x1A000 + 0x8D7) * 4));
    let found = Found { via: Via::CarveOut { phys: 0x1234_0000 }, bytes: buf.clone() };
    let s = probe::report(&found, &Ok(d.clone()), &regs, &ip::known_blocks(0x1900), true);
    let first = s.lines().next().unwrap();
    assert!(first.starts_with("discovery: ok via carve-out at phys 0x12340000"), "{first}");
    assert!(s.contains("\nip GC 11 0 11.0.1 var 0 sub 0 harvested no bases 0x00001260 0x0000a000 0x02402c00\n"), "{s}");
    assert!(s.contains("\nip ? 282 0 13.0.1 "));
    assert!(s.contains("\nmodule display 3.1.4: dcn3_1_4 (present: "));
    assert!(s.contains("\nfirmware smu13_0_4: none (APU: the SMU firmware is part of the BIOS)\n"));
    assert!(s.contains("\nfirmware amdgpu/psp_13_0_4_toc.bin\n"));
    assert!(!s.contains("DIFFERS") && s.contains("table Gc 11.0.1: agrees"), "{s}");
    assert!(s.contains("reg MP0_C2PMSG_81 (PSP sOS alive) @0x58244 = "));
    let hex = s.lines().find_map(|l| l.strip_prefix("hex ")).unwrap();
    assert_eq!(hex.len(), 2 * usize::from(d.binary_size));
    // Refused: the reason and the first bytes, nothing else.
    let mut bad = buf.clone();
    bad[0] = 0;
    let r = crate::gpucore::disc::parse(&bad);
    let s = probe::report(&Found { via: Via::Indirect { pos: 0x1FFF_0000 }, bytes: bad }, &r, &[], &[], false);
    assert!(s.starts_with("discovery: REFUSED (binary signature 0x28211400"), "{s}");
    assert_eq!(s.lines().count(), 2);
}

#[test]
fn gx_probe_arguments() {
    let j = |s: &str| mcp::Json::parse(s).unwrap();
    assert_eq!(crate::tools_gx::parse_args(&j("{}")), Ok(false));
    assert_eq!(crate::tools_gx::parse_args(&j(r#"{"hex":true}"#)), Ok(true));
    assert!(matches!(crate::tools_gx::parse_args(&j(r#"{"hex":1}"#)), Err(mcp::ToolError::BadArgument(_))));
}
