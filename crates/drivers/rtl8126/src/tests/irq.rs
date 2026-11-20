use crate::irq::*;

#[test]
fn the_mask_is_linuxs() {
    assert_eq!(IRQ_MASK, 0x2f);
}

#[test]
fn a_card_gone_from_the_bus_is_not_ours() {
    assert_eq!(classify(u32::MAX), Irq::NotOurs);
    assert_eq!(classify(0x0000_ffff), Irq::NotOurs);
}

#[test]
fn bits_outside_the_mask_are_not_ours() {
    assert_eq!(classify(0), Irq::NotOurs);
    assert_eq!(classify(0x0100), Irq::NotOurs);
    assert_eq!(classify(0x8000), Irq::NotOurs);
}

#[test]
fn events_keep_the_whole_status_for_the_acknowledge() {
    assert_eq!(classify(RX_OK | 0x0100), Irq::Events { status: RX_OK | 0x0100, link_changed: false });
    assert_eq!(classify(LINK_CHG), Irq::Events { status: LINK_CHG, link_changed: true });
}
