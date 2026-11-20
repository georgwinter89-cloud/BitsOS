use super::fixture::{self, ALTS};
use crate::desc::*;
use std::vec::Vec;

fn function() -> Function {
    Function::parse(&fixture::config()).unwrap()
}

/// The two totals the real device reports. Everything hand-built above has to come out exactly this long.
#[test]
fn the_fixture_has_the_real_devices_lengths() {
    assert_eq!(fixture::config().len(), 0x0C73);
    assert_eq!(9 + fixture::control_tree().len(), 0x0144);
}

#[test]
fn five_clocks_five_paths() {
    let f = function();
    assert_eq!(f.control_iface, 0);
    assert_eq!(f.clocks.iter().map(|c| c.id).collect::<Vec<_>>(), [3, 4, 5, 6, 8]);
    assert_eq!(f.inputs.len(), 5);
    assert_eq!(f.outputs.len(), 5);
    assert_eq!(f.features.iter().map(|u| u.id).collect::<Vec<_>>(), [27, 28, 29, 30, 32]);
    assert_eq!(f.other_units, [33], "the extension unit, kept only as an id");
}

#[test]
fn the_five_jacks_are_the_ones_lsusb_names() {
    let f = function();
    let ty = |id| f.input(id).map(|t| t.ty).or(f.output(id).map(|t| t.ty)).unwrap();
    assert_eq!(ty(21), terminal::SPEAKER);
    assert_eq!(ty(22), terminal::HEADPHONES);
    assert_eq!(ty(12), terminal::MICROPHONE);
    assert_eq!(ty(11), terminal::DESKTOP_MICROPHONE);
    assert_eq!(ty(24), terminal::SPDIF);
}

#[test]
fn terminals_read_field_by_field() {
    let f = function();
    assert_eq!(
        f.input(13),
        Some(&InputTerminal {
            id: 13,
            ty: terminal::USB_STREAMING,
            assoc: 0,
            clock: 5,
            channels: 8,
            channel_config: 0x63F,
            controls: 0,
        })
    );
    assert_eq!(
        f.output(21),
        Some(&OutputTerminal { id: 21, ty: terminal::SPEAKER, assoc: 13, source: 29, clock: 5, controls: 4 })
    );
}

#[test]
fn feature_units_keep_a_master_and_their_channels() {
    let f = function();
    let speaker = f.feature(29).unwrap();
    assert_eq!(speaker.source, 13);
    assert_eq!(speaker.controls.len(), 9);
    assert!(speaker.mute_writable());
    assert!(speaker.volume_writable());
    let headphones = f.feature(30).unwrap();
    assert_eq!(headphones.controls, [3, 0, 0]);
    assert!(headphones.mute_writable());
    assert!(!headphones.volume_writable());
}

#[test]
fn clock_controls_are_two_bits_each() {
    let clock = *function().clock(5).unwrap();
    assert_eq!(clock.controls, 7);
    assert!(clock.frequency_writable());
    assert!(clock.validity_readable());
    assert!(!ClockSource { controls: 0b01, ..clock }.frequency_writable(), "read only");
    assert!(!ClockSource { controls: 0b0011, ..clock }.validity_readable());
}

#[test]
fn all_sixty_alternate_settings_arrive_in_order() {
    let f = function();
    assert_eq!(f.alts.len(), 60);
    for (got, want) in f.alts.iter().zip(ALTS.iter()) {
        assert_eq!((got.iface, got.alt, got.link), (want.iface, want.alt, want.link));
        assert_eq!((got.formats, got.channels, got.channel_config), (want.formats, want.channels, want.config));
        assert_eq!((got.subslot, got.bits), (want.subslot, want.bits));
        assert_eq!((got.endpoint.address, got.endpoint.attributes), (want.ep, want.attributes));
        assert_eq!(got.endpoint.max_packet, want.mps);
        assert_eq!(got.format_type, fixture::format_type(want));
    }
    assert!(f.alts.iter().all(|a| a.alt != 0), "the idle setting is not a stream");
}

#[test]
fn only_the_s_pdif_extra_is_type_iii() {
    let f = function();
    let other: Vec<(u8, u8)> =
        f.alts.iter().filter(|a| a.format_type != FORMAT_TYPE_I).map(|a| (a.iface, a.alt)).collect();
    assert_eq!(other, [(5, 11)]);
}

#[test]
fn the_hid_interface_leaves_nothing_behind() {
    let f = function();
    assert!(f.alts.iter().all(|a| a.iface != 6));
    assert!(f.alts.iter().all(|a| a.endpoint.address != 0x8A));
}

#[test]
fn endpoint_sync_as_lsusb_shows_it() {
    let f = function();
    let at = |i, a| f.alts.iter().find(|x| x.iface == i && x.alt == a).unwrap().endpoint;
    assert_eq!(at(3, 1).sync(), Sync::Adaptive);
    assert!(!at(3, 1).is_in());
    assert_eq!(at(1, 1).sync(), Sync::Async);
    assert!(at(1, 1).is_in());
    assert!(at(1, 1).is_isochronous());
}

#[test]
fn a_high_speed_packet_size_counts_its_extra_transactions() {
    let ep = |max_packet| Endpoint { address: 5, attributes: 9, max_packet, interval: 1 };
    assert_eq!(ep(0x00F8).bytes_per_interval(), 248);
    assert_eq!(ep(0x0BE0).bytes_per_interval(), 992 * 2);
    assert_eq!(ep(0x0A2E).bytes_per_interval(), 558 * 2);
    assert_eq!(ep(0x1400).bytes_per_interval(), 1024 * 3);
}

#[test]
fn intervals_are_powers_of_125_us() {
    let ep = |interval| Endpoint { address: 5, attributes: 9, max_packet: 8, interval };
    assert_eq!(ep(1).interval_us(), Some(125));
    assert_eq!(ep(4).interval_us(), Some(1000));
    assert_eq!(ep(16).interval_us(), Some(125 << 15));
    assert_eq!(ep(0).interval_us(), None);
    assert_eq!(ep(17).interval_us(), None);
    assert_eq!((ep(1).interval_us_at(true), ep(4).interval_us_at(true), ep(4).interval_us_at(false)), (Some(1000), Some(8000), Some(1000)));
    assert_eq!((ep(0).interval_us_at(true), ep(17).interval_us_at(true)), (None, None));
}

/// Cut anywhere and say so in wTotalLength: the walk ends in an answer, never a panic, and a cut through the unit tree
/// does not come back as a smaller tree that looks valid. Past the tree a cut between streams is a device with fewer of
/// them.
#[test]
fn a_cut_through_the_unit_tree_is_refused() {
    let good = fixture::config();
    let tree_end = 9 + 8 + 9 + 0x0144;
    for cut in 9..good.len() {
        let mut short = good[..cut].to_vec();
        short[2..4].copy_from_slice(&(cut as u16).to_le_bytes());
        let parsed = Function::parse(&short);
        if cut < tree_end {
            assert!(parsed.is_err(), "cut at {cut} must not pass");
        }
    }
    let mut at_tree_end = good[..tree_end].to_vec();
    at_tree_end[2..4].copy_from_slice(&(tree_end as u16).to_le_bytes());
    assert_eq!(Function::parse(&at_tree_end).unwrap().alts.len(), 0);
}

#[test]
fn a_tree_without_a_header_is_refused() {
    let c = fixture::wrap(&[9, 4, 0, 0, 0, 1, 1, 0x20, 0], 1);
    assert_eq!(Function::parse(&c), Err(DescError::NoControlHeader));
}

#[test]
fn a_length_of_zero_ends_the_walk_with_an_error() {
    let mut c = fixture::config();
    // The first descriptor after the configuration.
    c[9] = 0;
    assert_eq!(Function::parse(&c), Err(DescError::Truncated { at: 9 }));
    c[9] = 1;
    assert_eq!(Function::parse(&c), Err(DescError::Truncated { at: 9 }));
    c[9] = 0xFF;
    assert!(Function::parse(&c).is_err());
}

#[test]
fn wrong_type_or_too_little_is_refused_up_front() {
    assert_eq!(Function::parse(&[]), Err(DescError::Truncated { at: 0 }));
    let mut c = fixture::config();
    c[1] = 4;
    assert_eq!(Function::parse(&c), Err(DescError::NotAConfiguration));
    let c = fixture::config();
    assert_eq!(Function::parse(&c[..100]), Err(DescError::ConfigLength { total: 0x0C73, have: 100 }));
}

#[test]
fn a_header_promising_more_than_the_tree_is_refused() {
    let tree = fixture::control_tree();
    let declared = (9 + tree.len() + 1) as u16;
    let c = fixture::wrap(&fixture::control_interface(&tree, declared), 1);
    assert_eq!(Function::parse(&c), Err(DescError::ControlLength { total: declared, walked: 9 + tree.len() }));
}

#[test]
fn a_unit_too_short_for_its_kind_is_refused() {
    // bLength as a terminal's would be, cut by one; the tree's own lengths stay honest about it.
    for (bytes, subtype) in [
        (std::vec![7u8, 0x24, 0x0A, 3, 7, 7, 0], 0x0A),
        (std::vec![16u8, 0x24, 2, 13, 1, 1, 0, 5, 2, 3, 0, 0, 0, 0, 0, 0], 2),
        (std::vec![11u8, 0x24, 3, 21, 1, 3, 13, 29, 5, 4, 0], 3),
        (std::vec![9u8, 0x24, 6, 29, 13, 3, 0, 0, 0], 6),
    ] {
        let declared = (9 + bytes.len()) as u16;
        let c = fixture::wrap(&fixture::control_interface(&bytes, declared), 1);
        let at = 9 + 8 + 9 + 9;
        assert_eq!(Function::parse(&c), Err(DescError::TooShort { at, subtype, len: bytes[0] }), "{subtype}");
    }
}

#[test]
fn a_feature_unit_with_a_broken_channel_count_is_refused() {
    let bytes = [11u8, 0x24, 6, 29, 13, 3, 0, 0, 0, 1, 0];
    let declared = (9 + bytes.len()) as u16;
    let c = fixture::wrap(&fixture::control_interface(&bytes, declared), 1);
    assert!(matches!(Function::parse(&c), Err(DescError::TooShort { subtype: 6, .. })));
}

#[test]
fn another_adc_version_is_refused() {
    let tree = fixture::control_tree();
    let mut body = fixture::control_interface(&tree, (9 + tree.len()) as u16);
    // bcdADC in the header, right after the IAD and the interface.
    let at = 8 + 9 + 3;
    body[at + 1] = 0x01;
    assert_eq!(Function::parse(&fixture::wrap(&body, 1)), Err(DescError::AdcVersion(0x0100)));
}

#[test]
fn no_audio_control_is_refused() {
    let c = fixture::wrap(&[9, 4, 0, 0, 0, 3, 0, 0, 0], 1);
    assert_eq!(Function::parse(&c), Err(DescError::NoControlInterface));
}

#[test]
fn a_uac1_function_is_not_taken_for_this_one() {
    let tree = fixture::control_tree();
    let mut body = fixture::control_interface(&tree, (9 + tree.len()) as u16);
    body[8 + 7] = 0;
    assert_eq!(Function::parse(&fixture::wrap(&body, 1)), Err(DescError::NoControlInterface));
}

#[test]
fn a_second_function_is_refused() {
    let tree = fixture::control_tree();
    let mut body = fixture::control_interface(&tree, (9 + tree.len()) as u16);
    let second = fixture::control_interface(&tree, (9 + tree.len()) as u16);
    body.extend_from_slice(&second);
    assert_eq!(Function::parse(&fixture::wrap(&body, 2)), Err(DescError::SecondControlInterface(0)));
}

#[test]
fn a_stream_with_a_part_missing_is_refused() {
    let tree = fixture::control_tree();
    let good_body = fixture::control_interface(&tree, (9 + tree.len()) as u16);
    let mut alt = Vec::new();
    fixture::streaming_alt(&mut alt, &ALTS[0]);
    let parts = [(9usize, 16usize), (25, 6), (31, 7)];
    for (at, len) in parts {
        let mut body = good_body.clone();
        body.extend_from_slice(&alt[..at]);
        body.extend_from_slice(&alt[at + len..]);
        let want = DescError::IncompleteAlt { iface: 1, alt: 1 };
        assert_eq!(Function::parse(&fixture::wrap(&body, 2)), Err(want), "without bytes {at}..{}", at + len);
    }
}
