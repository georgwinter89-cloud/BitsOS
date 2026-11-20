use crate::chip::*;

#[test]
fn xid_is_taken_as_linux_takes_it() {
    assert_eq!(xid_of(0x64a0_0000), 0x64a);
    // Bits 4 and 5 of the field are outside Linux's 0xfcf.
    assert_eq!(xid_of(0x64a0_0000 | (0x30 << 20)), 0x64a);
    assert_eq!(xid_of(0x0300_0700), 0x000);
}

#[test]
fn the_gaming_pc_card_wants_the_3_file() {
    assert_eq!(from_xid(0x64a), Ok(&RTL8126A_3));
    assert_eq!(from_xid(0x64a).unwrap().firmware, "rtl_nic/rtl8126a-3.fw");
    assert_eq!(from_xid(0x649), Ok(&RTL8126A_2));
    assert_eq!(from_xid(0x649).unwrap().firmware, "rtl_nic/rtl8126a-2.fw");
}

#[test]
fn the_table_mask_ignores_only_the_top_bit() {
    assert_eq!(from_xid(0xe4a), Ok(&RTL8126A_3));
}

#[test]
fn every_other_chip_is_refused() {
    // 0x6c9 is the 8127A, 0x688 and 0x609 are 8125s (M:107, :123, :129); 0x64b and 0x648 are near misses.
    for xid in [0x6c9, 0x64b, 0x648, 0x688, 0x609, 0x000, 0x7cf] {
        assert_eq!(from_xid(xid), Err(ChipError::Unknown(xid)), "{xid:#x}");
    }
}

#[test]
fn all_ones_means_gone_not_unknown() {
    assert_eq!(from_tx_config(u32::MAX), Err(ChipError::Gone));
    assert_eq!(from_tx_config(0x64a0_0000), Ok(&RTL8126A_3));
}

#[test]
fn the_firmware_fact_belongs_to_the_3_chip() {
    assert_eq!(FIRMWARE_3.name, RTL8126A_3.firmware);
    assert_eq!(FIRMWARE_3.sha256.len(), 64);
    // Not equal: the measured file ends four bytes after its last action, which the format allows (F:69-71).
    assert!(FIRMWARE_3.start as usize + FIRMWARE_3.actions as usize * 4 <= FIRMWARE_3.bytes);
}
