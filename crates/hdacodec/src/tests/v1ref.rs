//! The set-up lists against V1 itself: V1's ATI table copied literally, and V1's `path_setup`
//! transliterated from the assembler with raw bit tests. Both are interpreted here independently
//! of `setup.rs`/`ati.rs` and compared with what the crate produces.

use std::string::String;
use std::vec::Vec;

use super::{AA01, ALC269, load};
use crate::dump::Dump;
use crate::model::Codec;
use crate::path::{Output, OutputKind, outputs};
use crate::setup::{Action, Stream, path_setup};
use crate::{Nid, ati};

/// V1 `BitsOS-Plattform/snd/hdmi.asm` lines 187-207 (`ati_verbs`), `SND_FORMAT` = 0x0011
/// (`hda.asm` line 32). Flags: low byte 1 = pin, 2 = converter; 0x100 rev3 only; 0x200 digital
/// converter only; 0x400 output amp at 0 dB (if any); 0x800 or the pin's input index in;
/// 0x1000 HBR off if on.
const V1_ATI_VERBS: [(u32, u32); 21] = [
    (1, 0x77200),
    (0x101, 0x78901),
    (0x401, 0),
    (1, 0x70740),
    (0x801, 0x70100),
    (0x202, 0x70D01),
    (0x402, 0),
    (0x202, 0x72D01),
    (1, 0x77701),
    (1, 0x78511),
    (1, 0x77800),
    (1, 0x78600),
    (1, 0x77900),
    (1, 0x78700),
    (1, 0x77A00),
    (1, 0x78800),
    (1, 0x77100),
    (0x102, 0x770B4),
    (0x1001, 0),
    (2, 0x70610),
    (2, 0x20000 | 0x0011),
];

/// V1 `path.asm` `amp_gain` (lines 199-209): the offset, not above the top step.
fn amp_gain(caps: u32) -> u32 {
    (caps & 0x7F).min((caps >> 8) & 0x7F)
}

fn set(nid: Nid, verb: u32) -> Action {
    Action::Set { nid, verb }
}

/// V1 `hdmi.asm` lines 105-180: the table walk.
fn v1_ati(codec: &Codec, out: &Output) -> Vec<Action> {
    let g = codec.groups.iter().find(|g| g.nid == out.group).unwrap();
    let (pin, cvt) = (out.path[0], out.path[1]);
    let rev3 = codec.revision_id & 0xFF00 >= 0x0300;
    let cvt_caps = g.widget(cvt.nid).unwrap().caps.0;
    let mut list = Vec::new();
    for (flags, verb) in V1_ATI_VERBS {
        if flags & 0x100 != 0 && !rev3 {
            continue;
        }
        if flags & 0x200 != 0 && cvt_caps & (1 << 9) == 0 {
            continue;
        }
        let nid = if flags & 0xFF == 1 { pin.nid } else { cvt.nid };
        let mut verb = verb;
        if flags & 0x400 != 0 {
            let w = g.widget(nid).unwrap();
            if w.caps.0 & 4 == 0 {
                continue;
            }
            verb = amp_gain(w.out_amp.0) | 0x3B000;
        }
        if flags & 0x800 != 0 {
            verb |= pin.input.unwrap() as u32;
        }
        if flags & 0x1000 != 0 {
            list.push(Action::UpdateBits {
                nid,
                get: 0xF7C00,
                set: 0x77C,
                clear: 0x10,
                set_bits: 0,
            });
            continue;
        }
        list.push(set(nid, verb));
    }
    list
}

/// V1 `path.asm` `path_setup` (lines 137-197), with the widget fields as raw bits.
fn v1_path_setup(codec: &Codec, out: &Output) -> Vec<Action> {
    let g = codec.groups.iter().find(|g| g.nid == out.group).unwrap();
    let last = out.path.len() - 1;
    let mut list = Vec::new();
    for (d, hop) in out.path.iter().enumerate() {
        let w = g.widget(hop.nid).unwrap();
        if w.caps.0 & 4 != 0 {
            list.push(set(hop.nid, amp_gain(w.out_amp.0) | 0x3B000));
        }
        if d == last {
            break;
        }
        let i = hop.input.unwrap() as u32;
        let mixer = (w.caps.0 >> 20) & 0xF == 2;
        if !mixer && w.conns.nodes.len() > 1 {
            list.push(set(hop.nid, 0x70100 | i));
        }
        if d != 0 && w.caps.0 & 2 != 0 && i <= 15 {
            list.push(set(hop.nid, amp_gain(w.in_amp.0) | i << 8 | 0x37000));
        }
    }
    let dac = out.converter();
    list.push(set(dac, 0x20011));
    list.push(set(dac, 0x70610));
    let pc = g.widget(out.pin).unwrap().pin_caps.0;
    let mut ctl = 0x70740;
    if out.kind == OutputKind::Headphone && (pc & 8 != 0 || pc == 0) {
        ctl |= 0x80;
    }
    list.push(set(out.pin, ctl));
    if pc & 0x1_0000 != 0 {
        list.push(set(out.pin, 0x70C02));
    }
    list
}

#[test]
fn alc269_path_setup_is_v1s() {
    let (_, codec, outs) = load(ALC269);
    assert_eq!(outs.len(), 2);
    for o in &outs {
        assert_eq!(
            path_setup(&codec, o, &Stream::default()).unwrap(),
            v1_path_setup(&codec, o),
            "pin {:#x}",
            o.pin
        );
    }
}

#[test]
fn aa01_list_is_v1s_table() {
    // The fixture as it is, then variants that switch each conditional row of the table.
    let variants: [&[(&str, &str)]; 4] = [
        &[],
        &[("0 00 F0002 00100700", "0 00 F0002 00100200")], // before rev3
        &[("0 02 F0009 00000221", "0 02 F0009 00000021")], // converter not digital
        &[
            ("0 02 F0009 00000221", "0 02 F0009 00000225"), // converter and pin with out amps
            ("0 03 F0009 00400381", "0 03 F0009 00400385"),
            ("0 01 F0012 00000000", "0 01 F0012 80053F2A"),
        ],
    ];
    for (k, reps) in variants.iter().enumerate() {
        let mut text = String::from(AA01);
        for (a, b) in reps.iter() {
            assert!(text.contains(a), "variant {k}: {a}");
            text = text.replace(a, b);
        }
        let d = Dump::parse(&text).unwrap();
        let codec = Codec::read(&mut d.source(0), 0).unwrap();
        let outs = outputs(&codec);
        assert_eq!(outs.len(), 5, "variant {k}");
        for o in &outs {
            assert_eq!(
                ati::setup(&codec, o, &Stream::default()).unwrap(),
                v1_ati(&codec, o),
                "variant {k}, pin {:#x}",
                o.pin
            );
        }
    }
}

/// The V1 transliteration also agrees on the random codecs of the hostile tests.
#[test]
fn chaos_path_setup_is_v1s() {
    let mut compared = 0;
    for seed in 0..1500u64 {
        let Ok(codec) = Codec::read(&mut super::hostile::Chaos(seed), 0) else {
            continue;
        };
        if codec.is_ati() {
            continue;
        }
        for o in outputs(&codec) {
            if let Ok(list) = path_setup(&codec, &o, &Stream::default()) {
                assert_eq!(list, v1_path_setup(&codec, &o), "seed {seed}");
                compared += 1;
            }
        }
    }
    assert!(compared > 50, "only {compared} paths compared");
}
