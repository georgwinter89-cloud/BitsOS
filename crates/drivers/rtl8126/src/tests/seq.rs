use super::fake::{Fake, Op};
use crate::desc::{RX_BUF_SIZE, RX_MAX_SIZE};
use crate::regs::*;
use crate::seq::Step::*;
use crate::seq::*;
use std::vec::Vec;

const TX: u64 = 0x12_3456_7800;
const RX: u64 = 0x12_3456_8900;

fn ctx() -> StartCtx {
    StartCtx::new(0x2063, TX, RX).unwrap()
}

fn all_steps() -> Vec<Step> {
    [PROBE, HW_START, CLEANUP, STOP].iter().flat_map(|s| s.iter().flat_map(|p| p.iter().copied())).collect()
}

/// Taken with grep from `rtl_hw_start_8125_common` (M:3861-3928), leaving out only the branches for other chips:
/// a second, mechanical transcription for the hand-typed table to be compared with.
const LINUX_COMMON_OCP: [(u16, u16, u16); 23] = [
    (0xd40a, 0x0010, 0x0000),
    (0xd3e2, 0x0fff, 0x03a9),
    (0xd3e4, 0x00ff, 0x0000),
    (0xe860, 0x0000, 0x0080),
    (0xeb58, 0x0001, 0x0000),
    (0xe614, 0x0700, 0x0400),
    (0xe63e, 0x0c30, 0x0020),
    (0xc0b4, 0x0000, 0x000c),
    (0xeb6a, 0x00ff, 0x0033),
    (0xeb50, 0x03e0, 0x0040),
    (0xe056, 0x00f0, 0x0000),
    (0xe040, 0x1000, 0x0000),
    (0xea1c, 0x0003, 0x0001),
    (0xea1c, 0x0300, 0x0000),
    (0xe0c0, 0x4f0f, 0x4403),
    (0xe052, 0x0080, 0x0068),
    (0xd430, 0x0fff, 0x047f),
    (0xea1c, 0x0004, 0x0000),
    (0xeb54, 0x0000, 0x0001),
    (0xeb54, 0x0001, 0x0000),
    (0xc0ac, 0x0000, 0x1f80),
    (0xe092, 0x00ff, 0x0000),
    (0xea84, 0x0000, 0x0003),
];

#[test]
fn common_table_matches_the_mechanical_transcription() {
    let ocp: Vec<_> = START_COMMON
        .iter()
        .filter_map(|s| if let OcpMod(a, b, c) = s { Some((*a, *b, *c)) } else { None })
        .collect();
    assert_eq!(ocp, LINUX_COMMON_OCP[..20]);
    let plain: Vec<_> =
        START_COMMON.iter().filter_map(|s| if let Ocp(a, b) = s { Some((*a, *b)) } else { None }).collect();
    assert_eq!(plain, [(0xc140, 0xffff), (0xc142, 0xffff), (0xe098, 0xc302)]);
}

#[test]
fn the_ocp_values_outside_common_are_played_too() {
    let steps = all_steps();
    for (a, b, c) in &LINUX_COMMON_OCP[20..] {
        assert!(steps.contains(&OcpMod(*a, *b, *c)), "{a:#x}");
    }
}

#[test]
fn common_ends_by_opening_the_gate_after_the_wait() {
    let n = START_COMMON.len();
    assert_eq!(START_COMMON[n - 2], Wait { cond: Cond::OcpE00eClear, us: 1000, tries: 10 });
    assert_eq!(START_COMMON[n - 1], Mod32 { off: MISC, clear: RXDV_GATED_EN, set: 0 });
}

#[test]
fn every_table_write_is_on_the_allow_list_with_its_width() {
    for step in all_steps() {
        let (off, width) = match step {
            Wr8(off, _) | Mod8 { off, .. } => (off, Width::B1),
            Wr16(off, _) | Mod16 { off, .. } => (off, Width::B2),
            Wr32(off, _) | Mod32 { off, .. } => (off, Width::B4),
            CpCmd => (CPLUS_CMD, Width::B2),
            _ => continue,
        };
        assert!(write_allowed(off, width), "{off:#x}");
    }
}

#[test]
fn the_allow_list_and_the_never_list_do_not_touch() {
    for (off, width) in WRITES {
        assert!(!NEVER_WRITTEN.contains(off), "{off:#x}");
        assert!(off.is_multiple_of(width.bytes()), "{off:#x} is not aligned to its width");
    }
    for off in NEVER_WRITTEN {
        for width in [Width::B1, Width::B2, Width::B4] {
            assert!(!write_allowed(*off, width), "{off:#x}");
        }
    }
}

#[test]
fn no_eeprom_path_exists() {
    // EFUSEAR, FLASH, CSI and ERI are named on the never list; Cfg9346 has only its two harmless values.
    for off in [0xdc, 0x30, 0x64, 0x68, 0x70, 0x74] {
        assert!(NEVER_WRITTEN.contains(&off));
    }
    for step in all_steps() {
        if let Wr8(CFG9346, val) = step {
            assert!(val == CFG9346_LOCK || val == CFG9346_UNLOCK);
        }
        assert!(!matches!(step, Mod8 { off: CFG9346, .. }));
    }
}

#[test]
fn coalesce_range_is_written_as_linux_does_and_nothing_else_in_it_is_allowed() {
    assert!(write_allowed(0xa00, Width::B4));
    assert!(write_allowed(0xa7c, Width::B4));
    assert!(!write_allowed(0xa80, Width::B4));
    assert!(!write_allowed(0xa02, Width::B4));
    assert!(!write_allowed(0xa00, Width::B1));
}

#[test]
fn the_window_covers_every_register() {
    let highest = WRITES.iter().map(|(o, w)| u32::from(*o) + u32::from(w.bytes())).max().unwrap();
    assert_eq!(highest, MIN_BAR_SIZE);
    const {
        assert!(MAC0_BKP as u32 + 6 <= MIN_BAR_SIZE);
        assert!(MIN_BAR_SIZE <= 64 * 1024, "BAR2 on the Gaming-PC is 64 KiB");
    }
}

#[test]
fn rx_buffer_and_filter_agree() {
    assert_eq!(u32::from(RX_MAX_SIZE), RX_BUF_SIZE + 1);
    assert_eq!(RX_MAX_SIZE_VALUE, 16384);
    assert_eq!(RX_BUF_SIZE, 16383);
    assert!(HW_START.iter().flat_map(|p| p.iter()).any(|s| *s == Wr16(RX_MAX_SIZE_REG, 16384)));
}

const RX_MAX_SIZE_REG: u16 = crate::regs::RX_MAX_SIZE;

#[test]
fn hw_start_ring_addresses_come_before_the_enable_high_before_low() {
    let hw = Fake::new();
    play(&hw, HW_START, Some(&ctx())).unwrap();
    let tx_hi = hw.position(Op::W32(TX_DESC_HI, (TX >> 32) as u32));
    let tx_lo = hw.position(Op::W32(TX_DESC_LO, TX as u32));
    let rx_hi = hw.position(Op::W32(RX_DESC_HI, (RX >> 32) as u32));
    let rx_lo = hw.position(Op::W32(RX_DESC_LO, RX as u32));
    let enable = hw.position(Op::W8(CHIP_CMD, CMD_TX_ENB | CMD_RX_ENB));
    assert!(tx_hi < tx_lo && tx_lo < rx_hi && rx_hi < rx_lo && rx_lo < enable);
    let accept = hw.ops().iter().rposition(|o| matches!(o, Op::W32(RX_CONFIG, _))).unwrap();
    assert!(enable < accept, "frames are accepted only after the rings are set and the chip is on");
}

#[test]
fn hw_start_unlock_and_lock_are_paired_and_end_locked() {
    let hw = Fake::new();
    play(&hw, HW_START, Some(&ctx())).unwrap();
    let mut unlocked = false;
    let mut pairs = 0;
    for op in hw.ops() {
        match op {
            Op::W8(CFG9346, CFG9346_UNLOCK) => {
                assert!(!unlocked);
                unlocked = true;
            }
            Op::W8(CFG9346, CFG9346_LOCK) => {
                assert!(unlocked);
                unlocked = false;
                pairs += 1;
            }
            _ => {}
        }
    }
    assert!(!unlocked);
    assert_eq!(pairs, 2);
}

#[test]
fn hw_start_final_values() {
    let hw = Fake::new();
    play(&hw, HW_START, Some(&ctx())).unwrap();
    use crate::hw::Hw;
    assert_eq!(hw.rd32(TX_CONFIG), 0x0300_0700);
    assert_eq!(hw.rd32(RX_CONFIG), (8 << 27) | (7 << 8) | (1 << 11) | 0x0e);
    assert_eq!(hw.rd16(RX_MAX_SIZE_REG), 16384);
    assert_eq!(hw.rd16(EEE_TXIDLE_TIMER), 1546);
    assert_eq!(hw.rd16(UNNAMED_382), 0x221b);
    assert_eq!(hw.rd32(INTR_MASK), 0, "polled: no interrupt is enabled by the start");
    assert_eq!(hw.rd32(MAR0), u32::MAX);
    assert_eq!(hw.rd32(MAR0 + 4), u32::MAX);
    assert_eq!(hw.rd8(CHIP_CMD), CMD_TX_ENB | CMD_RX_ENB);
    assert_eq!(hw.rd32(MISC) & RXDV_GATED_EN, 0);
    assert_eq!(hw.rd16(CPLUS_CMD), 0x2043, "RX checksum off, the rest as found");
    assert_eq!(hw.s.borrow().cfg[&0x890], 0xfe);
    assert_eq!(hw.s.borrow().cfg[&0x70f], 0x27);
    assert_eq!(hw.s.borrow().mac_ocp[&0xe614] & 0x0700, 0x0400);
    assert_eq!(hw.s.borrow().mac_ocp[&0xea84] & 0b11, 0b11);
}

#[test]
fn hw_start_clears_exactly_the_coalesce_range() {
    let hw = Fake::new();
    play(&hw, HW_START, Some(&ctx())).unwrap();
    let cleared: Vec<_> = hw
        .ops()
        .into_iter()
        .filter_map(|o| if let Op::W32(off, 0) = o { Some(off) } else { None })
        .filter(|off| (0x900..0xb00).contains(off))
        .collect();
    assert_eq!(cleared.len(), 32);
    assert_eq!(cleared[0], 0xa00);
    assert_eq!(cleared[31], 0xa7c);
}

#[test]
fn aspm_is_off_at_the_end_and_the_clkreq_bit_never_comes_on() {
    let hw = Fake::new();
    hw.poke8(CONFIG5, ASPM_EN);
    hw.poke8(INT_CFG0, INT_CFG0_CLKREQEN);
    play(&hw, HW_START, Some(&ctx())).unwrap();
    use crate::hw::Hw;
    assert_eq!(hw.rd8(CONFIG5) & ASPM_EN, 0);
    assert_eq!(hw.rd8(INT_CFG0) & INT_CFG0_CLKREQEN, 0);
    for op in hw.ops() {
        if let Op::W8(off, val) = op {
            assert!(!(off == CONFIG5 && val & ASPM_EN != 0));
            assert!(!(off == INT_CFG0 && val & INT_CFG0_CLKREQEN != 0));
        }
    }
}

#[test]
fn the_rings_only_come_from_a_checked_context() {
    assert_eq!(StartCtx::new(0, 0, RX), Err(RingError::Zero));
    assert_eq!(StartCtx::new(0, TX, 0), Err(RingError::Zero));
    assert_eq!(StartCtx::new(0, TX + 1, RX), Err(RingError::Misaligned(TX + 1)));
    assert_eq!(StartCtx::new(0, TX, RX + 0x80), Err(RingError::Misaligned(RX + 0x80)));
    let hw = Fake::new();
    assert_eq!(play(&hw, HW_START, None), Err(SeqError::NoStartContext));
    assert!(!hw.ops().iter().any(|o| matches!(o, Op::W8(CHIP_CMD, _))), "stopped before the chip was enabled");
}

#[test]
fn cp_cmd_keeps_only_the_masked_bits_and_no_checksum() {
    let hw = Fake::new();
    let ctx = StartCtx::new(0xffff, TX, RX).unwrap();
    play(&hw, HW_START, Some(&ctx)).unwrap();
    use crate::hw::Hw;
    assert_eq!(hw.rd16(CPLUS_CMD), 0x2043);
}

#[test]
fn no_config_space_is_an_error_not_a_fallback() {
    let hw = Fake::new();
    hw.s.borrow_mut().no_cfg = true;
    assert_eq!(play(&hw, HW_START, Some(&ctx())), Err(SeqError::ConfigSpace(0x890)));
}

#[test]
fn a_refused_read_request_is_reported() {
    let hw = Fake::new();
    hw.s.borrow_mut().no_read_rq = true;
    assert_eq!(play(&hw, HW_START, Some(&ctx())), Err(SeqError::ReadRequest));
}

#[test]
fn e00e_wait_is_ten_tries_of_one_millisecond() {
    let hw = Fake::new();
    hw.s.borrow_mut().mac_ocp.insert(0xe00e, 1 << 13);
    assert_eq!(play(&hw, HW_START, Some(&ctx())), Err(SeqError::Timeout(Cond::OcpE00eClear)));
    assert_eq!(hw.ops().iter().filter(|o| **o == Op::Delay(1000)).count(), 10);
}

#[test]
fn probe_order() {
    let hw = Fake::new();
    play(&hw, PROBE, None).unwrap();
    let gate = hw.position(Op::W32(MISC, RXDV_GATED_EN));
    let wait = hw.position(Op::Delay(2000));
    let reset = hw.position(Op::W8(CHIP_CMD, CMD_RESET));
    let mask = hw.position(Op::W32(INTR_MASK, 0));
    assert!(mask < gate && gate < wait && wait < reset);
    use crate::hw::Hw;
    assert_eq!(hw.rd8(MCU) & NOW_IS_OOB, 0);
    assert_eq!(hw.s.borrow().mac_ocp[&0xc01e], 0x5555);
    assert_eq!(hw.s.borrow().mac_ocp[&0xc0aa], 0x07d0);
    assert_eq!(hw.s.borrow().mac_ocp[&0xc0a6], 0x0150);
}

#[test]
fn a_reset_that_never_ends_stops_the_probe_with_linuxs_wait() {
    let hw = Fake::new();
    hw.s.borrow_mut().stuck_reset = true;
    assert_eq!(play(&hw, PROBE, None), Err(SeqError::Timeout(Cond::ResetDone)));
    assert_eq!(hw.ops().iter().filter(|o| **o == Op::Delay(100)).count(), 100);
}

#[test]
fn empty_fifo_waits_use_both_conditions() {
    let hw = Fake::new();
    hw.poke16(INTR_MITIGATE, 0x0003);
    assert_eq!(play(&hw, &[RXDV_GATE_ON], None), Err(SeqError::Timeout(Cond::RxTxEmpty2)));
    let hw = Fake::new();
    hw.poke8(MCU, RXTX_EMPTY & (RXTX_EMPTY >> 1));
    assert_eq!(play(&hw, &[RXDV_GATE_ON], None), Err(SeqError::Timeout(Cond::RxTxEmpty)));
}

#[test]
fn cleanup_masks_first_closes_rx_then_gates_then_resets() {
    let hw = Fake::new();
    play(&hw, CLEANUP, None).unwrap();
    let mask = hw.position(Op::W32(INTR_MASK, 0));
    let close = hw.ops().iter().position(|o| matches!(o, Op::W32(RX_CONFIG, _))).unwrap();
    let gate = hw.position(Op::W32(MISC, RXDV_GATED_EN));
    let reset = hw.position(Op::W8(CHIP_CMD, CMD_RESET));
    assert!(mask < close && close < gate && gate < reset);
    assert_eq!(hw.ops().iter().filter(|o| **o == Op::Delay(2000)).count(), 2);
}

#[test]
fn play_all_goes_on_after_a_failure_and_counts_it() {
    let hw = Fake::new();
    hw.s.borrow_mut().stuck_reset = true;
    assert_eq!(play_all(&hw, STOP), 1);
    assert_eq!(hw.s.borrow().mac_ocp[&0xc0ac] & 0x1f80, 0, "the step after the failure still ran");
}

#[test]
fn doorbell_is_the_16_bit_write_of_bit_0() {
    let hw = Fake::new();
    doorbell(&hw);
    assert_eq!(hw.ops(), [Op::W16(TX_POLL, 1)]);
}
