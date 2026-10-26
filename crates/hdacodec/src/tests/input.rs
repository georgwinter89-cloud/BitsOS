//! SN-9: input pins, their ADC paths and the recording setup on the ALC269 (real Mini-PC walk and synthetic).

use std::vec::Vec;

use super::{ALC269, load};
use crate::input::{InputKind, VREF_80, capture_amp, input_setup, inputs};
use crate::setup::{Action, Stream};

const ALC269_REAL: &str = include_str!("../../fixtures/alc269-minipc.txt");

fn sets(list: &[Action]) -> Vec<(u8, u32)> {
    list.iter().filter_map(|a| match *a {
        Action::Set { nid, verb } => Some((nid, verb)),
        _ => None,
    }).collect()
}

#[test]
fn minipc_has_the_jack_mic_and_the_internal_mic() {
    let (_, codec, _) = load(ALC269_REAL);
    let ins = inputs(&codec);
    let pins: Vec<(u8, InputKind, bool, u8)> = ins.iter().map(|i| (i.pin, i.kind, i.fixed, i.converter())).collect();
    // 0x18: Mic In jack (04A19030) through mixer 0x23 to ADC 0x08; 0x12: fixed internal mic (90A60140), only
    // mixer 0x22 lists it, so ADC 0x09. 0x19/0x1A/0x1B are not connected (411111F0).
    assert_eq!(pins, [(0x12, InputKind::Mic, true, 0x09), (0x18, InputKind::Mic, false, 0x08)]);
    let jack = &ins[1];
    assert_eq!(jack.path.iter().map(|h| (h.nid, h.input)).collect::<Vec<_>>(), [(0x08, Some(0)), (0x23, Some(0)), (0x18, None)]);
    assert!(jack.jack.is_some(), "the mic jack senses a plug");
    assert!(ins[0].jack.is_none());
}

#[test]
fn minipc_jack_mic_setup() {
    let (_, codec, _) = load(ALC269_REAL);
    let ins = inputs(&codec);
    let jack = ins.iter().find(|i| i.pin == 0x18).unwrap();
    let list = input_setup(&codec, jack, &Stream { tag: 1, channel: 0, format: 0x0011 }).unwrap();
    assert_eq!(list[0], Action::Set { nid: 0x01, verb: 0x70500 });
    assert!(matches!(list[1], Action::WaitFor { nid: 0x01, .. }));
    let s = sets(&list[2..]);
    assert_eq!(s, [
        // Power control on the ADC and the pin (caps bit 10); the mixer has none.
        (0x08, 0x70500),
        (0x18, 0x70500),
        // ADC 0x08 (one input): its amp at 0 dB (offset 0x17), unmuted.
        (0x08, 0x37017),
        // Mixer 0x23: input 0 (the pin) unmuted; mute-only amp.
        (0x23, 0x37000),
        (0x08, 0x20011),
        (0x08, 0x70610),
        // Pin boost at step 0, then input on with VREF 80 % (pin caps 0x3734 offer 80 %).
        (0x18, 0x37000),
        (0x18, 0x70700 | 0x20 | VREF_80 as u32),
    ]);
    let (nid, idx, caps) = capture_amp(&codec, jack).unwrap();
    assert_eq!((nid, idx, caps.offset(), caps.steps()), (0x08, 0, 0x17, 0x3F));
}

#[test]
fn synthetic_alc269_jack_mic() {
    let (_, codec, _) = load(ALC269);
    let ins = inputs(&codec);
    let m = ins.iter().find(|i| i.pin == 0x18).expect("mic jack");
    assert_eq!((m.kind, m.fixed, m.converter()), (InputKind::Mic, false, 0x08));
}
