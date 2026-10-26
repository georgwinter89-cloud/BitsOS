//! The two Mini-PC codecs: V1's proven paths and the exact verb lists.

use std::vec;
use std::vec::Vec;

use super::{AA01, ALC269, load};
use crate::path::{Hop, JackSense, OutputKind};
use crate::setup::{Action, Stream, output_setup};
use crate::walk::check;

fn set(nid: u8, verb: u32) -> Action {
    Action::Set { nid, verb }
}

fn nids(path: &[Hop]) -> Vec<u8> {
    path.iter().map(|h| h.nid).collect()
}

#[test]
fn fixtures_match_generator_while_synthetic() {
    for (name, text) in [("alc269", ALC269), ("aa01", AA01)] {
        let dump = crate::dump::Dump::parse(text).unwrap();
        if dump.is_synthetic() {
            assert_eq!(
                crate::synth::fixture(name).unwrap().to_text(),
                text,
                "{name}: rerun the synth example"
            );
        }
    }
}

#[test]
fn fixtures_are_complete_walks() {
    for text in [ALC269, AA01] {
        let (dump, codec, _) = load(text);
        let c = check(&dump, 0);
        assert!(c.complete(), "{c:?}");
        assert!(codec.missing.is_empty());
    }
}

#[test]
fn alc269_outputs_are_v1_paths() {
    let (_, codec, outs) = load(ALC269);
    assert_eq!(codec.vendor_id, 0x10EC_0269);
    let found: Vec<_> = outs
        .iter()
        .map(|o| (o.pin, o.kind, nids(&o.path)))
        .collect();
    assert_eq!(
        found,
        vec![
            (0x14, OutputKind::Speaker, vec![0x14, 0x0C, 0x02]),
            (0x15, OutputKind::Headphone, vec![0x15, 0x0C, 0x02])
        ]
    );
    assert!(outs.iter().all(|o| o.converter() == 0x02));
    // Speaker is fixed (no jack); the headphone jack senses without a trigger.
    assert_eq!(outs[0].jack, None);
    assert_eq!(
        outs[1].jack,
        Some(JackSense {
            pin: 0x15,
            trigger: false,
            unsol: true
        })
    );
}

fn alc269_list(pin: u8, pinctl: u32) -> Vec<Action> {
    vec![
        set(0x01, 0x70500),
        Action::WaitFor {
            nid: 0x01,
            verb: 0xF0500,
            mask: 0xF0,
            value: 0,
            timeout_ms: 200,
        },
        set(pin, 0x70500),
        set(0x02, 0x70500),
        set(pin, 0x3B000),  // pin out amp: 0 dB is step 0, unmuted
        set(pin, 0x70100),  // pin selects mixer 0x0C (input 0)
        set(0x0C, 0x37000), // mixer input 0 (DAC 2) unmuted
        set(0x02, 0x3B057), // DAC out amp at 0 dB (offset 0x57)
        set(0x02, 0x20011), // 48 kHz, 16 bit, stereo
        set(0x02, 0x70610), // stream 1, channel 0
        set(pin, pinctl),
        set(pin, 0x70C02), // EAPD
    ]
}

#[test]
fn alc269_setup_lists() {
    let (_, codec, outs) = load(ALC269);
    let s = Stream::default();
    assert_eq!(
        output_setup(&codec, &outs[0], &s).unwrap(),
        alc269_list(0x14, 0x70740)
    );
    assert_eq!(
        output_setup(&codec, &outs[1], &s).unwrap(),
        alc269_list(0x15, 0x707C0)
    );
}

#[test]
fn aa01_outputs_and_ati_list() {
    let (_, codec, outs) = load(AA01);
    assert!(codec.is_ati() && codec.ati_rev3());
    let found: Vec<_> = outs
        .iter()
        .map(|o| (o.pin, o.endpoint, nids(&o.path)))
        .collect();
    assert_eq!(
        found,
        vec![
            (0x03, Some(0), vec![0x03, 0x02]),
            (0x05, Some(1), vec![0x05, 0x04]),
            (0x07, Some(2), vec![0x07, 0x06]),
            (0x09, Some(3), vec![0x09, 0x08]),
            (0x0B, Some(4), vec![0x0B, 0x0A]),
        ]
    );
    assert!(outs.iter().all(|o| o.kind == OutputKind::Digital && o.hdmi));
    let list = output_setup(&codec, &outs[0], &Stream::default()).unwrap();
    let hbr = Action::UpdateBits {
        nid: 3,
        get: 0xF7C00,
        set: 0x77C,
        clear: 0x10,
        set_bits: 0,
    };
    let want = vec![
        set(0x01, 0x70500),
        Action::WaitFor {
            nid: 0x01,
            verb: 0xF0500,
            mask: 0xF0,
            value: 0,
            timeout_ms: 200,
        },
        set(3, 0x77200),
        set(3, 0x78901),
        set(3, 0x70740),
        set(3, 0x70100),
        set(2, 0x70D01),
        set(2, 0x72D01),
        set(3, 0x77701),
        set(3, 0x78511),
        set(3, 0x77800),
        set(3, 0x78600),
        set(3, 0x77900),
        set(3, 0x78700),
        set(3, 0x77A00),
        set(3, 0x78800),
        set(3, 0x77100),
        set(2, 0x770B4),
        hbr,
        set(2, 0x70610),
        set(2, 0x20011),
    ];
    assert_eq!(list, want);
    // HBR: written only when the enable bit is on.
    assert_eq!(hbr.update_verb(0x01), None);
    assert_eq!(hbr.update_verb(0x11), Some(0x77C01));
}

#[test]
fn aa01_jack_sense_reads_measured_value() {
    let (dump, _, outs) = load(AA01);
    let sense = outs[0].jack.expect("HDMI pin senses");
    assert_eq!(sense.before_read(), None);
    let answer = dump.get(0, sense.pin, sense.read_verb()).flatten().unwrap();
    assert_eq!(answer, 0x7FFF_FFFF);
    assert!(!JackSense::plugged(answer) && JackSense::eld_valid(answer));
}

#[test]
fn stream_tag_is_checked() {
    let (_, codec, outs) = load(ALC269);
    let bad = Stream {
        tag: 0,
        ..Stream::default()
    };
    assert!(output_setup(&codec, &outs[0], &bad).is_err());
    let s = Stream {
        tag: 5,
        channel: 2,
        format: 0x4011,
    };
    let list = output_setup(&codec, &outs[0], &s).unwrap();
    assert!(list.contains(&set(0x02, 0x70652)) && list.contains(&set(0x02, 0x24011)));
}

#[test]
fn jack_sense_verbs() {
    let (_, _, outs) = load(ALC269);
    let hp = outs[1].jack.unwrap();
    // V1 switch.asm 125-129: 709 (trigger) where the pin asks for it, then F09.
    assert_eq!(hp.before_read(), None);
    assert_eq!(hp.read_verb(), 0xF0900);
    let with_trigger = JackSense {
        trigger: true,
        ..hp
    };
    assert_eq!(with_trigger.before_read(), Some(set(0x15, 0x70900)));
    // V1 polls; unsolicited enable is Linux AC_VERB_SET_UNSOLICITED_ENABLE | AC_USRSP_EN | tag.
    assert_eq!(hp.unsol_enable(1), Some(set(0x15, 0x70881)));
    assert_eq!(hp.unsol_enable(0x3F), Some(set(0x15, 0x708BF)));
    assert_eq!(hp.unsol_enable(0x40), None);
    let no_unsol = JackSense { unsol: false, ..hp };
    assert_eq!(no_unsol.unsol_enable(1), None);

    let (_, _, outs) = load(AA01);
    let hdmi = outs[0].jack.unwrap();
    assert_eq!(hdmi.pin, 0x03);
    assert_eq!(hdmi.read_verb(), 0xF0900);
    assert_eq!(hdmi.unsol_enable(2), Some(set(0x03, 0x70882)));
    assert!(JackSense::plugged(0x8000_0000) && !JackSense::eld_valid(0x8000_0000));
    assert!(JackSense::eld_valid(0x4000_0000) && !JackSense::plugged(0x4000_0000));
}
