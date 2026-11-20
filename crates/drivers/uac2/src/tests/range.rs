use super::fixture::{SPEAKER_RATES, range_reply};
use crate::range::*;
use std::vec::Vec;

/// (min, max, res) triplets behind their count.
fn reply(triplets: &[(u32, u32, u32)]) -> Vec<u8> {
    let mut r = (triplets.len() as u16).to_le_bytes().to_vec();
    for (min, max, res) in triplets {
        r.extend_from_slice(&min.to_le_bytes());
        r.extend_from_slice(&max.to_le_bytes());
        r.extend_from_slice(&res.to_le_bytes());
    }
    r
}

#[test]
fn the_count_is_two_bytes_not_three() {
    let r = reply(&[(48_000, 48_000, 0)]);
    assert_eq!(r.len(), 2 + 12);
    assert_eq!(&r[..2], &[1, 0]);
    let ranges = Ranges::parse(&r).unwrap();
    assert_eq!(ranges.triplets().next(), Some(Triplet { min: 48_000, max: 48_000, res: 0 }));
    assert_eq!(ranges.triplets().count(), 1);
}

#[test]
fn a_count_above_255_needs_both_bytes() {
    let triplets: Vec<(u32, u32, u32)> = (0..300).map(|i| (8_000 + i, 8_000 + i, 0)).collect();
    let r = reply(&triplets);
    assert_eq!(&r[..2], &[0x2C, 0x01]);
    assert_eq!(Ranges::parse(&r).unwrap().triplets().count(), 300);
}

#[test]
fn the_rates_of_the_speaker_clock() {
    let r = range_reply(&SPEAKER_RATES);
    let ranges = Ranges::parse(&r).unwrap();
    for rate in SPEAKER_RATES {
        assert!(ranges.contains(rate), "{rate}");
    }
    for rate in [0, 8_000, 44_099, 48_001, 192_001, 768_000] {
        assert!(!ranges.contains(rate), "{rate}");
    }
}

#[test]
fn a_step_range_holds_its_steps_only() {
    let r = reply(&[(44_100, 88_200, 44_100)]);
    let ranges = Ranges::parse(&r).unwrap();
    assert!(ranges.contains(44_100));
    assert!(ranges.contains(88_200));
    assert!(!ranges.contains(66_150));
    assert!(!ranges.contains(132_300));
}

#[test]
fn resolution_one_is_continuous_and_zero_is_only_the_minimum() {
    let r = reply(&[(8_000, 192_000, 1)]);
    assert!(Ranges::parse(&r).unwrap().contains(12_345));
    let r = reply(&[(8_000, 192_000, 0)]);
    let ranges = Ranges::parse(&r).unwrap();
    assert!(ranges.contains(8_000));
    assert!(!ranges.contains(8_001));
    assert!(!ranges.contains(192_000));
}

#[test]
fn triplets_linux_would_skip_are_skipped() {
    let r = reply(&[(96_000, 48_000, 0), (0x8000_0000, 0x8000_0000, 0), (1, 0xFFFF_FFFF, 1), (1, 2, 0x8000_0000)]);
    let ranges = Ranges::parse(&r).unwrap();
    assert_eq!(ranges.triplets().count(), 0);
    assert!(!ranges.contains(48_000));
    assert!(!ranges.contains(0x8000_0000));
}

#[test]
fn a_truncated_reply_is_refused() {
    assert_eq!(Ranges::parse(&[]), Err(RangeError::Truncated { need: 2, got: 0 }));
    assert_eq!(Ranges::parse(&[1]), Err(RangeError::Truncated { need: 2, got: 1 }));
    let r = reply(&[(1, 1, 0), (2, 2, 0)]);
    assert_eq!(Ranges::parse(&r[..25]), Err(RangeError::Truncated { need: 26, got: 25 }));
    // The real count with a lying first byte.
    let mut lying = reply(&[(1, 1, 0)]);
    lying[1] = 0xFF;
    assert!(matches!(Ranges::parse(&lying), Err(RangeError::Truncated { .. })));
}

#[test]
fn bytes_past_the_table_are_not_read() {
    let mut r = reply(&[(48_000, 48_000, 0)]);
    r.extend_from_slice(&96_000u32.to_le_bytes());
    r.extend_from_slice(&96_000u32.to_le_bytes());
    r.extend_from_slice(&0u32.to_le_bytes());
    let ranges = Ranges::parse(&r).unwrap();
    assert!(!ranges.contains(96_000));
}

#[test]
fn an_empty_table_offers_nothing() {
    let ranges = Ranges::parse(&[0, 0]).unwrap();
    assert_eq!(ranges.triplets().count(), 0);
    assert!(!ranges.contains(48_000));
}

#[test]
fn the_second_request_asks_for_exactly_the_table() {
    assert_eq!(full_length(&[7, 0]), Ok(2 + 7 * 12));
    assert_eq!(full_length(&[0, 0]), Ok(2));
    assert_eq!(full_length(&[1]), Err(RangeError::Truncated { need: 2, got: 1 }));
    let max = MAX_TRIPLETS.to_le_bytes();
    assert!(full_length(&max).is_ok());
    let over = (MAX_TRIPLETS + 1).to_le_bytes();
    assert_eq!(full_length(&over), Err(RangeError::TooManyTriplets(MAX_TRIPLETS + 1)));
}

#[test]
fn a_hostile_resolution_takes_no_time() {
    // Enumerating this would take a billion steps; `contains` does not enumerate.
    let r = reply(&[(1, 0x7FFF_FFFF, 2)]);
    let ranges = Ranges::parse(&r).unwrap();
    assert!(ranges.contains(0x7FFF_FFFF));
    assert!(!ranges.contains(0x7FFF_FFFE));
}
