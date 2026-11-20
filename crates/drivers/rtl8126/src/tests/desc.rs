use crate::desc::*;

#[test]
fn sizes() {
    assert_eq!(DESC_BYTES, 16);
    assert_eq!(RING_BYTES, 4096);
    assert_eq!(RING_ALIGN, 256);
    assert_eq!(RX_BUF_SIZE, 16383);
    assert_eq!(RX_MEMORY, 4 * 1024 * 1024);
}

#[test]
fn bytes_are_little_endian_in_linuxs_order() {
    let d = Desc { opts1: 0x8000_3fff, opts2: 0x0102_0304, addr: 0x1122_3344_5566_7788 };
    let b = d.to_bytes();
    assert_eq!(b[..4], [0xff, 0x3f, 0x00, 0x80]);
    assert_eq!(b[4..8], [0x04, 0x03, 0x02, 0x01]);
    assert_eq!(b[8..16], [0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11]);
    assert_eq!(Desc::from_bytes(&b), d);
}

#[test]
fn rx_descriptors_are_the_chips_and_only_the_last_ends_the_ring() {
    let first = rx_init(0, 0x1000);
    assert_eq!(first.opts1, DESC_OWN | 16383);
    assert_eq!(first.addr, 0x1000);
    assert_eq!(rx_init(254, 0).opts1 & RING_END, 0);
    assert_eq!(rx_init(255, 0).opts1 & RING_END, RING_END);
}

#[test]
fn every_rx_descriptor_promises_exactly_the_buffer() {
    for entry in 0..NUM_RX {
        assert_eq!(rx_init(entry, 0).opts1 & 0x3fff, RX_BUF_SIZE);
    }
}

#[test]
fn rearm_keeps_only_the_ring_end_bit() {
    let done = FIRST_FRAG | LAST_FRAG | 64;
    assert_eq!(rx_rearm_opts1(done), DESC_OWN | RX_BUF_SIZE);
    assert_eq!(rx_rearm_opts1(done | RING_END), DESC_OWN | RING_END | RX_BUF_SIZE);
    assert_eq!(rx_rearm_opts1(u32::MAX), DESC_OWN | RING_END | RX_BUF_SIZE);
}

#[test]
fn rx_decode_a_whole_frame_loses_its_checksum() {
    assert_eq!(rx_decode(FIRST_FRAG | LAST_FRAG | 64), Rx::Frame(60));
}

#[test]
fn rx_decode_ownership_comes_first() {
    assert_eq!(rx_decode(DESC_OWN | FIRST_FRAG | LAST_FRAG | 64), Rx::Owned);
    assert_eq!(rx_decode(DESC_OWN | (1 << 21)), Rx::Owned);
}

#[test]
fn rx_decode_error_bits() {
    let frame = FIRST_FRAG | LAST_FRAG | 64;
    let res = 1 << 21;
    assert_eq!(rx_decode(frame | res), Rx::Error { rwt: false, runt: false, crc: false });
    assert_eq!(rx_decode(frame | res | (1 << 22)), Rx::Error { rwt: true, runt: false, crc: false });
    assert_eq!(rx_decode(frame | res | (1 << 20)), Rx::Error { rwt: false, runt: true, crc: false });
    assert_eq!(rx_decode(frame | res | (1 << 19)), Rx::Error { rwt: false, runt: false, crc: true });
}

#[test]
fn rx_decode_only_the_summary_bit_makes_an_error() {
    // M:4792 looks at RxRES alone.
    assert_eq!(rx_decode(FIRST_FRAG | LAST_FRAG | 64 | (1 << 19)), Rx::Frame(60));
}

#[test]
fn rx_decode_fragments_are_dropped() {
    assert_eq!(rx_decode(FIRST_FRAG | 64), Rx::Fragmented);
    assert_eq!(rx_decode(LAST_FRAG | 64), Rx::Fragmented);
    assert_eq!(rx_decode(64), Rx::Fragmented);
}

#[test]
fn rx_decode_shorter_than_the_checksum_does_not_underflow() {
    for len in 0..4 {
        assert_eq!(rx_decode(FIRST_FRAG | LAST_FRAG | len), Rx::TooShort);
    }
    assert_eq!(rx_decode(FIRST_FRAG | LAST_FRAG | 4), Rx::Frame(0));
}

#[test]
fn rx_decode_no_status_can_claim_more_than_the_buffer() {
    let Rx::Frame(len) = rx_decode(FIRST_FRAG | LAST_FRAG | 0xffff) else { panic!() };
    assert!(len <= RX_BUF_SIZE as usize);
    assert_eq!(len, 16379);
}

#[test]
fn short_frames_are_padded_to_60() {
    assert_eq!(tx_padded_len(1), 60);
    assert_eq!(tx_padded_len(59), 60);
    assert_eq!(tx_padded_len(60), 60);
    assert_eq!(tx_padded_len(61), 61);
}

#[test]
fn tx_plan_gives_ownership_only_in_the_second_write() {
    let p = tx_plan(3, 0xabc000, 100).unwrap();
    assert_eq!(p.addr, 0xabc000);
    assert_eq!(p.opts2, 0);
    assert_eq!(p.opts1_prepare, LAST_FRAG | 100);
    assert_eq!(p.opts1_prepare & DESC_OWN, 0);
    assert_eq!(p.opts1_release, DESC_OWN | FIRST_FRAG | LAST_FRAG | 100);
}

#[test]
fn tx_plan_pads_in_the_length_field() {
    let p = tx_plan(0, 0, 14).unwrap();
    assert_eq!(p.len, 60);
    assert_eq!(p.opts1_release & 0xffff, 60);
}

#[test]
fn tx_plan_ring_end_on_the_last_slot_only() {
    assert_eq!(tx_plan(254, 0, 64).unwrap().opts1_release & RING_END, 0);
    assert_eq!(tx_plan(255, 0, 64).unwrap().opts1_release & RING_END, RING_END);
    assert_eq!(tx_plan(255, 0, 64).unwrap().opts1_prepare & RING_END, RING_END);
}

#[test]
fn tx_plan_refuses_nothing_and_too_much() {
    assert_eq!(tx_plan(0, 0, 0), None);
    assert!(tx_plan(0, 0, 1518).is_some());
    assert_eq!(tx_plan(0, 0, 1519), None);
}

#[test]
fn tx_done_is_ownership_returned() {
    assert!(!tx_done(DESC_OWN | 64));
    assert!(tx_done(FIRST_FRAG | LAST_FRAG | 64));
}

#[test]
fn cursor_counts_to_full_and_back() {
    let mut c = TxCursor::default();
    assert_eq!(c.slots_avail(), 256);
    assert_eq!(c.oldest_entry(), None);
    for i in 0..256 {
        assert_eq!(c.next_entry(), Some(i));
        c.sent();
    }
    assert_eq!(c.slots_avail(), 0);
    assert_eq!(c.next_entry(), None, "full is not empty");
    assert_eq!(c.in_flight(), 256);
    assert_eq!(c.oldest_entry(), Some(0));
    assert!(c.reclaimed());
    assert_eq!(c.next_entry(), Some(0));
    assert_eq!(c.oldest_entry(), Some(1));
}

#[test]
fn cursor_rings_again_only_while_something_is_in_flight() {
    let mut c = TxCursor::default();
    c.sent();
    c.sent();
    assert!(c.reclaimed());
    assert!(!c.reclaimed());
    assert_eq!(c.in_flight(), 0);
}

#[test]
fn cursor_goes_round_the_ring_many_times() {
    let mut c = TxCursor::default();
    for _ in 0..1000 {
        c.sent();
        c.reclaimed();
    }
    assert_eq!(c.slots_avail(), 256);
    assert_eq!(c.next_entry(), Some(1000 % 256));
}

#[test]
fn cursor_survives_the_u32_wrap() {
    let mut c = TxCursor::at(u32::MAX, u32::MAX - 2);
    assert_eq!(c.in_flight(), 2);
    c.sent();
    assert_eq!(c.in_flight(), 3);
    assert_eq!(c.slots_avail(), 253);
    assert_eq!(c.next_entry(), Some(0));
    assert_eq!(c.oldest_entry(), Some((u32::MAX - 2) % 256));
}
