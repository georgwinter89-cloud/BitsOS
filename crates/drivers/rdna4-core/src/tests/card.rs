use crate::card::*;

const BOTH: [Card; 2] = [CARD_A, CARD_B];
const PICTURE: u64 = 2560 * 1440 * 4;

#[test]
fn the_card_with_the_picture_is_chosen_by_address() {
    assert_eq!(choose(&BOTH, CARD_A.bar0_base + 0x10_0000, PICTURE), Ok(CARD_A.at));
    assert_eq!(choose(&BOTH, CARD_B.bar0_base, PICTURE), Ok(CARD_B.at));
    assert_eq!(CARD_A.at, Bdf { bus: 0x04, device: 0, function: 0 });
    assert_eq!(CARD_B.at, Bdf { bus: 0x07, device: 0, function: 0 });
}

#[test]
fn the_order_of_the_list_does_not_matter() {
    let flipped = [CARD_B, CARD_A];
    assert_eq!(choose(&flipped, CARD_A.bar0_base, PICTURE), Ok(CARD_A.at));
}

#[test]
fn a_framebuffer_up_to_the_last_byte_of_the_bar_counts() {
    let end = CARD_A.bar0_base + CARD_A.bar0_bytes;
    assert_eq!(choose(&BOTH, end - PICTURE, PICTURE), Ok(CARD_A.at));
}

#[test]
fn a_framebuffer_only_partly_in_a_bar_is_refused() {
    let end = CARD_A.bar0_base + CARD_A.bar0_bytes;
    assert_eq!(choose(&BOTH, end - PICTURE + 1, PICTURE), Err(CardRefusal::NoCardHoldsIt));
    assert_eq!(choose(&BOTH, CARD_A.bar0_base - 1, PICTURE), Err(CardRefusal::NoCardHoldsIt));
}

#[test]
fn a_framebuffer_in_no_card_is_refused() {
    assert_eq!(choose(&BOTH, 0x8000_0000, PICTURE), Err(CardRefusal::NoCardHoldsIt));
    assert_eq!(choose(&[], CARD_A.bar0_base, PICTURE), Err(CardRefusal::NoCardHoldsIt));
    assert_eq!(choose(&BOTH, u64::MAX - 8, PICTURE), Err(CardRefusal::NoCardHoldsIt));
}

#[test]
fn an_empty_framebuffer_names_nothing() {
    assert_eq!(choose(&BOTH, CARD_A.bar0_base, 0), Err(CardRefusal::EmptyFramebuffer));
}

#[test]
fn two_bars_over_the_same_place_are_refused_not_guessed() {
    let shadow = Card { at: Bdf { bus: 0x09, device: 0, function: 0 }, ..CARD_A };
    assert_eq!(choose(&[CARD_A, shadow], CARD_A.bar0_base, PICTURE), Err(CardRefusal::Ambiguous(CARD_A.at, shadow.at)));
}

#[test]
fn a_bar_whose_end_wraps_holds_nothing() {
    let wrapped = Card { at: Bdf { bus: 0x09, device: 0, function: 0 }, bar0_base: u64::MAX - 16, bar0_bytes: 1 << 34 };
    assert_eq!(choose(&[wrapped], u64::MAX - 8, 4), Err(CardRefusal::NoCardHoldsIt));
}

#[test]
fn the_two_measured_windows_do_not_overlap() {
    const { assert!(CARD_B.bar0_base + CARD_B.bar0_bytes <= CARD_A.bar0_base) };
    assert_eq!((VENDOR, DEVICE), (0x1002, 0x7550));
}
