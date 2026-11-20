use crate::clock::*;
use crate::desc::ClockSource;

const CLOCK: ClockSource = ClockSource { id: 5, attributes: 7, controls: 7, assoc_terminal: 0 };

#[test]
fn requests_go_to_the_clock_on_the_control_interface() {
    assert_eq!(get_rate(0, 5), Setup { request_type: 0xA1, request: 1, value: 0x0100, index: 0x0500, length: 4 });
    assert_eq!(get_valid(0, 5), Setup { request_type: 0xA1, request: 1, value: 0x0200, index: 0x0500, length: 1 });
    assert_eq!(range_head(0, 5), Setup { request_type: 0xA1, request: 2, value: 0x0100, index: 0x0500, length: 2 });
    assert_eq!(
        range_full(0, 5, 86),
        Setup { request_type: 0xA1, request: 2, value: 0x0100, index: 0x0500, length: 86 }
    );
    assert_eq!(get_rate(2, 8).index, 0x0802);
}

#[test]
fn setting_the_rate_sends_four_little_endian_bytes() {
    let (setup, data) = set_rate(0, 6, 48_000);
    assert_eq!(setup, Setup { request_type: 0x21, request: 1, value: 0x0100, index: 0x0600, length: 4 });
    assert_eq!(data, [0x80, 0xBB, 0x00, 0x00]);
}

#[test]
fn reading_the_rate_needs_all_four_bytes() {
    assert_eq!(decode_rate(&[0x80, 0xBB, 0, 0]), Some(48_000));
    assert_eq!(decode_rate(&[0x80, 0xBB, 0]), None);
    assert_eq!(decode_rate(&[]), None);
}

#[test]
fn a_clock_is_valid_if_it_says_so_or_cannot_say() {
    assert!(decode_valid(&CLOCK, Some(&[1])));
    assert!(!decode_valid(&CLOCK, Some(&[0])));
    assert!(!decode_valid(&CLOCK, Some(&[])));
    assert!(!decode_valid(&CLOCK, None), "the request failed");
    let cannot_say = ClockSource { controls: 0b0011, ..CLOCK };
    assert!(decode_valid(&cannot_say, None));
}

#[test]
fn a_clock_at_the_rate_is_left_alone() {
    assert!(!needs_set(48_000, 48_000));
    assert!(needs_set(44_100, 48_000));
}

#[test]
fn the_interface_comes_last_for_uac2() {
    let [first, read, write, valid, last] = start_order(0, 5, 48_000, 3, 1);
    assert_eq!(first, Step::Interface { iface: 3, alt: 0 });
    assert_eq!(read, Step::ReadRate(get_rate(0, 5)));
    let (setup, data) = set_rate(0, 5, 48_000);
    assert_eq!(write, Step::WriteRateIfDifferent(setup, data));
    assert_eq!(valid, Step::ReadValid(get_valid(0, 5)));
    assert_eq!(last, Step::Interface { iface: 3, alt: 1 });
}
