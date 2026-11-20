use super::fake::{Fake, Op};
use crate::ocp::*;
use crate::regs::{GPHY_OCP, OCPDR};

#[test]
fn window_packing() {
    assert_eq!(write_cmd(0xa430, 0x1234), 0xd218_1234);
    assert_eq!(read_cmd(0xa430), 0x5218_0000);
    assert_eq!(write_cmd(0xe098, 0xc302) & 0xffff, 0xc302);
}

#[test]
fn odd_and_wide_addresses_are_refused_and_nothing_is_written() {
    let hw = Fake::new();
    for bad in [0xa431, 0x1_0000, -2] {
        assert!(!addr_ok(bad));
        assert_eq!(mac_write(&hw, bad, 1), Err(OcpError::BadAddress(bad)));
        assert_eq!(mac_read(&hw, bad), Err(OcpError::BadAddress(bad)));
        assert_eq!(phy_ocp_write(&hw, bad, 1), Err(OcpError::BadAddress(bad)));
        assert_eq!(phy_ocp_read(&hw, bad), Err(OcpError::BadAddress(bad)));
    }
    assert!(hw.ops().is_empty());
    assert!(addr_ok(0xfffe));
}

#[test]
fn page_mapping_of_the_pages_this_chip_uses() {
    assert_eq!(phy_addr(page_base(0x0a43), 0x10), 0xa430);
    assert_eq!(phy_addr(page_base(0x0a43), 0x12), 0xa434);
    assert_eq!(phy_addr(page_base(0x0a44), 0x11), 0xa442);
    assert_eq!(phy_addr(page_base(0x0a5b), 0x12), 0xa5b4);
    assert_eq!(phy_addr(page_base(0x0a6d), 0x14), 0xa6d8);
    assert_eq!(phy_addr(page_base(0x0a42), 0x14), 0xa428);
    assert_eq!(phy_addr(page_base(0x0a4a), 0x11), 0xa4a2);
}

#[test]
fn page_zero_and_page_a40_are_the_standard_page_without_the_offset() {
    assert_eq!(page_base(0), STD_PHY_BASE);
    assert_eq!(phy_addr(page_base(0), 2), 0xa404);
    assert_eq!(phy_addr(page_base(0x0a40), 2), 0xa404);
}

#[test]
fn a_low_reg_on_another_page_lands_before_the_page_as_in_linux() {
    assert_eq!(phy_addr(page_base(0x0a43), 0), 0xa410);
}

#[test]
fn page_select_writes_nothing_to_the_chip() {
    let hw = Fake::new();
    let mut mdio = Mdio::default();
    mdio.phy_write(&hw, 0x1f, 0x0a43).unwrap();
    assert_eq!(mdio.base(), 0xa430);
    assert_eq!(mdio.phy_read(&hw, 0x1f), Ok(0x0a43));
    assert!(hw.ops().is_empty());
    mdio.phy_write(&hw, 0x1f, 0).unwrap();
    assert_eq!(mdio.phy_read(&hw, 0x1f), Ok(0));
}

#[test]
fn mcu_mapping_has_no_offset_and_no_doubling() {
    let hw = Fake::new();
    let mut mdio = Mdio::default();
    mdio.mcu_write(&hw, 0x1f, 0x0e40).unwrap();
    assert_eq!(mdio.base(), 0xe400);
    mdio.mcu_write(&hw, 0x12, 0xabcd).unwrap();
    assert_eq!(hw.ops(), [Op::W32(OCPDR, write_cmd(0xe412, 0xabcd))]);
    assert_eq!(mdio.mcu_read(&hw, 0x12), Ok(0xabcd));
    assert_eq!(mdio.mcu_write(&hw, 0x13, 1), Err(OcpError::BadAddress(0xe413)));
}

#[test]
fn mcu_page_zero_means_address_zero_not_the_standard_page() {
    let hw = Fake::new();
    let mut mdio = Mdio::default();
    mdio.mcu_write(&hw, 0x1f, 0).unwrap();
    assert_eq!(mdio.base(), 0);
    mdio.reset_base();
    assert_eq!(mdio.base(), STD_PHY_BASE);
}

#[test]
fn phy_and_mcu_share_one_page_register() {
    let hw = Fake::new();
    let mut mdio = Mdio::default();
    mdio.mcu_write(&hw, 0x1f, 0x0a43).unwrap();
    mdio.phy_write(&hw, 0x10, 7).unwrap();
    assert_eq!(hw.ops(), [Op::W32(GPHY_OCP, write_cmd(0xa430, 7))]);
}

#[test]
fn modify_paged_clears_sets_and_puts_the_page_back() {
    let hw = Fake::new();
    hw.s.borrow_mut().phy_ocp.insert(0xa430, 0xff00);
    let mut mdio = Mdio::default();
    mdio.phy_write(&hw, 0x1f, 0x0a44).unwrap();
    assert_eq!(mdio.modify_paged(&hw, 0x0a43, 0x10, 0x0f00, 0x0003), Ok(0xff00));
    assert_eq!(hw.s.borrow().phy_ocp[&0xa430], 0xf003);
    assert_eq!(mdio.base(), 0xa440);
}

fn looks(hw: &Fake) -> usize {
    hw.ops().iter().filter(|o| **o == Op::Delay(25)).count()
}

#[test]
fn a_stuck_phy_window_is_a_timeout_after_twenty_milliseconds() {
    let hw = Fake::new();
    hw.s.borrow_mut().stuck_phy = true;
    assert_eq!(phy_ocp_read(&hw, 0xa400), Err(OcpError::Timeout(0xa400)));
    assert_eq!(looks(&hw), 800, "Linux's 25 us step, 80 times its ten looks");
    assert_eq!(phy_ocp_write(&hw, 0xa400, 1), Err(OcpError::Timeout(0xa400)));
    assert_eq!(looks(&hw), 1600);
    assert_eq!(hw.waits.max_us(), 20_000, "a dead window shows in the kept maximum");
}

#[test]
fn a_window_that_answers_at_once_costs_no_wait() {
    let hw = Fake::new();
    phy_ocp_write(&hw, 0xa430, 7).unwrap();
    assert_eq!(phy_ocp_read(&hw, 0xa430), Ok(7));
    assert_eq!(looks(&hw), 0);
    assert_eq!((hw.waits.max_us(), hw.waits.over_linux()), (0, 0));
}

#[test]
fn a_window_slower_than_linuxs_ten_exact_looks_still_answers_and_the_wait_is_kept() {
    // 30 looks = 750 us: inside what Linux's sleeping looks cover, outside ten exact steps of 25 us.
    let hw = Fake::new();
    hw.s.borrow_mut().slow_phy = 30;
    phy_ocp_write(&hw, 0xa430, 0x1234).unwrap();
    assert_eq!(phy_ocp_read(&hw, 0xa430), Ok(0x1234));
    assert_eq!(looks(&hw), 60);
    assert_eq!((hw.waits.max_us(), hw.waits.over_linux()), (750, 2));
    // A slow access below Linux's numbers is kept as the maximum only when it is the longest.
    hw.s.borrow_mut().slow_phy = 3;
    phy_ocp_write(&hw, 0xa430, 1).unwrap();
    assert_eq!((hw.waits.max_us(), hw.waits.over_linux()), (750, 2));
}

#[test]
fn a_link_look_gives_up_after_two_milliseconds() {
    let hw = Fake::new();
    hw.s.borrow_mut().stuck_phy = true;
    assert_eq!(Mdio::for_look().phy_read(&hw, 1), Err(OcpError::Timeout(0xa402)));
    assert_eq!(looks(&hw), 80);
    // And reaches a window that takes 750 us.
    let hw = Fake::new();
    hw.s.borrow_mut().slow_phy = 30;
    hw.s.borrow_mut().phy_ocp.insert(0xa402, 0x7969);
    assert_eq!(Mdio::for_look().phy_read(&hw, 1), Ok(0x7969));
}

#[test]
fn modify_keeps_the_page_when_the_window_is_stuck() {
    let hw = Fake::new();
    hw.s.borrow_mut().stuck_phy = true;
    let mut mdio = Mdio::default();
    assert!(mdio.modify_paged(&hw, 0x0a43, 0x10, 1, 0).is_err());
    assert_eq!(mdio.base(), STD_PHY_BASE);
}

#[test]
fn mac_modify_is_read_then_write() {
    let hw = Fake::new();
    hw.s.borrow_mut().mac_ocp.insert(0xe614, 0x0755);
    mac_modify(&hw, 0xe614, 0x0700, 0x0400).unwrap();
    assert_eq!(hw.s.borrow().mac_ocp[&0xe614], 0x0455);
    assert_eq!(
        hw.ops(),
        [Op::W32(OCPDR, read_cmd(0xe614)), Op::W32(OCPDR, write_cmd(0xe614, 0x0455))]
    );
}
