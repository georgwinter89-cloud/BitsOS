//! The snapshot: which registers, what is skipped, JSON round trip, the diff and its volatile bits.

use super::fake::*;
use crate::layout as l;
use crate::attach::Regs;
use crate::snapshot::{self, Group, Reference, Scope};
use alloc::collections::BTreeMap;

fn wide(m: &BTreeMap<u32, u32>) -> Reference {
    Reference { regs: m.clone(), scope: Scope::Snapshot { wide: true } }
}
use dcn314_regs::bar5;

const G1: &str = include_str!("../../../../dcn314-regs/tests/data/p1-decoded.txt");

#[test]
fn register_list() {
    let r = snapshot::registers();
    assert_eq!(r.len(), 326, "the snapshot list (notes: gx-GX-2.md)");
    assert!(r.windows(2).all(|w| w[0].byte < w[1].byte), "sorted, each once");
    for &(b, _) in dcn314_regs::P1_SNAPSHOT {
        assert!(r.iter().any(|e| e.byte == b), "G-1 register {b:#x} missing");
    }
    for e in &r {
        assert!(
            !bar5::NBIO_RSMU_ALIAS.contains(&e.byte) && e.byte != 0xE118 && e.byte != 0xE11C && e.byte < 0x8_0000,
            "{:#x}",
            e.byte
        );
    }
    let group = |b| r.iter().find(|e| e.byte == b).unwrap().group;
    assert_eq!(group(0x0EACC), Group::Pipe(0), "HUBP0_DCHUBP_CNTL");
    assert_eq!(group(0x106EC), Group::Pipe(0), "DSCL0_SCL_MODE");
    assert_eq!(group(l::hubp(3, l::HUBP_CNTL)), Group::Pipe(3));
    assert_eq!(group(0x14004), Group::Global, "OTG0_OTG_CONTROL");
    assert_eq!(group(l::dp(0, l::DP_LINK_CNTL)), Group::DpStream(0));
    assert_eq!(group(l::otg(1, l::OTG_CONTROL)), Group::Otg(1));
    assert_eq!(group(l::dto_modulo(2)), Group::Otg(2));
    assert_eq!(group(l::dig(1, l::DIG_BE_EN_CNTL)), Group::Dig(1));
    assert_eq!(group(l::dig(0, l::DIG_BE_EN_CNTL)), Group::Dig(0));
    assert_eq!(group(bar5::MP0_C2PMSG_81), Group::Global);
}

#[test]
fn take_skips_gated_pipes_and_off_encoders() {
    let mut img = mini_pc();
    img.insert(l::pg_status(2), 0x4000_0000);
    let s = snapshot::take(&reader(&img), true);
    assert_eq!(s.skipped, ["pipe1: power domain gated", "pipe2: power domain gated"]);
    assert!(s.regs.contains_key(&l::hubp(0, l::HUBP_CNTL)) && s.regs.contains_key(&l::hubp(3, l::HUBP_CNTL)));
    assert!(!s.regs.contains_key(&l::hubp(1, l::HUBP_CNTL)) && !s.regs.contains_key(&l::hubp(2, l::HUBP_ADDR)));
    assert!(
        s.regs.contains_key(&l::dp(0, l::DP_LINK_CNTL)) && !s.regs.contains_key(&l::dp(1, l::DP_LINK_CNTL)),
        "DP1's back end is off"
    );
    assert_eq!(s.regs.get(&0x0EACC), Some(&0x300F_1002));
}

/// Narrow (the default): OTG0 and DIG0/DP0, the OTG a powered pipe drives, nothing of DIG1-4 or OTG1-3 else.
#[test]
fn narrow_reads_only_measured_instances() {
    let img = mini_pc();
    let n = snapshot::take(&reader(&img), false);
    let w = snapshot::take(&reader(&img), true);
    assert!(!n.wide && w.wide && n.regs.len() < w.regs.len());
    for k in 1..l::OTGS {
        assert!(!n.regs.contains_key(&l::otg(k, l::OTG_CONTROL)) && !n.regs.contains_key(&l::dto_cntl(k)), "OTG{k}");
        assert!(w.regs.contains_key(&l::otg(k, l::OTG_CONTROL)), "wide OTG{k}");
    }
    for d in 1..l::DIGS {
        assert!(!n.regs.contains_key(&l::dig(d, l::DIG_BE_EN_CNTL)), "DIG{d}");
    }
    assert!(n.regs.contains_key(&l::otg(0, l::OTG_CONTROL)) && n.regs.contains_key(&l::dp(0, l::DP_LINK_CNTL)));
    assert!(n.skipped.iter().any(|x| x.starts_with("OTG1: ")) && n.skipped.iter().any(|x| x.starts_with("DIG4: ")));
    // Every register narrow reads is one wide reads with the same value; the rest is wide-only.
    for (b, v) in &n.regs {
        assert_eq!(w.regs.get(b), Some(v));
    }
    let groups: BTreeMap<u32, Group> = snapshot::registers().into_iter().map(|e| (e.byte, e.group)).collect();
    for b in w.regs.keys().filter(|b| !n.regs.contains_key(b)) {
        assert!(groups[b].wide_only(), "{b:#x} only in wide but not wide-only");
    }
    // A powered, unblanked pipe on OTG2 brings OTG2 (and its DTO) into the narrow scope; the blanked pipes 1-3
    // of the G-1 image (on OTG1 and OTG3) do not.
    let mut img2 = img.clone();
    let c = img2[&l::hubp(3, l::HUBP_CNTL)];
    img2.insert(l::hubp(3, l::HUBP_CNTL), c & !0xF0 & !l::BLANKED | 2 << 4);
    let n2 = snapshot::take(&reader(&img2), false);
    assert!(n2.regs.contains_key(&l::otg(2, l::OTG_CONTROL)) && n2.regs.contains_key(&l::dto_modulo(2)));
    assert!(!n2.regs.contains_key(&l::otg(1, l::OTG_CONTROL)));
    // Narrow against a wide reference and the other way round: equal, the wide-only part outside the scope.
    let nr = snapshot::parse_reference(&snapshot::to_json(&n, "c7:00.0", "ok")).unwrap();
    assert_eq!(nr.scope, Scope::Snapshot { wide: false });
    let r = snapshot::compare(&nr, &w.regs, true);
    assert!(r.equal() && r.out_of_scope.len() == w.regs.len() - n.regs.len(), "{}", snapshot::render(&r));
    let r = snapshot::compare(&wide(&w.regs), &n.regs, false);
    assert!(r.equal() && !r.out_of_scope.is_empty(), "{}", snapshot::render(&r));
    assert!(snapshot::render(&r).contains("outside the other side's scope not compared"));
    // Same scope: a pipe that powered down is a difference (a real change of state).
    let mut gated = img.clone();
    gated.insert(l::pg_status(3), 0x4000_0000);
    let g = snapshot::take(&reader(&gated), false);
    let r = snapshot::compare(&nr, &g.regs, false);
    assert!(!r.equal() && !r.only_reference.is_empty(), "{}", snapshot::render(&r));
}

/// The full tool route over the fake image: the answer's JSON line stored as a file, read back, compared.
#[test]
fn stored_reference_file_round_trip() {
    use crate::tools::{self, Args};
    let img = mini_pc();
    let head = [alloc::string::String::from("gpu: fake")];
    let none = Args { wide: false, reference: None };
    let text = tools::respond(&reader(&img), &none, &head, "c7:00.0", "ok").unwrap();
    let json = text.lines().last().unwrap();
    let dir = std::env::temp_dir().join(alloc::format!("gx2-ref-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("dcn-ref.json");
    std::fs::write(&path, alloc::format!("{json}\n")).unwrap();
    let p = path.to_str().unwrap();
    let args = mcp::Json::parse(&alloc::format!("{{\"compare\":{}}}", mcp::Json::str(p).to_string())).unwrap();
    let read = |q: &str| std::fs::read(q).map_err(|e| mcp::ToolError::Failed(alloc::format!("{q}: {e}")));
    let a = tools::parse_args(&args, read).unwrap();
    let again = tools::respond(&reader(&img), &a, &head, "c7:00.0", "ok").unwrap();
    assert!(again.starts_with("equal: "), "{again}");
    // The same file against a changed image: the difference names the register.
    let mut changed = img.clone();
    changed.insert(l::otg(0, l::OTG_H_TOTAL), 0xA9E);
    let d = tools::respond(&reader(&changed), &tools::parse_args(&args, read).unwrap(), &head, "c7:00.0", "ok").unwrap();
    assert!(d.starts_with("differ: 1 of ") && d.contains("OTG0_OTG_H_TOTAL"), "{d}");
    // Argument errors.
    let bad = |a: &str| tools::parse_args(&mcp::Json::parse(a).unwrap(), read).err();
    assert!(matches!(bad(r#"{"compare":"/x","reference":"y"}"#), Some(mcp::ToolError::BadArgument(_))));
    assert!(matches!(bad(r#"{"compare":1}"#), Some(mcp::ToolError::BadArgument(_))));
    assert!(matches!(bad(r#"{"wide":1}"#), Some(mcp::ToolError::BadArgument(_))));
    assert!(matches!(bad(r#"{"compare":"/nonexistent/gx2"}"#), Some(mcp::ToolError::Failed(_))));
    std::fs::write(dir.join("bin"), [0xFFu8, 0xFE]).unwrap();
    let bin = alloc::format!("{{\"compare\":{}}}", mcp::Json::str(dir.join("bin").to_str().unwrap()).to_string());
    assert!(matches!(bad(&bin), Some(mcp::ToolError::Failed(s)) if s.contains("not UTF-8")));
    let r = tools::respond(&reader(&img), &Args { wide: false, reference: Some("{\"regs\":3}".into()) }, &head, "f", "ok");
    assert!(matches!(r, Err(mcp::ToolError::Failed(_))));
    let _ = std::fs::remove_dir_all(&dir);
}

/// `readable` is the only guard of `impl Regs for DcnRegs` against the NBIO aliases, the AZ pair, unaligned and
/// out-of-window offsets: pinned here, and through the fake reader that uses it.
#[test]
fn readable_guard() {
    let alias = bar5::NBIO_RSMU_ALIAS;
    let (az_index, az_data) = (crate::dcn::AZ_ENDPOINT_INDEX, crate::dcn::AZ_ENDPOINT_DATA);
    let refused =
        [alias.start, alias.end - 4, az_index, az_data, 0xE118, 0xE11C, 0x8_0000, 0x8_0004, u32::MAX & !3, 0x3, 0xE31A];
    let img = mini_pc();
    let r = reader(&img);
    for b in refused {
        assert!(!snapshot::readable(b), "{b:#x} readable");
        assert_eq!(Regs::read(&r, b), None, "{b:#x} read");
    }
    for b in [0xE318, l::hubp(0, l::HUBP_CNTL), l::otg(0, l::OTG_CONTROL), bar5::MP0_C2PMSG_81, 0x7_FFFC] {
        assert!(snapshot::readable(b), "{b:#x} not readable");
    }
    assert_eq!(Regs::read(&r, 0xE318), Some(crate::dcn::GATE_VALUE));
}

#[test]
fn json_round_trip_and_self_compare() {
    let s = snapshot::take(&reader(&mini_pc()), false);
    let j = snapshot::to_json(&s, "c7:00.0", "ok");
    assert!(
        j.starts_with("{\"format\":\"dcn-snapshot/1\",\"function\":\"c7:00.0\",\"attach\":\"ok\",\"wide\":false,\"read\":"),
        "{j}"
    );
    assert!(j.contains("\"0EACC\":\"300F1002\""));
    let back = snapshot::parse_reference(&j).unwrap();
    assert_eq!(back.regs, s.regs);
    let r = snapshot::compare(&back, &s.regs, false);
    assert!(r.equal() && r.compared == s.regs.len());
    assert!(snapshot::render(&r).starts_with("equal: "));
    assert!(mcp::Json::parse(&j).is_ok());
}

#[test]
fn volatile_bits_are_not_compared() {
    let s = snapshot::take(&reader(&mini_pc()), true).regs;
    let changed = |b: u32, v: u32| {
        let mut n = s.clone();
        n.insert(b, v);
        snapshot::compare(&wide(&s), &n, true)
    };
    // Heartbeat: any non-zero value is the same; zero is not.
    assert!(changed(bar5::MP0_C2PMSG_81, 1234).equal() || s.get(&bar5::MP0_C2PMSG_81) == Some(&0));
    let mut hb = s.clone();
    hb.insert(bar5::MP0_C2PMSG_81, 5);
    let mut hb2 = s.clone();
    hb2.insert(bar5::MP0_C2PMSG_81, 77);
    assert!(snapshot::compare(&wide(&hb), &hb2, true).equal());
    hb2.insert(bar5::MP0_C2PMSG_81, 0);
    assert!(!snapshot::compare(&wide(&hb), &hb2, true).equal());
    // Sticky underflow, outstanding requests, a pending flip, the update lock status: ignored.
    assert!(changed(0x0EACC, 0x300F_1002 & !0x7000_0000).equal(), "HUBP_UNDERFLOW_STATUS");
    assert!(changed(0x0EACC, 0x300F_1000).equal(), "HUBP_NO_OUTSTANDING_REQ");
    assert!(changed(0x0EB6C, 0x410_0100).equal(), "SURFACE_FLIP_PENDING");
    assert!(changed(0x14128, 0x100).equal(), "UPDATE_LOCK_STATUS");
    // Configuration is compared: HUBP_TTU_DISABLE, the lock itself, the surface address.
    let r = changed(0x0EACC, 0x300F_0002);
    assert!(!r.equal() && !r.differ[0].mode, "HUBP_TTU_DISABLE is mode-independent");
    assert!(!changed(0x14128, 1).equal(), "OTG_MASTER_UPDATE_LOCK");
    assert!(snapshot::render(&r).contains("HUBP_TTU_DISABLE=0x1->0x0"), "{}", snapshot::render(&r));
    let r = changed(0x0EB28, 0x200_0000);
    assert!(!r.equal() && r.differ[0].mode, "the surface address follows the mode");
}

#[test]
fn g1_reference_lines() {
    let g1 = snapshot::parse_reference(G1).unwrap();
    assert_eq!((g1.regs.len(), g1.scope), (dcn314_regs::P1_SNAPSHOT.len(), Scope::G1));
    let now = snapshot::take(&reader(&mini_pc()), false).regs;
    let r = snapshot::compare(&g1, &now, false);
    assert!(r.differ.is_empty() && r.only_reference.is_empty() && r.only_now.is_empty(), "{}", snapshot::render(&r));
    assert!(r.equal() && r.out_of_scope.len() == now.len() - 179, "what G-1 did not read is outside its scope");
    // Against G-1, the native mode's viewport is a mode-dependent difference, the timing a fixed one.
    let mut native = now.clone();
    native.insert(l::hubp(0, l::HUBP_VIEWPORT), 1440 << 16 | 2560);
    native.insert(l::otg(0, l::OTG_H_TOTAL), 0xA9E);
    let r = snapshot::compare(&g1, &native, false);
    let classes: alloc::vec::Vec<_> = r.differ.iter().map(|d| (d.byte, d.mode)).collect();
    assert_eq!(classes, [(l::hubp(0, l::HUBP_VIEWPORT), true), (l::otg(0, l::OTG_H_TOTAL), false)]);
    assert!(
        snapshot::render(&r).starts_with("differ: 2 of 179 registers (1 mode-dependent, 1 mode-independent)"),
        "{}",
        snapshot::render(&r)
    );
}

#[test]
fn bad_references() {
    assert!(snapshot::parse_reference("").is_err());
    assert!(snapshot::parse_reference("{\"regs\":{\"0EACC\":3}}").is_err());
    assert!(snapshot::parse_reference("{\"nope\":1}").is_err());
    assert!(snapshot::parse_reference("{").is_err());
    assert!(snapshot::parse_reference("0x0EACC zz").is_err());
}
/// The volatile list is a decision about each field: this pins it, so a regenerated header or a new register in
/// the list can not widen it unnoticed.
#[test]
fn volatile_fields_are_exactly() {
    let mut names: alloc::vec::Vec<&str> = snapshot::registers()
        .iter()
        .filter_map(|e| match snapshot::rule(e.byte) {
            snapshot::Rule::Bits { ignore } if ignore != 0 => Some((e.byte, ignore)),
            _ => None,
        })
        .flat_map(|(b, ignore)| {
            dcn314_regs::at_byte(b).flat_map(|r| r.fields.iter()).filter(move |f| f.mask & ignore != 0).map(|f| f.name)
        })
        .collect();
    names.sort_unstable();
    names.dedup();
    let want = [
        "CNVC_UPDATE_PENDING",
        "COMPBUF_SIZE_CHANGE_DONE_INT_STATUS",
        "DMCUB_PWAIT_MODE_STATUS",
        "DMDATA_VM_UNDERFLOW_STATUS",
        "FMT_DOUBLE_BUFFER_REG_UPDATE_PENDING",
        "HUBPREQ_MASTER_UPDATE_LOCK_STATUS",
        "HUBP_NO_OUTSTANDING_REQ",
        "HUBP_SEG_ALLOC_ERR_STATUS",
        "HUBP_TIMEOUT_STATUS",
        "HUBP_UNDERFLOW_STATUS",
        "HUBP_XRQ_NO_OUTSTANDING_REQ",
        "LB_G1_MEM_PWR_STATE",
        "LB_G2_MEM_PWR_STATE",
        "LB_G3_MEM_PWR_STATE",
        "LB_G4_MEM_PWR_STATE",
        "LB_G5_MEM_PWR_STATE",
        "LB_G6_MEM_PWR_STATE",
        "LUT_MEM_PWR_STATE",
        "OPPBUF_DOUBLE_BUFFER_PENDING",
        "OPTC_DOUBLE_BUFFER_PENDING",
        "OPTC_UNDERFLOW_INT_STATUS",
        "OPTC_UNDERFLOW_OCCURRED_CURRENT",
        "OPTC_UNDERFLOW_OCCURRED_STATUS",
        "OTG_ONE_SHOT_CRC0_PENDING",
        "OTG_ONE_SHOT_CRC1_PENDING",
        "OTG_ONE_SHOT_CRC2_PENDING",
        "OTG_ONE_SHOT_CRC3_PENDING",
        "SURFACE_FLIP_PENDING",
        "UPDATE_LOCK_STATUS",
        "VREADY_EVENT_OCCURRED",
        "VREADY_INT_STATUS",
        "VSTARTUP_EVENT_OCCURRED",
        "VSTARTUP_INT_STATUS",
        "VUPDATE_EVENT_OCCURRED",
        "VUPDATE_INT_STATUS",
        "VUPDATE_NO_LOCK_EVENT_OCCURRED",
        "VUPDATE_NO_LOCK_INT_STATUS",
        "VUPDATE_NO_LOCK_STATUS",
    ];
    assert_eq!(names, want);
}
