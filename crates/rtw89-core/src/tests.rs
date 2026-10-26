//! Host tests without node: the bus helpers, the op records, the DMA bump
//! allocator, and the bring-up prefix's failure paths on the in-memory bus.

use alloc::vec;
use alloc::vec::Vec;

use crate::bringup::{Bringup, RingConfig, efuse, power, safety};
use crate::bus::{Bus, BusError, Width, field, poll_loop, prep};
use crate::error::Rtw89Error;
use crate::membus::{MemBus, bump_alloc};
use crate::tables::regs::*;
use crate::trace::{Op, OpKind, compare_prefix};

/// A MemBus that answers like the measured chip: cv 1 (SYS_CFG1), XTAL SI data 1 (acv 1),
/// efuse RDY with byte 0xFF, MAC off
fn chip() -> MemBus {
    let mut b = MemBus::new();
    b.cfg.insert(4, 0x0010_0002);
    b.forced.insert(R_AX_SYS_CFG1, prep(B_AX_CHIP_VER_MASK, 1));
    b.forced
        .insert(R_AX_WLAN_XTAL_SI_CTRL, prep(B_AX_WL_XTAL_SI_DATA_MASK, 1));
    b.forced.insert(R_AX_EFUSE_CTRL, B_AX_EF_RDY | 0xff);
    b
}

fn count(b: &MemBus, line: &str) -> usize {
    b.lines().iter().filter(|l| l.as_str() == line).count()
}

#[test]
fn field_prep_lowbit_as_the_kit() {
    assert_eq!(field(0x0000_F123, 0xF000), 0xF);
    assert_eq!(prep(0x0000_0300, 1), 0x100);
    assert_eq!(prep(0, 7), 0, "an empty mask takes nothing");
    assert_eq!(field(0xffff_ffff, 0), 0);
    assert_eq!(prep(0xF000_0000, 0x1F), 0xF000_0000, "cut to the mask");
}

#[test]
fn narrow_rmw_cuts_like_the_js_ops() {
    let mut b = MemBus::new();
    b.set(Width::W8, 0x88, 0x1_01).unwrap();
    b.mask(Width::W16, 0x10, 0x000F_0F00, 0xA).unwrap();
    b.w8(0x20, 0x1ff).unwrap();
    assert_eq!(
        b.lines(),
        vec![
            "rmw8 0x00000088 0x00000000 0x00000001",
            "rmw16 0x00000010 0x00000F00 0x00000A00",
            "wr8 0x00000020 0x00000000 0x000000FF",
        ]
    );
}

#[test]
fn op_record_round_trip_and_hostile_bytes() {
    let op = Op::new(OpKind::Poll, 4, 1000, 0x30, 0x2000_0000, 0x2000_0000);
    let r = op.record();
    assert_eq!(r[0], 4);
    assert_eq!(&r[2..4], &1000u16.to_le_bytes());
    assert_eq!(Op::from_record(&r), Some(op));
    assert_eq!(op.line(), "poll32 0x00000030 0x20000000 0x20000000 1000 ms");
    for k in [0u8, 14, 19, 28, 255] {
        let mut bad = r;
        bad[0] = k;
        assert_eq!(Op::from_record(&bad), None, "kind {k}");
    }
    assert_eq!(
        Op::new(OpKind::Cfgr, 4, 0, 4, 0, 0).line(),
        "cfgr 0x00000004 0x00000000 0x00000000"
    );
    assert_eq!(
        Op::new(OpKind::Delay, 0, 0, 1000, 0, 0).line(),
        "delay 0x000003E8 0x00000000 0x00000000"
    );
    assert!(!Op::new(OpKind::Sync, 0, 0, 0, 0, 0).traced());
}

#[test]
fn compare_prefix_names_the_line() {
    assert_eq!(compare_prefix(&["a", "b"], &["a", "b", "c"]), Ok(()));
    let m = compare_prefix(&["a", "x"], &["a", "b"]).unwrap_err();
    assert_eq!(m.line, 1);
    let m = compare_prefix(&["a", "b"], &["a"]).unwrap_err();
    assert_eq!((m.line, m.golden), (1, None));
}

#[test]
fn bump_alloc_is_the_kit_order_and_never_panics() {
    let mut next = 0x61_0000;
    assert_eq!(bump_alloc(&mut next, 0x70_0000, 128, 128), Some(0x61_0000));
    assert_eq!(bump_alloc(&mut next, 0x70_0000, 8, 8), Some(0x61_0080));
    assert_eq!(bump_alloc(&mut next, 0x70_0000, 1, 128), Some(0x61_0100));
    assert_eq!(
        bump_alloc(&mut next, 0x70_0000, 0x10_0000, 8),
        None,
        "past the end"
    );
    assert_eq!(bump_alloc(&mut next, 0x70_0000, 8, 0), None);
    assert_eq!(
        bump_alloc(&mut next, 0x70_0000, 8, 24),
        None,
        "not a power of two"
    );
    assert_eq!(bump_alloc(&mut next, 0x70_0000, usize::MAX, 8), None);
    let mut top = u32::MAX - 3;
    assert_eq!(bump_alloc(&mut top, u32::MAX, 8, 8), None, "wraps");
    assert_eq!(bump_alloc(&mut top, u32::MAX, 8, 1 << 40), None);
}

#[test]
fn poll_loop_reads_once_more_after_the_deadline() {
    let mut b = MemBus::new();
    b.forced.insert(0x30, 0);
    let e = poll_loop(&mut b, Width::W32, 0x30, 0x1, 0x1, 3, 1000).unwrap_err();
    assert_eq!(
        e,
        BusError::PollTimeout {
            addr: 0x30,
            width: Width::W32,
            mask: 1,
            want: 1,
            last: 0
        }
    );
    b.forced.insert(0x30, 1);
    assert_eq!(
        poll_loop(&mut b, Width::W32, 0x30, 0x1, 0x1, 3, 1000),
        Ok(())
    );
    // a huge timeout is clamped to MAX_WAIT_MS, so it ends
    b.forced.insert(0x30, 0);
    b.clock_ms = 0;
    assert!(poll_loop(&mut b, Width::W32, 0x30, 0x1, 0x1, u16::MAX, 1000).is_err());
    assert!(b.clock_ms <= 10_001);
}

#[test]
fn ring_lengths_that_cannot_fit_are_errors() {
    let mut b = chip();
    let huge = RingConfig {
        rxq: u32::MAX,
        ..RingConfig::js()
    };
    assert!(matches!(
        crate::bringup::pci_alloc::pci_alloc(&mut b, &huge),
        Err(Rtw89Error::Invalid {
            value: u32::MAX,
            ..
        })
    ));
    assert!(
        b.trace.is_empty() && b.dma_live.is_empty(),
        "refused before any DMA"
    );
    let mut b = chip();
    let big = RingConfig {
        ach0: 1000,
        ..RingConfig::js()
    };
    assert!(matches!(
        crate::bringup::pci_alloc::pci_alloc(&mut b, &big),
        Err(Rtw89Error::Bus(BusError::DmaExhausted { .. }))
    ));
}

#[test]
fn prefix_on_the_memory_bus_ends_with_the_scoreboard() {
    let mut b = chip();
    let mut run = Bringup::new();
    run.probe_power_on(&mut b, &RingConfig::js()).unwrap();
    assert_eq!(b.lines().len(), 321);
    assert_eq!(
        b.lines().last().unwrap(),
        "wr8 0x000000AF 0x00000000 0x00000081"
    );
    assert_eq!(run.power.unwrap().secure_boot, Some(false));
    assert_eq!(run.mac.power_on, Some(true));
    assert_eq!(b.cfg[&4] & 0xffff, 0x6, "memory space and bus master on");
}

#[test]
fn another_chip_cut_stops_before_power_on_and_the_stop_restores_bus_master() {
    let mut b = chip();
    b.forced.insert(R_AX_SYS_CFG1, prep(B_AX_CHIP_VER_MASK, 0));
    let mut run = Bringup::new();
    let e = run.probe_power_on(&mut b, &RingConfig::js()).unwrap_err();
    assert_eq!(e, Rtw89Error::UnsupportedChip { cv: 0, acv: 1 });
    let n = b.lines().len();
    run.stop(&mut b).unwrap();
    let tail: Vec<_> = b.lines()[n..].to_vec();
    assert_eq!(tail[0], "# stop");
    assert!(
        !tail.iter().any(|l| l.starts_with("poll32 0x00000004")),
        "no power-off: never powered on"
    );
    assert_eq!(b.cfg[&4] & 0xffff, 0x2, "bus master off, memory space kept");
}

#[test]
fn warm_chip_bus_master_and_mac_go_off_first() {
    let mut b = chip();
    b.cfg.insert(4, 0x0010_0006);
    b.forced.insert(
        R_AX_IC_PWR_STATE,
        prep(B_AX_WLMAC_PWR_STE_MASK, MAC_AX_MAC_ON),
    );
    let s = safety::bus_master_on(&mut b).unwrap();
    assert!(s.was_master);
    assert_eq!(s.cmd0, 0x2);
    let l = b.lines();
    assert_eq!(l[0], "cfgr 0x00000004 0x00000000 0x00000000");
    assert_eq!(
        l[1], "cfgw 0x00000004 0x00000000 0x00000002",
        "bus master off before anything else"
    );
    assert_eq!(l[2], "rd32 0x000003F0 0x00000000 0x00000000");
    assert_eq!(
        count(&b, "poll32 0x00000004 0x00000200 0x00000000 20 ms"),
        1,
        "APFM_OFFMAC: powered off"
    );
    assert_eq!(l[l.len() - 2], "cfgw 0x00000004 0x00000000 0x00000006");
}

#[test]
fn mac_still_on_retries_once_then_fails_and_the_stop_powers_off() {
    let mut b = chip();
    b.forced.insert(
        R_AX_IC_PWR_STATE,
        prep(B_AX_WLMAC_PWR_STE_MASK, MAC_AX_MAC_ON),
    );
    let mut run = Bringup::new();
    let e = run.probe_power_on(&mut b, &RingConfig::js()).unwrap_err();
    assert!(matches!(e, Rtw89Error::MacAlreadyOn { .. }));
    // safety off, then rtw89_mac_pwr_on: on (busy), off, on (busy)
    assert_eq!(
        count(&b, "poll32 0x00000004 0x00000200 0x00000000 20 ms"),
        2
    );
    assert_eq!(run.mac.power_on, None, "unknown after a failed power-on");
    run.stop(&mut b).unwrap();
    assert_eq!(
        count(&b, "poll32 0x00000004 0x00000200 0x00000000 20 ms"),
        3,
        "the stop powers off"
    );
}

#[test]
fn a_failing_power_poll_powers_off_and_on_again_as_linux() {
    let mut b = chip();
    b.failing_polls.insert(R_AX_AFE_LDO_CTRL);
    let r = power::mac_pwr_on(&mut b, &power::PwrOpts::probe(1));
    assert!(
        matches!(r, Err(Rtw89Error::Bus(BusError::PollTimeout { addr, .. })) if addr == R_AX_AFE_LDO_CTRL)
    );
    assert_eq!(
        b.poll_failures, 2,
        "first try and the try after the power-off"
    );
    assert_eq!(
        count(&b, "poll32 0x00000004 0x00000200 0x00000000 20 ms"),
        1,
        "one power-off between"
    );
}

#[test]
fn efuse_dump_tries_five_times_and_the_power_on_goes_on() {
    let mut b = chip();
    b.failing_polls.insert(R_AX_EFUSE_CTRL);
    assert_eq!(efuse::read_fw_secure(&mut b, 1), Ok(None));
    assert_eq!(b.poll_failures, 5);
    let mut b = chip();
    b.failing_polls.insert(R_AX_EFUSE_CTRL);
    let on = power::mac_pwr_on(&mut b, &power::PwrOpts::probe(1)).unwrap();
    assert_eq!(on.secure_boot, None, "Linux ignores the failed dump");
    assert_eq!(
        b.lines().last().unwrap(),
        "wr8 0x000000AF 0x00000000 0x00000081"
    );
}

#[test]
fn secure_boot_bytes_other_than_ff_count() {
    let mut b = chip();
    b.forced.insert(R_AX_EFUSE_CTRL, B_AX_EF_RDY | 0x5a);
    assert_eq!(efuse::read_fw_secure(&mut b, 1), Ok(Some(true)));
}

#[test]
fn cut_a_dumps_with_burst_mode() {
    let mut b = chip();
    efuse::dump_physical(&mut b, 0x5EC, 2, CHIP_CAV).unwrap();
    assert_eq!(
        count(&b, "rmw32 0x00000038 0x00000000 0x00080000"),
        1,
        "EF_BURST on"
    );
    assert_eq!(
        count(&b, "rmw32 0x00000038 0x00080000 0x00000000"),
        1,
        "EF_BURST off"
    );
    let mut b = chip();
    assert_eq!(
        efuse::dump_physical(&mut b, 0, 0, 1).unwrap(),
        Vec::<u8>::new()
    );
    assert_eq!(
        efuse::dump_physical(&mut b, u32::MAX, 4, 1).unwrap().len(),
        0,
        "no wrap past the end"
    );
}

#[test]
fn rfe_5_and_a_valid_efuse_take_the_other_branches() {
    let mut b = chip();
    let o = power::PwrOpts {
        rfe: 5,
        efuse_valid: true,
        power_k_valid: false,
        cv: CHIP_CBV,
        probe: false,
    };
    power::pwr_on_func(&mut b, &o).unwrap();
    let l = b.lines();
    assert_eq!(
        l[0], "wr16 0x00000228 0x00000000 0x00004A82",
        "rtw8852b_pwr_sps_ana"
    );
    assert_eq!(
        count(&b, "rmw32 0x00000400 0x0000000C 0x00000004"),
        1,
        "C2_L1 only with rfe 5"
    );
    assert_eq!(
        count(&b, "rmw32 0x00000400 0x000000C0 0x00000040"),
        1,
        "R1_L1 only with rfe 5"
    );
    assert_eq!(
        count(&b, "rmw16 0x0000007A 0x0000000F 0x0000000A"),
        1,
        "HCI LDO VADJ on cut B"
    );
    let mut b = chip();
    power::pwr_on_func(&mut b, &power::PwrOpts::probe(1)).unwrap();
    assert_eq!(
        count(&b, "rmw16 0x0000007A 0x0000000F 0x0000000A"),
        0,
        "not at probe"
    );
    assert!(!b.lines()[0].starts_with("wr16"));
}

#[test]
fn a_failing_last_read_of_the_safety_step_turns_bus_master_off_again() {
    let mut b = chip();
    // ops: cfgr, rd32 IC_PWR_STATE, cfgw (master on), cfgr <- fails
    b.fail_at_op = Some(3);
    let e = safety::bus_master_on(&mut b).unwrap_err();
    assert_eq!(e, Rtw89Error::Bus(BusError::Gone));
    assert_eq!(b.cfg[&4], 0x2, "bus master off, memory space kept");
    let mut b = chip();
    b.fail_at_op = Some(5); // after the notes "stage fw" and "not Linux: safety ..."
    let mut run = Bringup::new();
    assert!(run.probe_power_on(&mut b, &RingConfig::js()).is_err());
    assert!(run.safety.is_none() && run.rings.is_none());
    assert_eq!(b.cfg[&4], 0x2);
    run.stop(&mut b).unwrap();
}

#[test]
fn lps_is_powered_off_and_all_ones_is_only_marked() {
    let mut b = chip();
    b.forced.insert(
        R_AX_IC_PWR_STATE,
        prep(B_AX_WLMAC_PWR_STE_MASK, safety::MAC_AX_MAC_LPS),
    );
    let s = safety::bus_master_on(&mut b).unwrap();
    assert_eq!((s.mac_state, s.powered_off, s.dead_look), (2, true, false));
    assert_eq!(
        count(&b, "poll32 0x00000004 0x00000200 0x00000000 20 ms"),
        1,
        "APFM_OFFMAC: powered off"
    );

    let mut b = chip();
    b.forced.insert(R_AX_IC_PWR_STATE, u32::MAX);
    let s = safety::bus_master_on(&mut b).unwrap();
    assert_eq!((s.mac_state, s.powered_off, s.dead_look), (3, false, true));
    assert_eq!(b.lines().len(), 4, "no power-off on an all-ones read");

    let mut b = chip();
    let s = safety::bus_master_on(&mut b).unwrap();
    assert_eq!((s.mac_state, s.powered_off, s.dead_look), (0, false, false));
}

#[test]
fn the_stop_powers_off_with_the_cards_rfe() {
    let mut b = chip();
    let mut run = Bringup::new();
    run.probe_power_on(&mut b, &RingConfig::js()).unwrap();
    run.rfe = 5;
    let n = b.lines().len();
    run.stop(&mut b).unwrap();
    let l = b.lines();
    assert_eq!(l[n], "# stop");
    assert_eq!(
        l[n + 1],
        "wr16 0x00000228 0x00000000 0x00004A82",
        "rtw8852b_pwr_off_func's RFE 5 write first"
    );
    let mut b = chip();
    let mut run = Bringup::new();
    run.probe_power_on(&mut b, &RingConfig::js()).unwrap();
    let n = b.lines().len();
    run.stop(&mut b).unwrap();
    assert!(
        !b.lines()[n + 1].starts_with("wr16"),
        "rfe 0: no SPS_ANA write"
    );
}

#[test]
fn dma_regions_go_back() {
    // a ring that does not fit: what was handed out before it is given back
    let mut b = chip();
    let big = RingConfig {
        rpq: 1000,
        ..RingConfig::js()
    };
    assert!(crate::bringup::pci_alloc::pci_alloc(&mut b, &big).is_err());
    assert!(b.dma_live.is_empty(), "left: {:?}", b.dma_live);

    // a refused chip cut: the stop gives the rings back
    let mut b = chip();
    b.forced.insert(R_AX_SYS_CFG1, prep(B_AX_CHIP_VER_MASK, 0));
    let mut run = Bringup::new();
    assert!(run.probe_power_on(&mut b, &RingConfig::js()).is_err());
    assert!(!b.dma_live.is_empty());
    run.stop(&mut b).unwrap();
    assert!(b.dma_live.is_empty() && run.rings.is_none());

    // a full run and its stop
    let mut b = chip();
    let mut run = Bringup::new();
    run.probe_power_on(&mut b, &RingConfig::js()).unwrap();
    assert_eq!(b.dma_live.len(), run.rings.as_ref().unwrap().dma.len());
    run.stop(&mut b).unwrap();
    assert!(b.dma_live.is_empty());
    assert!(
        b.dma_free(0x61_0000, 8).is_err(),
        "a second free is refused"
    );
}

#[test]
fn the_memory_bus_records_writes_cut_to_their_width_as_the_trace_bus() {
    let mut b = MemBus::new();
    b.write(Width::W8, 0x20, 0x1ff).unwrap();
    b.write(Width::W16, 0x22, 0x1_2345).unwrap();
    b.mem_write(crate::bus::MemWidth::M8, 0x61_0000, 0x1ff)
        .unwrap();
    assert_eq!(
        b.lines(),
        vec![
            "wr8 0x00000020 0x00000000 0x000000FF",
            "wr16 0x00000022 0x00000000 0x00002345",
            "mwr8 0x00610000 0x00000000 0x000000FF",
        ]
    );
}

#[test]
fn poll_loop_ends_even_when_the_clock_does_not_move() {
    for step in [0, 1, 999] {
        let mut b = MemBus::new();
        b.forced.insert(0x30, 0);
        let e = poll_loop(&mut b, Width::W32, 0x30, 1, 1, 5, step);
        assert!(
            matches!(e, Err(BusError::PollTimeout { .. })),
            "step {step}"
        );
        assert_eq!(b.clock_ms, 0, "MemBus's clock stood still");
    }
}
