//! V1 `dcn-test.mjs:454-540` ported: the Mini-PC image attaches, every broken copy is refused with V1's reason.
//! "Nothing written" holds by type: the gates get a read-only `Regs`.

use super::fake::*;
use crate::attach::{self, Attach, Gop, PciFacts, Refusal, Settle, Why};
use crate::layout as l;
use alloc::vec;
use alloc::vec::Vec;

fn run(img: &Image, pci: &PciFacts, gop: Option<Gop>) -> Result<Attach, Refusal> {
    attach::gates(pci, gop, &reader(img))
}

fn with(changes: &[(u32, u32)]) -> Image {
    let mut m = mini_pc();
    for &(b, v) in changes {
        m.insert(b, v);
    }
    m
}

fn or(b: u32, bits: u32) -> (u32, u32) {
    (b, mini_pc().get(&b).copied().unwrap_or(0) | bits)
}

#[test]
fn mini_pc_attaches() {
    let a = run(&mini_pc(), &pci(), Some(gop())).unwrap();
    assert_eq!((a.a0, a.a1, a.bytes, a.a0_offset), (GPU, GPU1, BYTES, 0));
    assert_eq!((a.memsize_mb, a.aper_mb, a.fb_first, a.fb_last), (8192, 256, GPU, 0x81_FFFF_FFFF));
    assert_eq!(a.pipes, vec![attach::Pipe { hubp: 0, otg: 0, flip_type: 0, address: GPU }]);
    assert!(a.dmcub_running);
    assert_eq!(a.windows, vec![attach::Window { name: "REGION3_CW0", first: 0x81_FFE0_0000, last: 0x81_FFEF_FFFF }]);
    assert_eq!(a.settle, Settle::OnA0);
    assert_eq!(a.reach, 256 * MB);
}

#[test]
fn every_register_gate_refuses_with_its_reason() {
    let p = |r| l::hubp(0, r);
    let cw = |n: usize, first: u64, span: u32| {
        let w = &l::DMCUB_WINDOWS[n];
        let mut v = vec![(w.top, l::WINDOW_ON | span), (w.offset, first as u32), (w.offset_high, (first >> 32) as u32)];
        if let Some(b) = w.base {
            v.push((b, 0));
        }
        v
    };
    let region4 = |first: u64, top: u32| {
        let w = l::DMCUB_WINDOWS.iter().find(|w| w.name == "REGION4").unwrap();
        vec![(w.top, l::WINDOW_ON | top), (w.offset, first as u32), (w.offset_high, (first >> 32) as u32)]
    };
    let cases: Vec<(&str, Vec<(u32, u32)>, Why)> = vec![
        ("MEMSIZE 0", vec![(l::MEMSIZE, 0)], Why::MemSize),
        ("MEMSIZE over 64 GB", vec![(l::MEMSIZE, 0x10001)], Why::MemSize),
        ("FB_TOP below FB_BASE", vec![(l::FB_TOP, 0x7FFF)], Why::MemSize),
        ("MEMSIZE 34: A1 ends at 35 MB", vec![(l::MEMSIZE, 34)], Why::Past),
        ("FB_TOP 32 MB on", vec![(l::FB_TOP, 0x8001)], Why::Past),
        ("APER_SIZE 35 MB: A1's pages end at 36", vec![(l::APER_SIZE, 35)], Why::Aper),
        ("pipe 0 blanked", vec![or(p(l::HUBP_CNTL), 1)], Why::NoPipe),
        ("pipe 0 shows something else", vec![(p(l::HUBP_ADDR), 0x100_0000)], Why::NoPipe),
        ("pipe 0's second power domain gated", vec![(l::pg_status(1), 0x8000_0000)], Why::NoPipe),
        ("pipe 0 timed by OTG 5", vec![or(p(l::HUBP_CNTL), 0x50)], Why::NoPipe),
        ("OTG0 off", vec![(l::otg(0, l::OTG_CONTROL), 0x11200)], Why::OtgOff),
        ("SURFACE_UPDATE_LOCK", vec![or(p(l::HUBP_FLIP), 1)], Why::Held),
        ("VUPDATE_SKIP_NUM 1", vec![or(p(l::HUBP_FLIP), 0x10)], Why::Held),
        ("master update lock status", vec![or(p(l::HUBP_FLIP), 0x200)], Why::Held),
        ("swap lock group (GSL)", vec![or(p(l::HUBP_FLIP2), 0x100)], Why::Held),
        ("triple buffering", vec![or(p(l::HUBP_FLIP2), 0x400)], Why::Held),
        ("OTG_MASTER_UPDATE_LOCK", vec![(l::otg(0, l::OTG_LOCK), 1)], Why::Held),
        ("the OTG's update lock status", vec![(l::otg(0, l::OTG_LOCK), 0x100)], Why::Held),
        ("VMID 1 (page tables)", vec![(p(l::HUBP_VMID), 1)], Why::Surf),
        ("TMZ", vec![(p(l::HUBP_SURF), 1)], Why::Surf),
        ("DCC", vec![(p(l::HUBP_SURF), 2)], Why::Surf),
        ("DP DTO without its status bit", vec![(l::dto_cntl(0), 0x1010)], Why::Sign),
        ("DP DTO phase 0", vec![(l::dto_phase(0), 0)], Why::Sign),
        ("DP DTO modulo 599999999", vec![(l::dto_modulo(0), 599_999_999)], Why::Sign),
        ("the DMCUB runs, its cache window 3 at A1 + 1 MB", cw(3, GPU1 + MB, 0xFFFF), Why::Dmcub),
        ("the DMCUB runs, its region 4 ends on A1's first byte", region4(GPU1 - 0x1000, 0x1000), Why::Dmcub),
        ("the DMCUB runs, its cache window 7 holds A1's last byte", cw(7, GPU1 + BYTES - 1, 0), Why::Dmcub),
        ("codec ID wrong (not a DCN 3.1.4 segment 2)", vec![(crate::dcn::GATE_REG, 0x1002_AA02)], Why::Codec),
    ];
    for (what, ch, why) in cases {
        let e = run(&with(&ch), &pci(), Some(gop())).expect_err(what);
        assert_eq!(e.why, why, "{what}: {e}");
    }
}

#[test]
fn just_enough_attaches() {
    let w3 = &l::DMCUB_WINDOWS[3];
    let r4 = l::DMCUB_WINDOWS.iter().find(|w| w.name == "REGION4").unwrap();
    let on_a1 = |top: u32| vec![(w3.top, top), (w3.offset, (GPU1 + MB) as u32), (w3.offset_high, 0x80)];
    let cases: Vec<(&str, Vec<(u32, u32)>)> = vec![
        ("MEMSIZE 35", vec![(l::MEMSIZE, 35)]),
        ("APER_SIZE 36", vec![(l::APER_SIZE, 36)]),
        ("FB_TOP 48 MB on", vec![(l::FB_TOP, 0x8002)]),
        ("the DMCUB off, a window of it on A1", [on_a1(l::WINDOW_ON | 0xFFFF), vec![(l::DMCUB_CNTL, 0)]].concat()),
        ("the DMCUB runs, its window on A1 not enabled", on_a1(0xFFFF)),
        (
            "its region 4 ends right before A1",
            vec![(r4.top, l::WINDOW_ON | 0xFFF), (r4.offset, (GPU1 - 0x1000) as u32), (r4.offset_high, 0x80)],
        ),
        (
            "its cache window 3 begins right after A1",
            vec![(w3.top, l::WINDOW_ON | 0xFFFF), (w3.offset, (GPU1 + BYTES) as u32), (w3.offset_high, 0x80)],
        ),
    ];
    for (what, ch) in cases {
        run(&with(&ch), &pci(), Some(gop())).unwrap_or_else(|e| panic!("{what}: {e}"));
    }
}

#[test]
fn pci_gates_refuse() {
    let mut no_bridge = pci();
    no_bridge.bridges = None;
    let mut decode = pci();
    decode.command = 0x4;
    let mut bar5 = pci();
    bar5.bar5 = None;
    let mut bar5_zero = pci();
    bar5_zero.bar5 = Some((0, 0x8_0000));
    let mut intel = pci();
    intel.vendor = 0x8086;
    let mut narrow = pci();
    narrow.bridges = Some(vec![attach::BridgeWindows { mem: None, prefetch: Some((BAR0, BAR0 + 36 * MB - 2)) }]);
    let mut enough = pci();
    enough.bridges = Some(vec![attach::BridgeWindows { mem: None, prefetch: Some((BAR0, BAR0 + 36 * MB - 1)) }]);
    let img = mini_pc();
    for (what, f, g, why) in [
        ("a GOP framebuffer of 0 bytes", pci(), Some(Gop { phys: BAR0, bytes: 0 }), Why::NotGop),
        ("no GOP framebuffer", pci(), None, Why::NotGop),
        ("another vendor", intel, Some(gop()), Why::NotGop),
        ("A0 below BAR0", pci(), Some(Gop { phys: BAR0 - MB, bytes: BYTES }), Why::NotGop),
        ("A0 2^34 above BAR0", pci(), Some(Gop { phys: BAR0 + (1 << 34), bytes: BYTES }), Why::NotGop),
        ("memory decode off", decode, Some(gop()), Why::Decode),
        ("BAR5 not a 32-bit memory BAR", bar5, Some(gop()), Why::Bar5),
        ("BAR5 0", bar5_zero, Some(gop()), Why::Bar5),
        ("no bridge leads to bus C7", no_bridge, Some(gop()), Why::Bridge),
        ("the bridges pass 36 MB less a byte", narrow, Some(gop()), Why::Aper),
        ("a GOP mode of more than 32 MB", pci(), Some(Gop { phys: BAR0, bytes: 32 * MB + 4 }), Why::Mode),
    ] {
        assert_eq!(run(&img, &f, g).expect_err(what).why, why, "{what}");
    }
    run(&img, &enough, Some(gop())).expect("the bridges pass 36 MB");
    let mut bus0 = pci();
    bus0.bus = 0;
    bus0.bridges = None;
    assert_eq!(run(&img, &bus0, Some(gop())).unwrap().reach, u64::MAX, "bus 0: the root complex decodes it all");
}

#[test]
fn a0_16_mb_into_bar0() {
    let img = with(&[(l::hubp(0, l::HUBP_ADDR), 0x100_0000)]);
    let a = run(&img, &pci(), Some(Gop { phys: BAR0 + 16 * MB, bytes: BYTES })).unwrap();
    assert_eq!((a.a0, a.a1), (0x80_0100_0000, 0x80_0300_0000));
}

#[test]
fn settle_is_reported_not_done() {
    let a = run(&with(&[(l::hubp(0, l::HUBP_ADDR), 32 << 20)]), &pci(), Some(gop())).unwrap();
    assert_eq!((a.settle, a.pipes[0].address), (Settle::OnA1, GPU1));
    let a = run(&with(&[or(l::hubp(0, l::HUBP_FLIP), l::FLIP_PENDING)]), &pci(), Some(gop())).unwrap();
    assert_eq!(a.settle, Settle::Pending);
    let a = run(&with(&[or(l::hubp(0, l::HUBP_FLIP), l::FLIP_TYPE)]), &pci(), Some(gop())).unwrap();
    assert_eq!(a.pipes[0].flip_type, 1);
}

#[test]
fn second_pipe_on_the_same_image() {
    let img = with(&[
        (l::hubp(1, l::HUBP_CNTL), 0x300F_1002),
        (l::hubp(1, l::HUBP_ADDR_HI), 0x80),
        (l::hubp(1, l::HUBP_FLIP), 0x410_0000),
    ]);
    let a = run(&img, &pci(), Some(gop())).unwrap();
    assert_eq!(a.pipes.iter().map(|p| (p.hubp, p.otg)).collect::<Vec<_>>(), vec![(0, 0), (1, 0)]);
    // Pipe 1 needs domains 1-3: one gated and it is not read.
    let mut img2 = img.clone();
    img2.insert(l::pg_status(3), 0x4000_0000);
    assert_eq!(run(&img2, &pci(), Some(gop())).unwrap().pipes.len(), 1);
}

#[test]
fn unreadable_register_refuses() {
    let img = mini_pc();
    let r = |b: u32| {
        if b == l::DMCUB_CNTL { None } else { reader(&img)(b) }
    };
    let e = attach::gates(&pci(), Some(gop()), &r).unwrap_err();
    assert_eq!((e.why, e.reg), (Why::Read, Some((l::DMCUB_CNTL, 0))));
}

#[test]
fn refusal_text_names_gate_and_register() {
    let e = run(&with(&[(l::dto_modulo(0), 599_999_999)]), &pci(), Some(gop())).unwrap_err();
    assert_eq!(alloc::format!("{e}"), "gate 7: no DP clock signature for its timing generator (0x00508 = 0x23c345ff)");
}
