//! GX-10: a fixture that is not what the generator writes is refused when it is parsed (`linuxseq::parse`): a
//! foreign header, a line of no known kind, a wait whose answers do not fit its condition, its counts or Linux's
//! bound, a sleep that follows no write or is longer than the silence the trace shows. And the fixtures the
//! kernel carries hold what the trace shows: the counts and values Linux's code says they must.

use crate::gpucore::psp::{Bound, Cond};
use crate::rdna4::linuxseq::compare::{Compare, Kind};
use crate::rdna4::linuxseq::fold::Did;
use crate::rdna4::linuxseq::{self, SECTIONS, Section, What, parse, section};
use std::string::String;
use std::vec::Vec;

#[test]
fn a_fixture_that_is_not_the_generators_is_refused() {
    let head = "# linuxseq x: t sha256 0, init lines 1-2, by g";
    let ok = std::format!("{head}\n# wait A: ...\n1 R A 0x10 0x00000001 x3\n2 WAIT A 0x10 changed from=0x00000001 last=0x00000002 reads=3 us=5 seen=0x00000001x2,0x00000002x1\n").replace("last=", "max=7 last=");
    let s = parse("x", &ok).unwrap();
    assert_eq!((s.lines.len(), s.lines[0].at, s.lines[1].init), (2, 3, 2));
    // Three reads in a row are three reads of the stage.
    let reads = [Did::Read { dword: 0x10, value: Some(1) }; 3];
    let mut c = Compare::new(&s, &[]).unwrap();
    c.feed(&reads[..2]).unwrap();
    assert_eq!(c.stop().unwrap_err().kind, Kind::Missing);
    let mut c = Compare::new(&s, &[]).unwrap();
    c.feed(&reads).unwrap();
    c.feed(&[Did::Wait { dword: 0x10, cond: Cond::Changed { from: 1 }, bound: Bound::Reads(7), reads: 9, last: Some(2) }]).unwrap();
    c.finish().unwrap();
    for bad in [
        "# linuxseq y: t\n1 R A 0x10 0x1\n",
        "1 R A 0x10 0x1\n",
        &std::format!("{head}\n"),
        &std::format!("{head}\n1 X A 0x10 0x1\n"),
        &std::format!("{head}\n1 R A 0x10 0x1 x1\n"),
        &std::format!("{head}\none R A 0x10 0x1\n"),
        // A wait whose answers do not fit it: the condition met before the end, never met, a wrong count, more
        // reads than Linux makes before it gives up, no bound at all.
        &std::format!("{head}\n1 WAIT A 0x10 mask=0x1 is=0x1 max=9 last=0x1 reads=2 us=0 seen=0x1x2\n"),
        &std::format!("{head}\n1 WAIT A 0x10 mask=0x1 is=0x1 max=9 last=0x0 reads=2 us=0 seen=0x0x2\n"),
        &std::format!("{head}\n1 WAIT A 0x10 mask=0x1 is=0x1 max=9 last=0x1 reads=5 us=0 seen=0x0x2,0x1x1\n"),
        &std::format!("{head}\n1 WAIT A 0x10 mask=0x1 is=0x1 max=2 last=0x1 reads=3 us=0 seen=0x0x2,0x1x1\n"),
        &std::format!("{head}\n1 WAIT A 0x10 mask=0x1 is=0x1 last=0x1 reads=3 us=0 seen=0x0x2,0x1x1\n"),
        // A sleep that follows no write, or one longer than the silence the trace shows.
        &std::format!("{head}\n1 R A 0x10 0x1\n1 SLEEP us=20000 gap=20007\n"),
        &std::format!("{head}\n1 W A 0x10 0x1\n1 SLEEP us=20000 gap=19000\n"),
    ] {
        assert!(parse("x", bad).is_err(), "{bad}");
    }
    let _: String = parse("x", "").unwrap_err();
    let slept = std::format!("{head}\n1 W A 0x10 0x1\n1 SLEEP us=20000 gap=20007\n");
    let s = parse("x", &slept).unwrap();
    assert_eq!(s.lines[1].what, What::Sleep { us: 20000, gap: 20007 });
}

#[test]
fn the_kernels_sections_hold_what_the_trace_shows() {
    let writes = |s: &Section, n: &str| -> Vec<u32> {
        s.lines.iter().filter_map(|l| match l.what { What::Write { name, value, .. } if n.is_empty() || name == n => Some(value), _ => None }).collect()
    };
    let hdp = |s: &Section| s.lines.iter().filter(|l| matches!(l.what, What::Hdp { .. })).count();
    let nbif = section("nbif").unwrap();
    assert_eq!(writes(&nbif, "").len(), 5, "STRAP2, the two remap registers, DF, the doorbell aperture");
    let mmhub = section("mmhub").unwrap();
    assert_eq!((writes(&mmhub, "").len(), hdp(&mmhub)), (144, 2));
    assert_eq!(writes(&section("ih").unwrap(), "").len(), 32);
    // sw-init is carried for its 28 TLB flushes after a GART bind (two HDP flushes each).
    let sw = section("sw-init").unwrap();
    assert_eq!((writes(&sw, "MMVM_INVALIDATE_ENG17_REQ"), hdp(&sw)), (std::vec![0x00f8_0001; 28], 56));
    let boot = section("psp-boot").unwrap();
    // psp_hw_start's order (amdgpu_psp.c:2952-3048): kdb, spl, sys, soc, intf, dbg, ras, ipkeymgr, sos.
    assert_eq!(writes(&boot, "MPASP_SMN_C2PMSG_35"), [0x8_0000, 0x1000_0000, 0x1_0000, 0xb_0000, 0xd_0000, 0xc_0000, 0xe_0000, 0xf_0000, 0x2_0000]);
    assert_eq!(writes(&boot, "MPASP_SMN_C2PMSG_36"), [3; 9]);
    assert_eq!(hdp(&boot), 0, "Linux's firmware buffer is in the GART: no flush");
    let ring = section("psp-ring").unwrap();
    let want: Vec<u32> = (1..=31).map(|i| i * 0x10).collect();
    assert_eq!(writes(&ring, "MPASP_SMN_C2PMSG_67"), want, "31 ring commands");
    assert_eq!(hdp(&ring), 31, "one flush per frame (amdgpu_psp.c:3961)");
    assert_eq!(ring.lines[0].init, 752);
    // One flush pair in every section: the hole soc24's remap makes, then the memory size.
    assert_eq!(linuxseq::hdp_pair(), Some((0x1fc00, 0xde3)));
    for (n, _) in SECTIONS {
        let s = section(n).unwrap();
        assert!(s.lines.iter().all(|l| !matches!(l.what, What::Hdp { flush, mem, .. } if (flush, mem) != (0x1fc00, 0xde3))), "{n}");
        assert!(s.head.contains("plan/hw/gaming-pc-gpu-2026-11-18/init.txt.zst sha256 ") && s.head.contains("host/gaming-pc/gx-trace-seq.py"), "{}", s.head);
    }
    assert_eq!(linuxseq::name_of(0x16083), Some("MPASP_SMN_C2PMSG_67"));
    // Wave 43: the power controller's start, 125 messages, each with Linux's wait before it (but the first) and
    // after it, and the thermal alert's four registers before the last.
    let smu = section("smu").unwrap();
    assert_eq!((smu.lines[0].init, smu.lines.len()), (893, 741));
    assert_eq!(writes(&smu, "MP1_SMN_C2PMSG_66").len(), 125);
    let waits = smu.lines.iter().filter(|l| matches!(l.what, What::Wait { name: "MP1_SMN_C2PMSG_90", cond: crate::gpucore::psp::Cond::Changed { from: 0 }, max: 2_000_000, .. })).count();
    assert_eq!(waits, 125 + 124);
    assert!(section("display").is_err(), "later waves' sections are not in the kernel");
}
