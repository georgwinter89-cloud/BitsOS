//! SN-5: the display side of HDMI audio against a register image of the Mini-PC (saved-5.json, 04.10.2026),
//! the port of V1's `BitsOS-Plattform/dpaudio-test.mjs` (sections 1-6). The image behaves as the hardware: the
//! endpoint registers sit behind the index (0xE118) and data (0xE11C) registers, a per-register keep mask models
//! read-only and self-clearing bits. A model of Linux's sequence written here field by field, apart from the
//! engine's tables (the JS test's `STAGES`), says what `on` must leave; every image is compared whole.

use std::vec::Vec;
use std::{format, vec};

use crate::dpaudio::{self, EP_DATA, EP_INDEX, Loc, Off, OffError, On, OnError, RegError, Regs, State};
use crate::dpaudio_fake::{Image, firmware};
use crate::eld::{AzConfig, Sink};

const ALL: u32 = !0;

/// An EDID that says what V1's hard-coded sink said: PNP "IVM", product 0, name "iiyama", LPCM 2 channels
/// 32/44.1/48 kHz 16 bit, speakers FL/FR.
fn iiyama_edid() -> Vec<u8> {
    let mut e = vec![0u8; 256];
    e[..8].copy_from_slice(&[0, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0]);
    e[8] = 0x26;
    e[9] = 0xCD;
    e[18] = 1;
    e[19] = 3;
    let name = [0, 0, 0, 0xFC, 0, b'i', b'i', b'y', b'a', b'm', b'a', 0x0A, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20];
    e[72..90].copy_from_slice(&name);
    e[126] = 1;
    let cs = e[..127].iter().fold(0u8, |a, &b| a.wrapping_add(b));
    e[127] = 0u8.wrapping_sub(cs);
    let x = &mut e[128..];
    x[..4].copy_from_slice(&[0x02, 0x03, 12, 0]);
    x[4..12].copy_from_slice(&[0x23, 0x09, 0x07, 0x01, 0x83, 0x01, 0x00, 0x00]);
    let cs = x[..127].iter().fold(0u8, |a, &b| a.wrapping_add(b));
    x[127] = 0u8.wrapping_sub(cs);
    e
}

fn iiyama() -> AzConfig {
    AzConfig::from_sink(&Sink::from_edid(&iiyama_edid()).expect("edid"))
}

/// Linux's DP audio sequence (DP-AUDIO.md section 4) as fields: (where, mask, value) per stage, with V1's
/// measured sink.
fn stages() -> Vec<Vec<(Loc, u32, u32)>> {
    let (b, e) = (Loc::Bar, Loc::Ep);
    let name = *b"iiyama\0\0";
    let mut s3 = vec![(e(0x54), 1, 1), (e(0x25), 0x7F | 1 << 16 | 1 << 17 | 1 << 18 | 3 << 24, 0x01 | 1 << 17), (e(0x28), ALL, 0x0701_0701)];
    s3.extend([0x29, 0x2A, 0x2B, 0x2C, 0x2D, 0x2E, 0x2F, 0x31, 0x32, 0x33, 0x35].map(|ix| (e(ix), ALL, 0)));
    s3.extend([
        (e(0x38), 1, 1),
        (e(0x37), 0xFF, 0),
        (e(0x37), 0xFF00, 0),
        (e(0x3A), ALL, 0x0000_CD26),
        (e(0x3B), ALL, 7),
        (e(0x3C), ALL, 0),
        (e(0x3D), ALL, 0),
        (e(0x3E), ALL, u32::from_le_bytes([name[0], name[1], name[2], name[3]])),
        (e(0x3F), ALL, u32::from_le_bytes([name[4], name[5], name[6], name[7]])),
        (e(0x40), ALL, 0),
        (e(0x41), ALL, 0),
        (e(0x42), ALL, 0),
        (e(0x54), 1, 0),
    ]);
    vec![
        vec![(b(0x5AC), 0x70, 0x10), (b(0x5BC), ALL, 6_000_000), (b(0x5B8), ALL, 240_000), (b(0x5AC), 1 << 20, 1 << 20)],
        vec![(b(0x15514), ALL, 0), (b(0x154D4), 0xFF00, 0x300)],
        s3,
        vec![(e(0x54), 1 << 31 | 1, 1 << 31 | 1), (e(0x54), 1, 0)],
        vec![
            (b(0x155C0), 1, 1),
            (b(0x157C4), ALL, 0x8000),
            (b(0x157D4), ALL, 1),
            (b(0x1551C), 0x31, 1),
            (b(0x15508), 1 << 26, 1 << 26),
            (b(0x154D4), 1 << 28 | 1, 0),
            (b(0x1550C), 0x80, 0x80),
            (b(0x154E0), 3 << 28, 0),
            (b(0x157AC), 0x10, 0x10),
            (b(0x157AC), 0x1100, 0x1100),
            (b(0x157AC), 1, 1),
        ],
        vec![(b(0x1551C), 0x31, 1), (b(0x15508), 1, 1)],
    ]
}

/// The image as the stages leave it, and the writes they make.
fn model_with(img: &Image, which: &[usize]) -> (Image, Vec<(Loc, u32)>) {
    let mut out = img.clone();
    let mut writes = Vec::new();
    let st = stages();
    for &s in which {
        for &(l, mask, v) in &st[s - 1] {
            let new = (out.g(l) & !mask) | (v & mask);
            out.s(l, new);
            writes.push((l, new));
        }
    }
    out.log.clear();
    (out, writes)
}

fn model(img: &Image) -> Image {
    model_with(img, &[1, 2, 3, 4, 5, 6]).0
}

fn fresh(img: &Image) -> Image {
    let mut i = img.clone();
    i.log.clear();
    i.accesses = 0;
    i
}

#[test]
fn fixture_and_counts() {
    let f = firmware();
    assert_eq!(f.g(Loc::Ep(0x56)), 0x1856_00F0);
    assert_eq!(f.g(Loc::Bar(0x15508)), 0x0400_0800);
    assert_eq!(dpaudio::on_writes(&iiyama()).len(), dpaudio::ON_WRITES);
    assert_eq!(dpaudio::UNDO.len(), dpaudio::OFF_WRITES);
    let per_stage: Vec<usize> = (1..=6).map(|s| dpaudio::on_writes(&iiyama()).iter().filter(|w| w.stage == s).count()).collect();
    assert_eq!(per_stage, [4, 2, 27, 2, 11, 2], "usb.md 7.4: 48 writes in 6 stages 4/2/27/2/11/2");
}

#[test]
fn no_engine_nothing_written() {
    let mut i = Image { attached: false, ..Default::default() };
    assert_eq!(dpaudio::state(&mut i), State::Absent);
    assert_eq!(dpaudio::on(&mut i, &iiyama()), Err(OnError::NoEngine));
    assert_eq!(dpaudio::off(&mut i), Err(OffError::NoEngine));
    assert_eq!(i.written(), 0);
    assert_eq!(OnError::NoEngine.why(), "HDMI audio: no display engine");
}

#[test]
fn state_only_selects_the_endpoint() {
    let mut i = fresh(&firmware());
    let start = i.clone();
    assert_eq!(dpaudio::state(&mut i), State::Off);
    assert_eq!(i.written(), 0);
    i.same(&start).unwrap();
    let mut on = fresh(&model(&firmware()));
    assert_eq!(dpaudio::state(&mut on), State::On);
    assert_eq!(on.written(), 0);
}

/// A dead BAR answers all ones: gate 1 refuses before any write, and the index stays as found.
#[test]
fn dead_bar_nothing_written() {
    let mut i = fresh(&firmware());
    i.dead = true;
    let start = i.clone();
    match dpaudio::on(&mut i, &iiyama()) {
        Err(OnError::Gate { gate, read }) => assert_eq!((gate.loc, read), (Loc::Bar(0xE318), !0)),
        r => panic!("dead BAR: {r:?}"),
    }
    assert!(matches!(dpaudio::off(&mut i), Err(OffError::NotConfirmed { read: 0xFFFF_FFFF, .. })));
    assert_eq!(dpaudio::state(&mut i), State::Absent);
    assert_eq!(i.written(), 0);
    i.same(&start).unwrap();
}

/// The engine vanishes after n reads, for every n up to the first write: `on` refuses, nothing is written and the
/// endpoint index is as found.
#[test]
fn engine_gone_during_the_gates() {
    let fw = fresh(&firmware());
    let mut n = 0;
    loop {
        let mut i = fw.clone();
        i.reads_left = Some(n);
        let r = dpaudio::on(&mut i, &iiyama());
        i.reads_left = None;
        if i.written() > 0 {
            assert!(n > 13, "a write after only {n} reads: the gates were not all read");
            break;
        }
        // In the gates: "no engine"; at the first write's read: refused before it (nothing to undo).
        assert!(matches!(r, Err(OnError::NoEngine | OnError::Access { nth: 1, .. })), "after {n} reads: {r:?}");
        assert_eq!(i.index(), fw.index(), "after {n} reads");
        i.same(&fw).unwrap();
        n += 1;
    }
}

#[test]
fn measured_state_on_and_off() {
    let fw = firmware();
    let mut i = fresh(&fw);
    assert_eq!(dpaudio::state(&mut i), State::Off);
    let r = dpaudio::on(&mut i, &iiyama()).unwrap();
    assert_eq!(r, On::Written { writes: 48 });
    assert_eq!(r.line(), "HDMI audio: display side on (DIG0, DP SST, 48 writes)");
    let (want, writes) = model_with(&fw, &[1, 2, 3, 4, 5, 6]);
    i.same(&want).unwrap();
    assert_eq!(i.log, writes, "the exact write list (where, value) as the model makes it");
    assert_eq!(i.g(Loc::Bar(EP_INDEX)), 0, "the endpoint index as found");
    assert_eq!(i.unpaired, 0, "every index/data access under the pair lock");
    assert_eq!(dpaudio::state(&mut i), State::On);
    i.log.clear();
    let again = dpaudio::on(&mut i, &iiyama()).unwrap();
    assert_eq!(again, On::AlreadyOn);
    assert_eq!(again.line(), "HDMI audio: display side on already (DIG0, DP SST), nothing written");
    assert_eq!(i.written(), 0);
    let off = dpaudio::off(&mut i).unwrap();
    assert_eq!(off, Off { writes: 41, mismatched: 0 });
    assert_eq!(off.line(), "HDMI audio: display side off (41 writes)");
    i.same(&fw).unwrap();
    let undo: Vec<Loc> = i.log.iter().map(|w| w.0).collect();
    assert_eq!(undo, dpaudio::UNDO.iter().map(|w| w.loc).collect::<Vec<_>>(), "the undo's order");
    assert_eq!(*i.log.last().unwrap(), (Loc::Bar(EP_INDEX), 0), "last the endpoint index as saved");
    assert_eq!(dpaudio::state(&mut i), State::Off);
}

/// Every register the kit writes holding something else before (a stick program, a kernel before).
fn junk() -> Image {
    let mut i = firmware();
    for o in [0x15508u32, 0x15514, 0x5AC, 0x5B8, 0x5BC, 0x154D4, 0x154E0, 0x1550C, 0x1551C, 0x155C0, 0x157AC, 0x157C4, 0x157D4, EP_INDEX] {
        i.s(Loc::Bar(o), 0xA5A5_0000 | (o & 0xFFFF));
    }
    for ix in [0x25u32, 0x28, 0x29, 0x2A, 0x2B, 0x2C, 0x2D, 0x2E, 0x2F, 0x31, 0x32, 0x33, 0x35, 0x37, 0x38, 0x3A, 0x3B, 0x3C, 0x3D, 0x3E, 0x3F, 0x40, 0x41, 0x42, 0x54]
    {
        i.s(Loc::Ep(ix), 0x5A5A_0000 | ix);
    }
    i
}

#[test]
fn from_other_values() {
    let mut j = junk();
    let v = j.g(Loc::Bar(0x157AC)) & !0x1FF0_0000;
    j.s(Loc::Bar(0x157AC), v); // gate 14 lets it through
    // The index register holds junk: the endpoint is reached all the same and the junk put back.
    let mut i = fresh(&j);
    assert_eq!(dpaudio::on(&mut i, &iiyama()), Ok(On::Written { writes: 48 }));
    i.same(&model(&j)).unwrap();
    let mut i = fresh(&junk());
    assert_eq!(dpaudio::off(&mut i), Ok(Off { writes: 41, mismatched: 0 }));
    i.same(&firmware()).unwrap();
}

/// What makes each gate fail alone (dpaudio-test.mjs GATES).
fn breakers() -> Vec<(Loc, u32)> {
    vec![
        (Loc::Bar(0xE318), 0x1002AA02),
        (Loc::Bar(0x508), 600_000_001),
        (Loc::Bar(0x504), 0),
        (Loc::Bar(0x500), 0x1010),
        (Loc::Bar(0x155C8), 0x100),
        (Loc::Bar(0x155C4), 0x30100),
        (Loc::Bar(0x1552C), 0x1000001),
        (Loc::Bar(0x15720), 0x100),
        (Loc::Bar(0x15730), 0x10200),
        (Loc::Bar(0x14004), 0x11200),
        (Loc::Bar(0xE32C), 0x200F0),
        (Loc::Bar(0xE334), 0x8000_0009),
        (Loc::Ep(0x56), 0x5856_00F0),
        (Loc::Bar(0x157AC), 0x10_0000),
    ]
}

#[test]
fn each_gate_alone_and_order() {
    for (k, &(l, v)) in breakers().iter().enumerate() {
        let mut start = firmware();
        start.s(l, v);
        let mut i = fresh(&start);
        let r = dpaudio::on(&mut i, &iiyama());
        let g = dpaudio::GATES[k];
        assert_eq!(r, Err(OnError::Gate { gate: g, read: v }), "gate {}", g.name);
        let err = r.unwrap_err();
        assert_eq!(err.why(), format!("HDMI audio: display side not ready: {}", g.name));
        assert_eq!(err.line(), format!("HDMI audio: display side not ready: {} (read {v:X}), nothing written", g.name));
        assert_eq!(i.written(), 0, "{}: nothing written", g.name);
        i.same(&start).unwrap();
    }
    let mut named = Vec::new();
    for k in 0..14 {
        let mut i = firmware();
        for &(l, v) in &breakers()[k..] {
            i.s(l, v);
        }
        match dpaudio::on(&mut fresh(&i), &iiyama()) {
            Err(OnError::Gate { gate, .. }) => named.push(gate.name),
            r => panic!("{r:?}"),
        }
    }
    assert_eq!(named, dpaudio::GATES.iter().map(|g| g.name).collect::<Vec<_>>(), "with gates n to 14 failing, gate n is named");
}

#[test]
fn a_register_that_does_not_read_back() {
    let mut start = firmware();
    start.keep.insert(Loc::Ep(0x25), ALL);
    let mut i = fresh(&start);
    let err = dpaudio::on(&mut i, &iiyama()).unwrap_err();
    assert_eq!(err.why(), "HDMI audio: display side refused: endpoint CHANNEL_SPEAKER");
    assert_eq!(err.line(), "HDMI audio: display side refused: endpoint CHANNEL_SPEAKER (stage 3, write 8 at 25: wrote 20001, read 0)");
    assert_eq!(err.undo().unwrap().map(|o| o.line()), Ok("HDMI audio: display side off (41 writes)".into()));
    i.same(&start).unwrap();

    let mut start = firmware();
    start.keep.insert(Loc::Bar(0x157AC), ALL);
    let mut i = fresh(&start);
    let err = dpaudio::on(&mut i, &iiyama()).unwrap_err();
    assert_eq!(err.why(), "HDMI audio: display side refused: DP_SEC_CNTL");
    assert_eq!(err.line(), "HDMI audio: display side refused: DP_SEC_CNTL (stage 5, write 44 at 157AC: wrote 10, read 0)");
    assert_eq!(err.undo().unwrap().map(|o| o.line()), Ok("HDMI audio: display side off (41 writes)".into()));
    i.same(&start).unwrap();

    // AFMT_AUDIO_INFO_UPDATE, a strobe, does not stay set: no surprise (its check leaves it out).
    let mut start = firmware();
    start.keep.insert(Loc::Bar(0x1550C), 0x80);
    let mut i = fresh(&start);
    assert_eq!(dpaudio::on(&mut i, &iiyama()), Ok(On::Written { writes: 48 }));
    i.same(&model(&start)).unwrap();
    assert_eq!(dpaudio::off(&mut i), Ok(Off { writes: 41, mismatched: 0 }));
    i.same(&start).unwrap();
}

#[test]
fn more_states() {
    let fw = firmware();
    let on = model(&fw);
    let mut i = fresh(&on);
    assert_eq!(dpaudio::on(&mut i, &iiyama()), Ok(On::AlreadyOn));
    assert_eq!(i.written(), 0);
    i.same(&on).unwrap();

    let half = model_with(&fw, &[1, 2, 3]).0;
    let mut i = fresh(&half);
    assert_eq!(dpaudio::state(&mut i), State::Off, "found half on (stages 1-3)");
    assert_eq!(dpaudio::on(&mut i, &iiyama()), Ok(On::Written { writes: 48 }));
    i.same(&on).unwrap();

    let no_clock = model_with(&fw, &[2, 3, 4, 5, 6]).0;
    let mut i = fresh(&no_clock);
    assert_eq!(dpaudio::state(&mut i), State::Off, "on but for the audio clock (DTO_SEL not 1)");
    assert_eq!(dpaudio::on(&mut i, &iiyama()), Ok(On::Written { writes: 48 }));
    i.same(&on).unwrap();

    let mut at5a = firmware();
    at5a.s(Loc::Bar(EP_INDEX), 0x5A);
    let mut i = fresh(&at5a);
    dpaudio::on(&mut i, &iiyama()).unwrap();
    assert_eq!(i.g(Loc::Bar(EP_INDEX)), 0x5A, "the index found at 0x5A is put back after every access");
    i.same(&model(&at5a)).unwrap();
    let mut gated = at5a.clone();
    gated.s(Loc::Bar(0x15720), 0x100);
    let mut i = fresh(&gated);
    assert!(matches!(dpaudio::on(&mut i, &iiyama()), Err(OnError::Gate { .. })));
    i.same(&gated).unwrap();
}

#[test]
fn off_cases() {
    // DP_SEC_CNTL stuck: counted, every other register restored.
    let mut img = model(&firmware());
    img.keep.insert(Loc::Bar(0x157AC), ALL);
    let mut i = fresh(&img);
    let off = dpaudio::off(&mut i).unwrap();
    assert_eq!(off, Off { writes: 41, mismatched: 1 });
    assert_eq!(off.line(), "HDMI audio: display side off (41 writes), 1 did not read back");
    let mut want = firmware();
    want.s(Loc::Bar(0x157AC), 0x1111);
    i.same(&want).unwrap();

    // The link changed since (HDMI mode, not trained): undone all the same (it needs only gates 1 and 2).
    let mut changed = firmware();
    changed.s(Loc::Bar(0x155C4), 0x30100);
    changed.s(Loc::Bar(0x15720), 0x100);
    let mut i = fresh(&model(&changed));
    assert_eq!(dpaudio::off(&mut i), Ok(Off { writes: 41, mismatched: 0 }));
    i.same(&changed).unwrap();

    for (at, v, gate) in [(0xE318, 0x1002AA02u32, "codec_id 1002AA01: segment 2 base"), (0x508, 599_999_999, "DP_DTO0_MODULO 600 MHz: segment 1 base")] {
        let mut img = model(&firmware());
        img.s(Loc::Bar(at), v);
        let mut i = fresh(&img);
        let err = dpaudio::off(&mut i).unwrap_err();
        assert_eq!(err.line(), format!("HDMI audio: display side not confirmed, nothing undone: {gate} (read {v:X})"));
        assert_eq!(i.written(), 0);
        i.same(&img).unwrap();
        assert_eq!(dpaudio::state(&mut i), State::Absent);
    }
}

#[test]
fn a_write_refused_half_way_is_undone() {
    // A register access fails after the 10th write: stopped, undone as far as the owner lets it.
    struct Flaky {
        img: Image,
        left: u32,
    }
    impl Regs for Flaky {
        fn read(&mut self, b: u32) -> Result<u32, RegError> {
            self.img.read(b)
        }
        fn write(&mut self, b: u32, v: u32) -> Result<(), RegError> {
            if self.left == 0 {
                return Err(RegError::OutOfRange);
            }
            self.left -= 1;
            self.img.write(b, v)
        }
    }
    let mut f = Flaky { img: fresh(&firmware()), left: 10 };
    let err = dpaudio::on(&mut f, &iiyama()).unwrap_err();
    assert!(matches!(err, OnError::Access { error: RegError::OutOfRange, .. }), "{err:?}");
    assert!(matches!(err.undo(), Some(Err(OffError::Access { .. }))));
}

#[test]
fn names_match_the_generated_register_map() {
    use dcn314_regs::{at_byte, lookup_indirect};
    let bar = |o: u32| at_byte(o).map(|r| r.name).collect::<Vec<_>>();
    let want = [
        (0xE318, "AZALIA_F0_CODEC_ROOT_PARAMETER_VENDOR_AND_DEVICE_ID"),
        (0x508, "DP_DTO0_MODULO"),
        (0x504, "DP_DTO0_PHASE"),
        (0x500, "OTG0_PIXEL_RATE_CNTL"),
        (0x155C8, "DIG0_DIG_BE_EN_CNTL"),
        (0x155C4, "DIG0_DIG_BE_CNTL"),
        (0x1552C, "DIG0_DIG_FE_CNTL"),
        (0x15720, "DP0_DP_LINK_CNTL"),
        (0x15730, "DP0_DP_VID_STREAM_CNTL"),
        (0x14004, "OTG0_OTG_CONTROL"),
        (0xE32C, "AZALIA_F0_CODEC_FUNCTION_PARAMETER_SUPPORTED_SIZE_RATES"),
        (0xE334, "AZALIA_F0_CODEC_FUNCTION_PARAMETER_POWER_STATES"),
        (0x157AC, "DP0_DP_SEC_CNTL"),
        (0x5AC, "DCCG_AUDIO_DTO_SOURCE"),
        (0x5B8, "DCCG_AUDIO_DTO1_PHASE"),
        (0x5BC, "DCCG_AUDIO_DTO1_MODULE"),
        (0x15514, "AFMT0_AFMT_AUDIO_SRC_CONTROL"),
        (0x154D4, "AFMT0_AFMT_AUDIO_PACKET_CONTROL2"),
        (0x155C0, "DIG0_AFMT_CNTL"),
        (0x157C4, "DP0_DP_SEC_AUD_N"),
        (0x157D4, "DP0_DP_SEC_TIMESTAMP"),
        (0x1551C, "AFMT0_AFMT_MEM_PWR"),
        (0x15508, "AFMT0_AFMT_AUDIO_PACKET_CONTROL"),
        (0x1550C, "AFMT0_AFMT_INFOFRAME_CONTROL0"),
        (0x154E0, "AFMT0_AFMT_60958_0"),
        (EP_INDEX, "AZF0ENDPOINT0_AZALIA_F0_CODEC_ENDPOINT_INDEX"),
        (EP_DATA, "AZF0ENDPOINT0_AZALIA_F0_CODEC_ENDPOINT_DATA"),
    ];
    let mut every: Vec<Loc> = dpaudio::GATES.iter().map(|g| g.loc).collect();
    every.extend(dpaudio::on_writes(&iiyama()).iter().map(|w| w.loc));
    every.extend(dpaudio::UNDO.iter().map(|w| w.loc));
    for l in &every {
        if let Loc::Bar(o) = l {
            let n = want.iter().find(|w| w.0 == *o).unwrap_or_else(|| panic!("{o:X} has no expected name")).1;
            assert!(bar(*o).contains(&n), "{o:X}: {:?}, want {n}", bar(*o));
        }
    }
    let ep = |name: &str| lookup_indirect(&format!("AZF0ENDPOINT0_AZALIA_F0_CODEC_PIN_CONTROL_{name}")).map(|i| i.index);
    assert_eq!(ep("HOT_PLUG_CONTROL"), Some(0x54));
    assert_eq!(ep("CHANNEL_SPEAKER"), Some(0x25));
    assert_eq!(ep("RESPONSE_LIPSYNC"), Some(0x37));
    assert_eq!(ep("RESPONSE_HBR"), Some(0x38));
    for k in 0..14u32 {
        assert_eq!(ep(&format!("AUDIO_DESCRIPTOR{k}")), Some(0x28 + k));
    }
    for k in 0..9u32 {
        assert_eq!(ep(&format!("SINK_INFO{k}")), Some(0x3A + k));
    }
    let port = lookup_indirect("AZF0ENDPOINT0_AZALIA_F0_CODEC_PIN_CONTROL_RESPONSE_CONFIGURATION_DEFAULT").map(|i| i.index);
    assert_eq!(port, Some(0x56), "gate 13 reads the pin's configuration default (PORT_CONNECTIVITY)");
    // The fields stage 3 sets, at the bits the tables use.
    let field = |reg: &str, f: &str| {
        let i = lookup_indirect(&format!("AZF0ENDPOINT0_AZALIA_F0_CODEC_PIN_CONTROL_{reg}")).unwrap();
        i.fields.iter().find(|x| x.name == f).map(|x| x.mask).unwrap_or_else(|| panic!("{reg}.{f}"))
    };
    assert_eq!(field("CHANNEL_SPEAKER", "SPEAKER_ALLOCATION"), 0x7F);
    assert_eq!(field("CHANNEL_SPEAKER", "DP_CONNECTION"), 1 << 17);
    assert_eq!(field("CHANNEL_SPEAKER", "HDMI_CONNECTION"), 1 << 16);
    assert_eq!(field("AUDIO_DESCRIPTOR0", "MAX_CHANNELS"), 0x7);
    assert_eq!(field("AUDIO_DESCRIPTOR0", "SUPPORTED_FREQUENCIES"), 0xFF00);
    assert_eq!(field("AUDIO_DESCRIPTOR0", "DESCRIPTOR_BYTE_2"), 0xFF_0000);
    assert_eq!(field("AUDIO_DESCRIPTOR0", "SUPPORTED_FREQUENCIES_STEREO"), 0xFF00_0000);
    assert_eq!(field("SINK_INFO0", "MANUFACTURER_ID"), 0xFFFF);
    assert_eq!(field("SINK_INFO0", "PRODUCT_ID"), 0xFFFF_0000);
    assert_eq!(field("SINK_INFO4", "DESCRIPTION0"), 0xFF);
    assert_eq!(field("RESPONSE_LIPSYNC", "VIDEO_LIPSYNC"), 0xFF);
    assert_eq!(field("RESPONSE_LIPSYNC", "AUDIO_LIPSYNC"), 0xFF00);
    assert_eq!(field("HOT_PLUG_CONTROL", "AUDIO_ENABLED"), 1 << 31);
}

/// SN-11: the boot switch's monitor sense reads the link gates only: each link gate alone names itself, the
/// segment bases alone give Absent, the other gates (clock, AZ, endpoint port, secondary packets) leave it Lit;
/// nothing is written and the endpoint index is not touched.
#[test]
fn sense_reads_only_the_link_gates() {
    use crate::dpaudio::Sense;
    let mut i = fresh(&firmware());
    assert_eq!(dpaudio::sense(&mut i), Sense::Lit);
    assert_eq!(i.written(), 0);
    for (k, &(l, v)) in breakers().iter().enumerate() {
        let mut start = firmware();
        start.s(l, v);
        let mut i = fresh(&start);
        let want = match k {
            0 | 1 => Sense::Absent,
            k if dpaudio::LINK_GATES.contains(&k) => Sense::Dark { gate: dpaudio::GATES[k], read: v },
            _ => Sense::Lit,
        };
        assert_eq!(dpaudio::sense(&mut i), want, "gate {}", dpaudio::GATES[k].name);
        assert_eq!(i.written(), 0, "gate {}: nothing written", dpaudio::GATES[k].name);
        i.same(&start).unwrap();
    }
    let mut i = Image { attached: false, ..Default::default() };
    assert_eq!(dpaudio::sense(&mut i), Sense::Absent);
}
