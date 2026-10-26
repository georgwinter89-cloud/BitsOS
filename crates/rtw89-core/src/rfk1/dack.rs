//! RCK (RF resistor), DACK (ADC/DAC DC offsets and the DAC MSB calibration) and RX DCK (RX DC
//! offset) of rtw8852b_rfk.c, in Linux's order (JS js/rfk1-dack.mjs).

use crate::bus::{Bus, field};
use crate::error::Rtw89Result;
use crate::phy::io::{MASKDWORD, RFREG_MASK, phy_mask32, phy_r32_mask, rf_r, rf_w};
use crate::phy::{Chan, Phy};
use crate::tables::Rfk;
use crate::tables::regs::*;
use crate::tables::rfk1 as t;

use super::util::{
    RF_PATH_NUM_8852B, btc_phymap, debug_bb_read, debug_rf_read, kpath, poll_read, rfk_parser,
    set_rx_dck, wait_rx_mode,
};
use super::{Addc, MSBK_NR, Rck, RfkEvent, RfkHooks, RfkState, path_idx};

/// rtw8852b_rfk.c:28 ADDC_T_AVG: samples of the ADC DC per check
const ADDC_T_AVG: i32 = 100;
const RF_PATH_A: u32 = 0;
const RF_PATH_B: u32 = 1;

/// FIELD_GET_SIGNED of a 12-bit field
fn sign12(v: u32) -> i32 {
    ((v << 20) as i32) >> 20
}

// Linux: _rck (rtw8852b_rfk.c:336)
fn rck_path<B: Bus + ?Sized>(bus: &mut B, st: &mut RfkState, path: u32) -> Rtw89Result<()> {
    let p = path_idx(path)?;
    let rf_reg5 = rf_r(bus, path, RR_RSV1, RFREG_MASK)?;
    rf_w(bus, path, RR_RSV1, RR_RSV1_RST, 0x0)?;
    rf_w(bus, path, RR_MOD, RR_MOD_MASK, RR_MOD_V_RX)?;
    debug_rf_read(bus, path, RR_MOD)?; // debug read: RF0x00
    rf_w(bus, path, RR_RCKC, RFREG_MASK, 0x00240)?; // RCK trigger
    let poll = poll_read(bus, |b| rf_r(b, path, RR_RCKS, 1 << 3), |v| v != 0, 2, 30)?;
    let rck_val = rf_r(bus, path, RR_RCKC, RR_RCKC_CA)?;
    rf_w(bus, path, RR_RCKC, RFREG_MASK, rck_val)?;
    rf_w(bus, path, RR_RSV1, RFREG_MASK, rf_reg5)?;
    debug_rf_read(bus, path, RR_RCKC)?; // debug read: RF 0x1b
    if poll.timed_out {
        st.push(RfkEvent::RckTimeout {
            path,
            value: poll.val,
        });
    }
    st.rck[p] = Some(Rck {
        rck_val,
        timeout: poll.timed_out,
    }); // Linux only prints these
    Ok(())
}

// Linux: rtw8852b_rck (rtw8852b_rfk.c:3744)
pub fn rck<B: Bus + ?Sized>(bus: &mut B, st: &mut RfkState) -> Rtw89Result<()> {
    for path in 0..RF_PATH_NUM_8852B {
        rck_path(bus, st, path)?;
    }
    bus.flush()?;
    Ok(())
}

// Linux: _afe_init (rtw8852b_rfk.c:371)
fn afe_init<B: Bus + ?Sized>(bus: &mut B) -> Rtw89Result<()> {
    bus.w32(R_AX_PHYREG_SET, 0xf)?; // a MAC register, no BB offset
    rfk_parser(bus, t::AFE_INIT_DEFS)
}

// Linux: _drck (rtw8852b_rfk.c:378)
fn drck<B: Bus + ?Sized>(bus: &mut B, st: &mut RfkState) -> Rtw89Result<()> {
    phy_mask32(bus, R_DRCK_V1, B_DRCK_V1_KICK, 0x1)?;
    let poll = poll_read(
        bus,
        |b| phy_r32_mask(b, R_DRCK_RS, B_DRCK_RS_DONE),
        |v| v != 0,
        1,
        10000,
    )?;
    if poll.timed_out {
        st.push(RfkEvent::DrckTimeout { value: poll.val });
    }
    phy_mask32(bus, R_DRCK_V1, B_DRCK_V1_KICK, 0x0)?;
    phy_mask32(bus, R_DRCK_FH, B_DRCK_LAT, 0x1)?;
    bus.delay_us(1)?;
    phy_mask32(bus, R_DRCK_FH, B_DRCK_LAT, 0x0)?;
    let rck_d = phy_r32_mask(bus, R_DRCK_RS, B_DRCK_RS_LPS)?;
    phy_mask32(bus, R_DRCK_V1, B_DRCK_V1_SEL, 0x0)?;
    phy_mask32(bus, R_DRCK_V1, B_DRCK_V1_CV, rck_d)?;
    debug_bb_read(bus, R_DRCK_V1) // debug read: 0xc0cc
}

// Linux: _addck_backup (rtw8852b_rfk.c:404)
fn addck_backup<B: Bus + ?Sized>(bus: &mut B, st: &mut RfkState) -> Rtw89Result<()> {
    let a = &mut st.dack.addck_d;
    phy_mask32(bus, R_ADDCK0, B_ADDCK0, 0x0)?;
    a[0][0] = phy_r32_mask(bus, R_ADDCKR0, B_ADDCKR0_A0)?;
    a[0][1] = phy_r32_mask(bus, R_ADDCKR0, B_ADDCKR0_A1)?;
    phy_mask32(bus, R_ADDCK1, B_ADDCK1, 0x0)?;
    a[1][0] = phy_r32_mask(bus, R_ADDCKR1, B_ADDCKR1_A0)?;
    a[1][1] = phy_r32_mask(bus, R_ADDCKR1, B_ADDCKR1_A1)?;
    Ok(())
}

// Linux: _addck_reload (rtw8852b_rfk.c:417)
fn addck_reload<B: Bus + ?Sized>(bus: &mut B, st: &RfkState) -> Rtw89Result<()> {
    let a = &st.dack.addck_d;
    // S0
    phy_mask32(bus, R_ADDCK0D, B_ADDCK0D_VAL, a[0][0])?;
    phy_mask32(bus, R_ADDCK0, B_ADDCK0_VAL, a[0][1] >> 6)?;
    phy_mask32(bus, R_ADDCK0D, B_ADDCK0D_VAL2, a[0][1] & 0x3f)?;
    phy_mask32(bus, R_ADDCK0, B_ADDCK0_MAN, 0x3)?;
    // S1
    phy_mask32(bus, R_ADDCK1D, B_ADDCK1D_VAL, a[1][0])?;
    phy_mask32(bus, R_ADDCK1, B_ADDCK0_VAL, a[1][1] >> 6)?;
    phy_mask32(bus, R_ADDCK1D, B_ADDCK1D_VAL2, a[1][1] & 0x3f)?;
    phy_mask32(bus, R_ADDCK1, B_ADDCK1_MAN, 0x3)?;
    Ok(())
}

// Linux: _dack_backup_s0 (rtw8852b_rfk.c:434), _dack_backup_s1 (:461)
fn dack_backup<B: Bus + ?Sized>(bus: &mut B, st: &mut RfkState, path: u32) -> Rtw89Result<()> {
    let p = path_idx(path)?;
    let s0 = path == RF_PATH_A;
    if s0 {
        phy_mask32(bus, R_P0_NRBW, B_P0_NRBW_DBG, 0x1)?;
    } else {
        phy_mask32(bus, R_P1_DBGMOD, B_P1_DBGMOD_ON, 0x1)?;
    }
    let d = &mut st.dack;
    for i in 0..MSBK_NR {
        let iv = i as u32;
        if s0 {
            phy_mask32(bus, R_DCOF0, B_DCOF0_V, iv)?;
            d.msbk_d[p][0][i] = phy_r32_mask(bus, R_DACK_S0P2, B_DACK_S0M0)?;
            phy_mask32(bus, R_DCOF8, B_DCOF8_V, iv)?;
            d.msbk_d[p][1][i] = phy_r32_mask(bus, R_DACK_S0P3, B_DACK_S0M1)?;
        } else {
            phy_mask32(bus, R_DACK10, B_DACK10, iv)?;
            d.msbk_d[p][0][i] = phy_r32_mask(bus, R_DACK10S, B_DACK10S)?;
            phy_mask32(bus, R_DACK11, B_DACK11, iv)?;
            d.msbk_d[p][1][i] = phy_r32_mask(bus, R_DACK11S, B_DACK11S)?;
        }
    }
    let regs = if s0 {
        [
            (R_DACK_BIAS00, B_DACK_BIAS00),
            (R_DACK_BIAS01, B_DACK_BIAS01),
            (R_DACK_DADCK00, B_DACK_DADCK00),
            (R_DACK_DADCK01, B_DACK_DADCK01),
        ]
    } else {
        [
            (R_DACK_BIAS10, B_DACK_BIAS10),
            (R_DACK_BIAS11, B_DACK_BIAS11),
            (R_DACK_DADCK10, B_DACK_DADCK10),
            (R_DACK_DADCK11, B_DACK_DADCK11),
        ]
    };
    d.biask_d[p][0] = phy_r32_mask(bus, regs[0].0, regs[0].1)?;
    d.biask_d[p][1] = phy_r32_mask(bus, regs[1].0, regs[1].1)?;
    d.dadck_d[p][0] = phy_r32_mask(bus, regs[2].0, regs[2].1)?;
    d.dadck_d[p][1] = phy_r32_mask(bus, regs[3].0, regs[3].1)?;
    Ok(())
}

// Linux: _check_addc (rtw8852b_rfk.c:488): 100 samples of the ADC DC; Linux only prints the
// averages, kept in `Dack::addc` to judge ADDCK on the chip
fn check_addc<B: Bus + ?Sized>(bus: &mut B, st: &mut RfkState, path: u32) -> Rtw89Result<()> {
    rfk_parser(
        bus,
        if path == RF_PATH_A {
            t::CHECK_ADDC_DEFS_A
        } else {
            t::CHECK_ADDC_DEFS_B
        },
    )?;
    let (mut dc_re, mut dc_im) = (0i32, 0i32);
    for _ in 0..ADDC_T_AVG {
        let tmp = phy_r32_mask(bus, R_DBG32_D, MASKDWORD)?;
        dc_re += sign12(field(tmp, 0xfff000));
        dc_im += sign12(tmp & 0xfff);
    }
    // C's integer division truncates toward zero, as Rust's
    st.dack.addc.push(Addc {
        path,
        dc_re: dc_re / ADDC_T_AVG,
        dc_im: dc_im / ADDC_T_AVG,
    });
    Ok(())
}

// Linux: _addck (rtw8852b_rfk.c:511), S0 then S1
fn addck<B: Bus + ?Sized>(bus: &mut B, st: &mut RfkState) -> Rtw89Result<()> {
    let path_reg = [R_PATH0_SAMPL_DLY_T_V1, R_PATH1_SAMPL_DLY_T_V1];
    for path in [RF_PATH_A, RF_PATH_B] {
        let s0 = path == RF_PATH_A;
        if s0 {
            phy_mask32(bus, R_ADDCK0, B_ADDCK0_MAN, 0x0)?;
            phy_mask32(bus, R_PATH1_SAMPL_DLY_T_V1, 0x30, 0x0)?;
            phy_mask32(bus, R_P0_NRBW, B_P0_NRBW_DBG, 0x1)?;
        } else {
            phy_mask32(bus, R_P1_DBGMOD, B_P1_DBGMOD_ON, 0x1)?;
        }
        phy_mask32(bus, R_ANAPAR, B_ANAPAR_ADCCLK, 0x0)?;
        phy_mask32(bus, R_ANAPAR, B_ANAPAR_FLTRST, 0x0)?;
        phy_mask32(bus, R_ANAPAR, B_ANAPAR_FLTRST, 0x1)?;
        phy_mask32(bus, R_ANAPAR_PW15, B_ANAPAR_PW15_H, 0xf)?;
        phy_mask32(bus, R_ANAPAR, B_ANAPAR_EN, 0x0)?;
        phy_mask32(bus, path_reg[path as usize], 1 << 1, 0x1)?;
        phy_mask32(bus, R_ANAPAR_PW15, B_ANAPAR_PW15_H, 0x3)?;
        check_addc(bus, st, path)?; // before ADDCK

        let (reg, rreg, trg, kick) = if s0 {
            (R_ADDCK0, R_ADDCKR0, B_ADDCK0_TRG, B_ADDCK0)
        } else {
            (R_ADDCK1, R_ADDCKR1, B_ADDCK1_TRG, B_ADDCK1)
        };
        phy_mask32(bus, reg, trg, 0x1)?;
        phy_mask32(bus, reg, trg, 0x0)?;
        bus.delay_us(1)?;
        phy_mask32(bus, reg, kick, 0x1)?;
        let poll = poll_read(bus, |b| phy_r32_mask(b, rreg, 1), |v| v != 0, 1, 10000)?;
        if poll.timed_out {
            st.dack.addck_timeout[path as usize] = true;
            st.push(RfkEvent::AddckTimeout {
                path,
                value: poll.val,
            });
        }
        check_addc(bus, st, path)?; // after ADDCK

        phy_mask32(bus, path_reg[path as usize], 1 << 1, 0x0)?;
        phy_mask32(bus, R_ANAPAR, B_ANAPAR_EN, 0x1)?;
        phy_mask32(bus, R_ANAPAR_PW15, B_ANAPAR_PW15_H, 0xc)?;
        phy_mask32(bus, R_ANAPAR, B_ANAPAR_ADCCLK, 0x1)?;
        if s0 {
            phy_mask32(bus, R_P0_NRBW, B_P0_NRBW_DBG, 0x0)?;
        } else {
            phy_mask32(bus, R_P1_DBGMOD, B_P1_DBGMOD_ON, 0x0)?;
        }
    }
    Ok(())
}

// Linux: _check_dadc (rtw8852b_rfk.c:588)
fn check_dadc<B: Bus + ?Sized>(bus: &mut B, st: &mut RfkState, path: u32) -> Rtw89Result<()> {
    let a = path == RF_PATH_A;
    rfk_parser(
        bus,
        if a {
            t::CHECK_DADC_EN_DEFS_A
        } else {
            t::CHECK_DADC_EN_DEFS_B
        },
    )?;
    check_addc(bus, st, path)?;
    rfk_parser(
        bus,
        if a {
            t::CHECK_DADC_DIS_DEFS_A
        } else {
            t::CHECK_DADC_DIS_DEFS_B
        },
    )
}

// Linux: _dack_s0_check_done (rtw8852b_rfk.c:601): both OK bits must be set (|| in the C
// stops at the first clear one)
fn dack_s0_check_done<B: Bus + ?Sized>(bus: &mut B, part1: bool) -> Rtw89Result<u32> {
    let (r0, b0, r1, b1) = if part1 {
        (R_DACK_S0P0, B_DACK_S0P0_OK, R_DACK_S0P1, B_DACK_S0P1_OK)
    } else {
        (R_DACK_S0P2, B_DACK_S0P2_OK, R_DACK_S0P3, B_DACK_S0P3_OK)
    };
    if phy_r32_mask(bus, r0, b0)? == 0 || phy_r32_mask(bus, r1, b1)? == 0 {
        return Ok(0);
    }
    Ok(1)
}

// Linux: _dack_s1_check_done (rtw8852b_rfk.c:650): one OK bit is enough (&& in the C)
fn dack_s1_check_done<B: Bus + ?Sized>(bus: &mut B, part1: bool) -> Rtw89Result<u32> {
    let (r0, b0, r1, b1) = if part1 {
        (R_DACK_S1P0, B_DACK_S1P0_OK, R_DACK_S1P1, B_DACK_S1P1_OK)
    } else {
        (R_DACK10S, B_DACK_S1P2_OK, R_DACK11S, B_DACK_S1P3_OK)
    };
    if phy_r32_mask(bus, r0, b0)? == 0 && phy_r32_mask(bus, r1, b1)? == 0 {
        return Ok(0);
    }
    Ok(1)
}

// Linux: _dack_s0 (rtw8852b_rfk.c:616), _dack_s1 (:665)
fn dack_path<B: Bus + ?Sized>(bus: &mut B, st: &mut RfkState, path: u32) -> Rtw89Result<()> {
    let p = path_idx(path)?;
    let s0 = path == RF_PATH_A;
    let done = if s0 {
        dack_s0_check_done::<B>
    } else {
        dack_s1_check_done::<B>
    };
    let defs: [&[Rfk]; 3] = if s0 {
        [t::DACK_S0_1_DEFS, t::DACK_S0_2_DEFS, t::DACK_S0_3_DEFS]
    } else {
        [t::DACK_S1_1_DEFS, t::DACK_S1_2_DEFS, t::DACK_S1_3_DEFS]
    };
    rfk_parser(bus, defs[0])?;
    if poll_read(bus, |b| done(b, true), |v| v != 0, 1, 10000)?.timed_out {
        st.dack.msbk_timeout[p] = true;
        st.push(RfkEvent::MsbkTimeout { path });
    }
    rfk_parser(bus, defs[1])?;
    if poll_read(bus, |b| done(b, false), |v| v != 0, 1, 10000)?.timed_out {
        st.dack.dadck_timeout[p] = true;
        st.push(RfkEvent::DadckTimeout { path });
    }
    rfk_parser(bus, defs[2])?;
    if !s0 {
        check_dadc(bus, st, RF_PATH_B)?;
    }
    dack_backup(bus, st, path)?;
    if s0 {
        phy_mask32(bus, R_P0_NRBW, B_P0_NRBW_DBG, 0x0)?;
    } else {
        phy_mask32(bus, R_P1_DBGMOD, B_P1_DBGMOD_ON, 0x0)?;
    }
    Ok(())
}

// Linux: _dac_cal (rtw8852b_rfk.c:756) with _dack (:700); _dack_dump only prints
fn dac_cal<B: Bus + ?Sized>(bus: &mut B, st: &mut RfkState) -> Rtw89Result<()> {
    st.dack.dack_done = false;
    st.dack.addc.clear();
    let rf0 = rf_r(bus, RF_PATH_A, RR_MOD, RFREG_MASK)?;
    let rf1 = rf_r(bus, RF_PATH_B, RR_MOD, RFREG_MASK)?;
    afe_init(bus)?;
    drck(bus, st)?;
    rf_w(bus, RF_PATH_A, RR_RSV1, RR_RSV1_RST, 0x0)?;
    rf_w(bus, RF_PATH_B, RR_RSV1, RR_RSV1_RST, 0x0)?;
    rf_w(bus, RF_PATH_A, RR_MOD, RFREG_MASK, 0x337e1)?;
    rf_w(bus, RF_PATH_B, RR_MOD, RFREG_MASK, 0x337e1)?;
    addck(bus, st)?;
    addck_backup(bus, st)?;
    addck_reload(bus, st)?;
    rf_w(bus, RF_PATH_A, RR_MODOPT, RFREG_MASK, 0x0)?;
    rf_w(bus, RF_PATH_B, RR_MODOPT, RFREG_MASK, 0x0)?;
    dack_path(bus, st, RF_PATH_A)?;
    dack_path(bus, st, RF_PATH_B)?;
    st.dack.dack_done = true;
    rf_w(bus, RF_PATH_A, RR_MOD, RFREG_MASK, rf0)?;
    rf_w(bus, RF_PATH_B, RR_MOD, RFREG_MASK, rf1)?;
    rf_w(bus, RF_PATH_A, RR_RSV1, RR_RSV1_RST, 0x1)?;
    rf_w(bus, RF_PATH_B, RR_RSV1, RR_RSV1_RST, 0x1)?;
    st.dack.dack_cnt = st.dack.dack_cnt.wrapping_add(1);
    Ok(())
}

// Linux: rtw8852b_dack (rtw8852b_rfk.c:3752)
pub fn dack<B: Bus + ?Sized>(
    bus: &mut B,
    st: &mut RfkState,
    chan: Option<&Chan>,
    hooks: &mut dyn RfkHooks<B>,
) -> Rtw89Result<()> {
    let phy_map = btc_phymap(chan, RTW89_PHY_0, 0)?;
    hooks.btc_wl_rfk(bus, phy_map, BTC_WRFKT_DACK, BTC_WRFK_START)?;
    dac_cal(bus, st)?;
    hooks.btc_wl_rfk(bus, phy_map, BTC_WRFKT_DACK, BTC_WRFK_STOP)?;
    bus.flush()?;
    Ok(())
}

// Linux: _rx_dck (rtw8852b_rfk.c:304); `tssi_mode` is rtwdev->is_tssi_mode
fn rx_dck_paths<B: Bus + ?Sized>(bus: &mut B, tssi_mode: [bool; 2], phy: u32) -> Rtw89Result<()> {
    for path in 0..RF_PATH_NUM_8852B {
        let tssi = tssi_mode[path as usize];
        let trk = R_P0_TSSI_TRK + (path << 13);
        let rf_reg5 = rf_r(bus, path, RR_RSV1, RFREG_MASK)?;
        let dck_tune = rf_r(bus, path, RR_DCK, RR_DCK_FINE)?;
        if tssi {
            phy_mask32(bus, trk, B_P0_TSSI_TRK_EN, 0x1)?;
        }
        rf_w(bus, path, RR_RSV1, RR_RSV1_RST, 0x0)?;
        rf_w(bus, path, RR_DCK, RR_DCK_FINE, 0x0)?;
        rf_w(bus, path, RR_MOD, RR_MOD_MASK, RR_MOD_V_RX)?;
        set_rx_dck(bus, phy, path)?;
        rf_w(bus, path, RR_DCK, RR_DCK_FINE, dck_tune)?;
        rf_w(bus, path, RR_RSV1, RFREG_MASK, rf_reg5)?;
        if tssi {
            phy_mask32(bus, trk, B_P0_TSSI_TRK_EN, 0x0)?;
        }
    }
    Ok(())
}

// Linux: rtw8852b_rx_dck (rtw8852b_rfk.c:3778)
pub fn rx_dck<B: Bus + ?Sized>(
    bus: &mut B,
    phy: &Phy,
    st: &mut RfkState,
    chan: Option<&Chan>,
    hooks: &mut dyn RfkHooks<B>,
    phy_idx: u32,
) -> Rtw89Result<()> {
    let phy_map = btc_phymap(chan, phy_idx, 0)?;
    hooks.btc_wl_rfk(bus, phy_map, BTC_WRFKT_RXDCK, BTC_WRFK_START)?;
    let tx_en = hooks.stop_sch_tx(bus)?; // MAC 0: phy_idx 0 is mac_idx 0 here
    wait_rx_mode(bus, kpath(phy_idx), &mut st.events)?;
    rx_dck_paths(bus, phy.tssi_mode, phy_idx)?;
    hooks.resume_sch_tx(bus, tx_en)?;
    hooks.btc_wl_rfk(bus, phy_map, BTC_WRFKT_RXDCK, BTC_WRFK_STOP)?;
    bus.flush()?;
    Ok(())
}
