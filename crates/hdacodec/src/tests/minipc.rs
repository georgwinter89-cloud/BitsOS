//! The real codec walks read on the Mini-PC on 21.10.2026 (`ktest sn.hda.dump`, after fix/hda), not generated.

use std::vec::Vec;

use super::load;
use crate::path::OutputKind;
use crate::walk::check;

const ALC269_REAL: &str = include_str!("../../fixtures/alc269-minipc.txt");
const AA01_REAL: &str = include_str!("../../fixtures/aa01-minipc.txt");

#[test]
fn alc269_on_the_minipc_has_speaker_and_headphone_on_one_dac() {
    let (dump, codec, outs) = load(ALC269_REAL);
    assert!(check(&dump, 0).complete());
    assert_eq!(codec.vendor_id, 0x10EC_0269);
    let pins: Vec<(u8, OutputKind)> = outs.iter().map(|o| (o.pin, o.kind)).collect();
    assert_eq!(pins, [(0x14, OutputKind::Speaker), (0x15, OutputKind::Headphone)]);
    for o in &outs {
        assert_eq!(o.path.last().map(|h| h.nid), Some(0x02), "pin {:#x} ends at DAC 0x02", o.pin);
    }
    assert!(outs.iter().find(|o| o.pin == 0x15).is_some_and(|o| o.jack.is_some()), "the headphone jack senses");
}

#[test]
fn aa01_on_the_minipc_has_four_hdmi_pins() {
    let (dump, codec, outs) = load(AA01_REAL);
    assert!(check(&dump, 0).complete());
    assert_eq!(codec.vendor_id, 0x1002_AA01);
    let pins: Vec<u8> = outs.iter().map(|o| o.pin).collect();
    assert_eq!(pins, [0x3, 0x5, 0x7, 0x9]);
    assert!(outs.iter().all(|o| o.kind == OutputKind::Digital && o.hdmi));
}
