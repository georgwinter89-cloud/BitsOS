use super::fake::{Fake, Op, TEST_MAC};
use crate::bringup::*;
use crate::chip::{ChipError, RTL8126A_3};
use crate::hw::Hw;
use crate::regs::*;
use crate::seq::{Cond, SeqError, StartCtx};
use core::cell::Cell;

fn index(stage: Stage) -> usize {
    BRING_UP.iter().position(|s| *s == stage).unwrap()
}

#[test]
fn the_order_has_each_stage_once() {
    for (i, a) in BRING_UP.iter().enumerate() {
        assert!(!BRING_UP[i + 1..].contains(a), "{a:?}");
        assert!(!TEAR_DOWN.contains(a), "{a:?}");
    }
}

#[test]
fn the_order_keeps_linuxs_dependencies() {
    assert!(index(Stage::MapBar) < index(Stage::Identify));
    assert!(index(Stage::Identify) < index(Stage::ProbeSeq));
    assert!(index(Stage::ProbeSeq) < index(Stage::ReadMac));
    assert!(index(Stage::ReadMac) < index(Stage::SetMac));
    assert!(index(Stage::SetMac) < index(Stage::AllocRings), "no DMA memory before the card is identified");
    assert!(index(Stage::AllocRings) < index(Stage::BusMaster), "no bus master before rings exist");
    assert!(index(Stage::LoadFirmware) < index(Stage::PhyUp));
    assert!(index(Stage::BusMaster) < index(Stage::Cleanup));
    assert!(index(Stage::Cleanup) < index(Stage::ArmRx));
    assert!(index(Stage::ArmRx) < index(Stage::HwStart));
    assert!(index(Stage::HwStart) < index(Stage::Link));
    assert_eq!(TEAR_DOWN, [Stage::BusMasterOff, Stage::Stop]);
}

#[test]
fn glue_is_what_needs_a_kernel() {
    use crate::bringup::By::*;
    for stage in
        [Stage::MapBar, Stage::AllocRings, Stage::LoadFirmware, Stage::BusMaster, Stage::ArmRx, Stage::BusMasterOff]
    {
        assert_eq!(stage.by(), Glue, "{stage:?}");
    }
    for stage in [Stage::Identify, Stage::ProbeSeq, Stage::PhyUp, Stage::HwStart, Stage::Stop] {
        assert_eq!(stage.by(), Core, "{stage:?}");
    }
    for stage in BRING_UP.iter().chain(TEAR_DOWN.iter()) {
        assert!(!stage.linux().is_empty());
    }
}

#[test]
fn probe_of_the_gaming_pc_card() {
    let hw = Fake::new();
    let p = probe(&hw).unwrap();
    assert_eq!(p.chip, &RTL8126A_3);
    assert_eq!(p.mac, TEST_MAC);
    assert_eq!(p.cp_cmd, 0x2061);
    assert_eq!(hw.rd32(INTR_MASK), 0);
    assert_eq!(hw.rd8(CFG9346), CFG9346_LOCK);
}

#[test]
fn probe_refuses_before_writing_when_the_chip_is_unknown_or_gone() {
    for (tx, want) in [(0x6c90_0000u32, ChipError::Unknown(0x6c9)), (u32::MAX, ChipError::Gone)] {
        let hw = Fake::new();
        hw.poke32(TX_CONFIG, tx);
        assert_eq!(probe(&hw), Err(ProbeError::Chip(want)));
        assert!(hw.ops().is_empty());
    }
}

#[test]
fn probe_does_not_turn_on_anything_that_moves_data() {
    let hw = Fake::new();
    probe(&hw).unwrap();
    for op in hw.ops() {
        assert_ne!(op, Op::W8(CHIP_CMD, CMD_TX_ENB | CMD_RX_ENB));
        assert!(!matches!(op, Op::W32(TX_DESC_LO | TX_DESC_HI | RX_DESC_LO | RX_DESC_HI, _)));
        assert!(!matches!(op, Op::W16(TX_POLL, _)));
    }
}

#[test]
fn probe_reports_a_failure_in_the_sequence() {
    let hw = Fake::new();
    hw.s.borrow_mut().stuck_reset = true;
    assert_eq!(probe(&hw), Err(ProbeError::Seq(SeqError::Timeout(Cond::ResetDone))));
}

#[test]
fn set_mac_writes_high_first_with_commits_under_unlock() {
    let hw = Fake::new();
    set_mac(&hw, &[0x02, 0x11, 0x22, 0x33, 0x44, 0x55]);
    assert_eq!(
        hw.ops(),
        [
            Op::W8(CFG9346, CFG9346_UNLOCK),
            Op::W32(MAC4, 0x5544),
            Op::W32(MAC0, 0x3322_1102),
            Op::W8(CFG9346, CFG9346_LOCK),
        ]
    );
}

#[test]
fn the_backup_address_wins_then_the_running_one_and_no_address_is_invented() {
    let hw = Fake::new();
    assert_eq!(read_mac(&hw), Some(TEST_MAC));

    let hw = Fake::new();
    hw.poke32(MAC0_BKP, 0);
    hw.poke16(MAC0_BKP + 4, 0);
    hw.poke32(MAC0, 0x0301_0106);
    hw.poke16(MAC4, 0x0605);
    assert_eq!(read_mac(&hw), Some([0x06, 0x01, 0x01, 0x03, 0x05, 0x06]));

    let hw = Fake::new();
    hw.poke32(MAC0_BKP, 0);
    hw.poke16(MAC0_BKP + 4, 0);
    assert_eq!(read_mac(&hw), None);
    assert_eq!(probe(&hw), Err(ProbeError::NoMac));
}

#[test]
fn unusable_addresses() {
    assert!(!mac_usable(&[0; 6]));
    assert!(!mac_usable(&[0x01, 0, 0, 0, 0, 0]));
    assert!(!mac_usable(&[0xff; 6]));
    assert!(mac_usable(&TEST_MAC));
}

#[test]
fn start_arms_rx_after_the_reset_and_before_the_chip_runs() {
    let hw = Fake::new();
    let ctx = StartCtx::new(0x2061, 0x1000, 0x2000).unwrap();
    let armed_at = Cell::new(None);
    start(&hw, &ctx, || armed_at.set(Some(hw.ops().len()))).unwrap();
    let armed_at = armed_at.get().unwrap();
    let ops = hw.ops();
    let reset = ops.iter().position(|o| *o == Op::W8(CHIP_CMD, CMD_RESET)).unwrap();
    let run = ops.iter().position(|o| *o == Op::W8(CHIP_CMD, CMD_TX_ENB | CMD_RX_ENB)).unwrap();
    assert!(reset < armed_at && armed_at <= run);
    assert_eq!(hw.rd32(TX_DESC_LO), 0x1000);
    assert_eq!(hw.rd32(RX_DESC_LO), 0x2000);
}

#[test]
fn start_does_not_arm_when_the_cleanup_failed() {
    let hw = Fake::new();
    hw.s.borrow_mut().stuck_reset = true;
    let ctx = StartCtx::new(0, 0x1000, 0x2000).unwrap();
    let armed = Cell::new(false);
    assert_eq!(start(&hw, &ctx, || armed.set(true)), Err(SeqError::Timeout(Cond::ResetDone)));
    assert!(!armed.get());
}

#[test]
fn a_stop_after_a_start_leaves_the_chip_quiet() {
    let hw = Fake::new();
    let ctx = StartCtx::new(0, 0x1000, 0x2000).unwrap();
    start(&hw, &ctx, || {}).unwrap();
    assert_eq!(stop(&hw), 0);
    assert_eq!(hw.rd32(INTR_MASK), 0);
    assert_eq!(hw.rd32(RX_CONFIG) & RX_ACCEPT_MASK, 0);
    assert_ne!(hw.rd32(MISC) & RXDV_GATED_EN, 0);
    assert_eq!(hw.s.borrow().mac_ocp[&0xc0ac] & 0x1f80, 0);
}

#[test]
fn a_stop_on_a_stuck_card_still_runs_to_the_end() {
    let hw = Fake::new();
    hw.s.borrow_mut().stuck_reset = true;
    hw.poke16(INTR_MITIGATE, 0);
    hw.poke8(MCU, 0);
    assert_eq!(stop(&hw), 3);
    assert_eq!(hw.rd32(INTR_MASK), 0);
}
