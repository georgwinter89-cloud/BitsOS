use super::fake::{Fake, Op};
use crate::fwscript::Script;
use crate::ocp::{Mdio, OcpError, STD_PHY_BASE};
use crate::phy::*;
use std::vec;

const BMCR: u32 = 0xa400;

#[test]
fn id_is_read_from_the_standard_page() {
    let hw = Fake::new();
    assert_eq!(read_id(&hw, &Mdio::default()), Ok(PHY_ID_8126A));
    assert_eq!(PHY_ID_8126A, 0x001c_c862);
}

#[test]
fn settings_land_on_the_addresses_linux_reaches() {
    let hw = Fake::new();
    {
        let mut s = hw.s.borrow_mut();
        s.phy_ocp.insert(0xa442, 0x0000);
        s.phy_ocp.insert(0xa5b4, 0xffff);
        s.phy_ocp.insert(0xa430, 0xffff);
        s.phy_ocp.insert(0xa6d8, 0xffff);
        s.phy_ocp.insert(0xa428, 0xffff);
        s.phy_ocp.insert(0xa4a2, 0xffff);
    }
    let mut mdio = Mdio::default();
    bring_up(&hw, &mut mdio, None).unwrap();
    let s = hw.s.borrow();
    assert_eq!(s.phy_ocp[&0xa442], 1 << 11);
    assert_eq!(s.phy_ocp[&0xa5b4], 0x7fff);
    assert_eq!(s.phy_ocp[&0xa430], 0xfffb);
    assert_eq!(s.phy_ocp[&0xa6d8], 0xffef);
    assert_eq!(s.phy_ocp[&0xa428], 0xff7f);
    assert_eq!(s.phy_ocp[&0xa4a2], 0xfdff);
}

#[test]
fn another_phy_gets_nothing_written() {
    let hw = Fake::new();
    hw.s.borrow_mut().phy_ocp.insert(0xa406, 0xc800);
    let mut mdio = Mdio::default();
    assert_eq!(bring_up(&hw, &mut mdio, None), Err(PhyError::WrongId(0x001c_c800)));
    let writes = hw.ops().into_iter().filter(|o| matches!(o, Op::W32(_, v) if v & 0x8000_0000 != 0)).count();
    assert_eq!(writes, 0);
}

#[test]
fn resume_clears_power_down_and_reset_clears_isolate() {
    let hw = Fake::new();
    hw.s.borrow_mut().phy_ocp.insert(BMCR, 0x0800 | 0x0400);
    let mut mdio = Mdio::default();
    bring_up(&hw, &mut mdio, None).unwrap();
    let bmcr = hw.s.borrow().phy_ocp[&BMCR];
    assert_eq!(bmcr & 0x0800, 0, "power down");
    assert_eq!(bmcr & 0x0400, 0, "isolate");
    assert_ne!(bmcr & 0x0200, 0, "autonegotiation restarted");
}

fn file_writing_page_a43_reg_10() -> vec::Vec<u8> {
    let mut f = vec![0u8; 48];
    f[36..40].copy_from_slice(&48u32.to_le_bytes());
    f[40..44].copy_from_slice(&2u32.to_le_bytes());
    for a in [0x801f_0a43u32, 0x8010_0127] {
        f.extend_from_slice(&a.to_le_bytes());
    }
    let sum = f.iter().fold(0u8, |s, b| s.wrapping_add(*b));
    f[44] = sum.wrapping_neg();
    f
}

#[test]
fn the_firmware_runs_before_the_settings_and_the_page_is_back_after_it() {
    let f = file_writing_page_a43_reg_10();
    let script = Script::parse(&f).unwrap();
    let hw = Fake::new();
    let mut mdio = Mdio::default();
    let up = bring_up(&hw, &mut mdio, Some(&script)).unwrap();
    assert_eq!(up.id, PHY_ID_8126A);
    assert_eq!(up.played.unwrap().writes, 2);
    // The script sets bit 2 and the ALDPS setting clears it, so this value means the setting came second.
    assert_eq!(hw.s.borrow().phy_ocp[&0xa430], 0x0123);
    assert_eq!(mdio.base(), STD_PHY_BASE);
}

#[test]
fn no_script_means_no_played_report() {
    let hw = Fake::new();
    let mut mdio = Mdio::default();
    assert_eq!(bring_up(&hw, &mut mdio, None).unwrap().played, None);
}

#[test]
fn a_stuck_reset_is_reported_after_12_looks_of_50_ms() {
    let hw = Fake::new();
    hw.s.borrow_mut().stuck_bmcr = true;
    let mut mdio = Mdio::default();
    assert_eq!(soft_reset(&hw, &mut mdio), Err(PhyError::ResetStuck));
    assert_eq!(hw.ops().iter().filter(|o| **o == Op::Delay(50_000)).count(), 12);
}

#[test]
fn a_dead_window_is_reported_as_such() {
    let hw = Fake::new();
    hw.s.borrow_mut().stuck_phy = true;
    let mut mdio = Mdio::default();
    assert!(matches!(bring_up(&hw, &mut mdio, None), Err(PhyError::Ocp(OcpError::Timeout(_)))));
}

#[test]
fn speed_table() {
    assert_eq!(speed_mbps(0x0000), Some(10));
    assert_eq!(speed_mbps(0x0010), Some(100));
    assert_eq!(speed_mbps(0x0020), Some(1000));
    assert_eq!(speed_mbps(0x0210), Some(2500));
    assert_eq!(speed_mbps(0x0220), Some(5000));
    assert_eq!(speed_mbps(0x0200), None, "10 Gb/s in the table of 6.6, not on this chip");
    assert_eq!(speed_mbps(0x0030), None);
    assert_eq!(speed_mbps(0xf9cf | 0x0020), Some(1000), "bits outside the mask do not matter");
}

fn set(hw: &Fake, reg: u32, v: u16) {
    hw.s.borrow_mut().phy_ocp.insert(reg, v);
}

fn phy_writes(hw: &Fake) -> std::vec::Vec<(u32, u16)> {
    hw.ops().into_iter().filter_map(|o| match o {
        Op::W32(crate::regs::GPHY_OCP, v) if v & 0x8000_0000 != 0 => Some((((v >> 16) & 0x7fff) << 1, v as u16)),
        _ => None,
    }).collect()
}

#[test]
fn link_follows_phylib() {
    let hw = Fake::new();
    let mdio = Mdio::default();
    // Down: the latched bit is read a second time.
    assert_eq!(read_link(&hw, &mdio), Ok(Link { up: false, speed_mbps: None, full_duplex: false }));
    assert_eq!(hw.ops().len(), 3, "BMCR, BMSR, BMSR");
    // Link bit without "autonegotiation complete" is no link.
    set(&hw, 0xa402, 0x0004);
    assert!(!read_link(&hw, &mdio).unwrap().up);
    set(&hw, 0xa402, 0x0024);
    set(&hw, 0xa434, 0x0028);
    assert_eq!(read_link(&hw, &mdio), Ok(Link { up: true, speed_mbps: Some(1000), full_duplex: true }));
    set(&hw, 0xa434, 0x0210);
    assert_eq!(read_link(&hw, &mdio), Ok(Link { up: true, speed_mbps: Some(2500), full_duplex: false }));
    // While autonegotiation restarts the status is not believed.
    set(&hw, 0xa400, 0x1200);
    assert!(!read_link(&hw, &mdio).unwrap().up);
    assert!(phy_writes(&hw).is_empty(), "a link look writes nothing");
}

#[test]
fn abilities_come_from_three_registers() {
    let hw = Fake::new();
    let mdio = Mdio::default();
    assert_eq!(abilities(&hw, &mdio), Ok(0));
    set(&hw, 0xa616, 0x6000);
    set(&hw, 0xa402, 0x7909);
    set(&hw, 0xa41e, 0x2000);
    assert_eq!(abilities(&hw, &mdio), Ok(mode::GAMING_PC));
    // Without the "extended status" bit register 15 is not believed.
    set(&hw, 0xa402, 0x7809);
    assert_eq!(abilities(&hw, &mdio).unwrap() & mode::FULL_1000, 0);
    set(&hw, 0xa402, 0x7909);
    set(&hw, 0xa41e, 0x3000);
    assert_ne!(abilities(&hw, &mdio).unwrap() & mode::HALF_1000, 0);
}

#[test]
fn advertisement_values_are_linuxs() {
    // What `ethtool` shows on the Gaming-PC: 10 and 100 half and full, 1000 full, 2500 full, 5000 full, both pauses.
    let a = advert(mode::GAMING_PC);
    assert_eq!(a, Advert { adv: 0x0de0, ctrl1000: 0x0200, mgbt: 0x0180 });
    assert_eq!(advert(capped(mode::GAMING_PC, 1000)), Advert { adv: 0x0de0, ctrl1000: 0x0200, mgbt: 0 });
    assert_eq!(advert(capped(mode::GAMING_PC, 2500)).mgbt, 0x0080);
    assert_eq!(advert(capped(mode::GAMING_PC, 100)), Advert { adv: 0x0de0, ctrl1000: 0, mgbt: 0 });
    assert_eq!(advert(capped(mode::GAMING_PC, 10)).adv, 0x0c60);
    assert_eq!(advert(mode::HALF_1000).ctrl1000, 0x0100);
}

#[test]
fn config_aneg_writes_what_changes_and_restarts() {
    let hw = Fake::new();
    let mut mdio = Mdio::default();
    // As after a reset: EEE advertised for every mode, 10/100 advertised, 1000 half and full, no multi-gigabit.
    for (reg, v) in [(0xa5c4, 0x0006), (0xa6ec, 0x0003), (0xa5d0, 0x0006), (0xa6d4, 0x0003), (0xa408, 0x01e1), (0xa412, 0x0300)] {
        set(&hw, reg, v);
    }
    set(&hw, 0xa402, 0x7909);
    set(&hw, 0xa400, 0x1000);
    config_aneg(&hw, &mut mdio, advert(mode::GAMING_PC)).unwrap();
    assert_eq!(
        phy_writes(&hw),
        [(0xa5d4, 0x0180), (0xa5d0, 0x0000), (0xa6d4, 0x0000), (0xa408, 0x0de1), (0xa412, 0x0200), (0xa400, 0x1200)],
        "multi-gigabit, EEE off twice, MII_ADVERTISE with the selector kept, 1000 full only, restart"
    );
    // Nothing differs any more and autonegotiation is on: nothing is written, not even a restart.
    let before = hw.ops().len();
    set(&hw, 0xa400, 0x1000);
    config_aneg(&hw, &mut mdio, advert(mode::GAMING_PC)).unwrap();
    assert!(phy_writes(&hw).len() == 6 && hw.ops().len() > before);
    // Autonegotiation off or the PHY isolated: restarted though nothing changed.
    set(&hw, 0xa400, 0x0400);
    config_aneg(&hw, &mut mdio, advert(mode::GAMING_PC)).unwrap();
    assert_eq!(phy_writes(&hw).last(), Some(&(0xa400, 0x1200)));
}

#[test]
fn a_phy_without_eee_gets_no_eee_write() {
    let hw = Fake::new();
    let mut mdio = Mdio::default();
    set(&hw, 0xa5d0, 0x0006);
    config_aneg(&hw, &mut mdio, advert(mode::GAMING_PC)).unwrap();
    assert!(phy_writes(&hw).iter().all(|(r, _)| *r != 0xa5d0 && *r != 0xa6d4));
}

#[test]
fn settings_that_already_stand_are_not_written_again() {
    let hw = Fake::new();
    let mut mdio = Mdio::default();
    bring_up(&hw, &mut mdio, None).unwrap();
    let first = phy_writes(&hw).len();
    // BMCR for the resume is unchanged now (no power-down bit), the six settings stand: only the reset is written.
    bring_up(&hw, &mut mdio, None).unwrap();
    assert_eq!(phy_writes(&hw).len() - first, 1);
    assert_eq!(phy_writes(&hw).last().unwrap().0, 0xa400);
}

#[test]
fn after_the_firmware_the_reset_bit_is_awaited_as_linux_does() {
    let f = file_writing_page_a43_reg_10();
    let script = Script::parse(&f).unwrap();
    let hw = Fake::new();
    let mut mdio = Mdio::default();
    bring_up(&hw, &mut mdio, Some(&script)).unwrap();
    let ops = hw.ops();
    let last_fw = ops.iter().rposition(|o| matches!(o, Op::W32(_, v) if v & 0x8000_ffff == 0x8000_0127)).unwrap();
    assert_eq!(ops[last_fw + 1], Op::Delay(50_000), "the 50 ms look comes before the first setting");
}
