use crate::desc::{FeatureUnit, Function};
use crate::feature::{self, MUTE, VOLUME};
use crate::fixture;
use std::vec;
use std::vec::Vec;

fn board() -> Function {
    Function::parse(&fixture::config()).unwrap()
}

#[test]
fn the_speaker_unit_has_mute_on_the_master_and_volume_on_each_channel() {
    let f = board();
    let fu = f.feature(29).unwrap();
    assert_eq!(feature::writable(fu, MUTE).collect::<Vec<_>>(), vec![0]);
    assert_eq!(feature::writable(fu, VOLUME).collect::<Vec<_>>(), (1..=8).collect::<Vec<u8>>());
    let hp = f.feature(30).unwrap();
    assert_eq!(feature::writable(hp, VOLUME).count(), 0, "the headphone jack has mute only");
}

#[test]
fn requests_carry_control_channel_unit_and_interface() {
    // mixer.c:466-498: CUR, interface recipient, (control << 8 | channel), (unit << 8 | interface).
    let (s, d) = feature::set_mute(0, 29, 0, true);
    assert_eq!((s.request_type, s.request, s.value, s.index, s.length, d), (0x21, 1, 0x0100, 0x1D00, 1, [1]));
    let r = feature::volume_range(0, 29, 2);
    assert_eq!((r.request_type, r.request, r.value, r.index, r.length), (0xA1, 2, 0x0202, 0x1D00, 8));
    let (v, d) = feature::set_volume(0, 29, 1, -256);
    assert_eq!((v.value, v.length, d), (0x0201, 2, (-256i16).to_le_bytes()));
}

#[test]
fn the_range_reply_gives_0_db_inside_it() {
    let reply = |n: u16, min: i16, max: i16| {
        let mut r = n.to_le_bytes().to_vec();
        for x in [min, max, 256] {
            r.extend_from_slice(&x.to_le_bytes());
        }
        r
    };
    assert_eq!(feature::decode_volume_range(&reply(1, -0x7F00, 0)), Some((-0x7F00, 0, 256)));
    assert_eq!(feature::unity(feature::decode_volume_range(&reply(1, -0x7F00, 0x0600))), 0);
    assert_eq!(feature::unity(feature::decode_volume_range(&reply(1, -0x2000, -0x0100))), -0x0100, "a range below 0 dB: its top");
    assert_eq!(feature::unity(feature::decode_volume_range(&reply(1, 0x0100, 0x0600))), 0x0100);
    assert_eq!(feature::decode_volume_range(&reply(0, 0, 0)), None, "no triplet");
    assert_eq!(feature::decode_volume_range(&reply(1, 5, -5)), None, "min above max");
    assert_eq!(feature::decode_volume_range(&[1, 0, 0]), None, "short");
    assert_eq!(feature::unity(None), 0);
    let silent = FeatureUnit { id: 1, source: 2, controls: vec![0, 0] };
    assert_eq!(feature::writable(&silent, MUTE).count(), 0);
}
