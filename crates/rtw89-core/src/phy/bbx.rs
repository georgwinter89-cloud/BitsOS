//! The 8852B's own BB helpers (JS js/phy-bbx.mjs; Linux rtw8852b.c, rtw8852b_common.c):
//! BB/RF disable and reset, the BB set-up after the tables, and the PMAC/TSSI helpers the
//! RF calibrations and the coexistence call. The `_phy_idx` arguments of Linux are left out:
//! with dbcc off rtw89_phy_write32_idx writes PHY 0.

use crate::bringup::power::{xtal_read, xtal_write};
use crate::bus::{Bus, field};
use crate::error::Rtw89Result;
use crate::tables::phy::{PMAC_HT20_MCS7, PREAGC_DIS, PREAGC_EN};
use crate::tables::regs::*;

use super::Band;
use super::efuse::EfuseGain;
use super::io::{phy_clr32, phy_mask32, phy_r32, phy_set32, rf_r, rf_w, write_reg3_tbl};

pub use crate::macinit::enable_bb_rf;

// Linux: __rtw8852bx_mac_disable_bb_rf (rtw8852b_common.c:2033)
pub fn disable_bb_rf<B: Bus + ?Sized>(bus: &mut B) -> Rtw89Result<()> {
    bus.clr32(R_AX_WLRF_CTRL, B_AX_AFC_AFEDIG)?;
    bus.clr(
        crate::bus::Width::W8,
        R_AX_SYS_FUNC_EN,
        B_AX_FEN_BBRSTB | B_AX_FEN_BB_GLB_RSTN,
    )?;
    let s0 = xtal_read(bus, XTAL_SI_WL_RFC_S0)? & !XTAL_SI_RF00S_EN & 0xff;
    xtal_write(bus, XTAL_SI_WL_RFC_S0, s0, FULL_BIT_MASK)?;
    let s1 = xtal_read(bus, XTAL_SI_WL_RFC_S1)? & !XTAL_SI_RF10S_EN & 0xff;
    xtal_write(bus, XTAL_SI_WL_RFC_S1, s1, FULL_BIT_MASK)?;
    Ok(())
}

// Linux: __rtw8852bx_bb_reset_all (rtw8852b_common.c:1077)
pub fn bb_reset_all<B: Bus + ?Sized>(bus: &mut B) -> Rtw89Result<()> {
    phy_mask32(bus, R_S0_HW_SI_DIS, B_S0_HW_SI_DIS_W_R_TRIG, 0x7)?;
    phy_mask32(bus, R_S1_HW_SI_DIS, B_S1_HW_SI_DIS_W_R_TRIG, 0x7)?;
    bus.delay_us(1)?;
    phy_mask32(bus, R_RSTB_ASYNC, B_RSTB_ASYNC_ALL, 1)?;
    phy_mask32(bus, R_RSTB_ASYNC, B_RSTB_ASYNC_ALL, 0)?;
    phy_mask32(bus, R_S0_HW_SI_DIS, B_S0_HW_SI_DIS_W_R_TRIG, 0x0)?;
    phy_mask32(bus, R_S1_HW_SI_DIS, B_S1_HW_SI_DIS_W_R_TRIG, 0x0)?;
    phy_mask32(bus, R_RSTB_ASYNC, B_RSTB_ASYNC_ALL, 1)?;
    Ok(())
}

// Linux: rtw8852b_bb_reset (rtw8852b.c:614), the chip's bb_reset op
pub fn bb_reset<B: Bus + ?Sized>(bus: &mut B) -> Rtw89Result<()> {
    phy_set32(bus, R_P0_TXPW_RSTB, B_P0_TXPW_RSTB_MANON)?;
    phy_set32(bus, R_P0_TSSI_TRK, B_P0_TSSI_TRK_EN)?;
    phy_set32(bus, R_P1_TXPW_RSTB, B_P1_TXPW_RSTB_MANON)?;
    phy_set32(bus, R_P1_TSSI_TRK, B_P1_TSSI_TRK_EN)?;
    bb_reset_all(bus)?;
    phy_clr32(bus, R_P0_TXPW_RSTB, B_P0_TXPW_RSTB_MANON)?;
    phy_clr32(bus, R_P0_TSSI_TRK, B_P0_TSSI_TRK_EN)?;
    phy_clr32(bus, R_P1_TXPW_RSTB, B_P1_TXPW_RSTB_MANON)?;
    phy_clr32(bus, R_P1_TSSI_TRK, B_P1_TSSI_TRK_EN)?;
    Ok(())
}

// Linux: rtw8852b_bb_reset_en (rtw8852b.c:590)
pub fn bb_reset_en<B: Bus + ?Sized>(bus: &mut B, band: Band, en: bool) -> Rtw89Result<()> {
    if en {
        phy_mask32(bus, R_S0_HW_SI_DIS, B_S0_HW_SI_DIS_W_R_TRIG, 0x0)?;
        phy_mask32(bus, R_S1_HW_SI_DIS, B_S1_HW_SI_DIS_W_R_TRIG, 0x0)?;
        phy_mask32(bus, R_RSTB_ASYNC, B_RSTB_ASYNC_ALL, 1)?;
        if band == Band::B2g {
            phy_mask32(bus, R_RXCCA, B_RXCCA_DIS, 0x0)?;
        }
        phy_mask32(bus, R_PD_CTRL, B_PD_HIT_DIS, 0x0)?;
    } else {
        phy_mask32(bus, R_RXCCA, B_RXCCA_DIS, 0x1)?;
        phy_mask32(bus, R_PD_CTRL, B_PD_HIT_DIS, 0x1)?;
        phy_mask32(bus, R_S0_HW_SI_DIS, B_S0_HW_SI_DIS_W_R_TRIG, 0x7)?;
        phy_mask32(bus, R_S1_HW_SI_DIS, B_S1_HW_SI_DIS_W_R_TRIG, 0x7)?;
        bus.delay_us(1)?;
        phy_mask32(bus, R_RSTB_ASYNC, B_RSTB_ASYNC_ALL, 0)?;
    }
    Ok(())
}

// Linux: __rtw8852bx_bb_sethw (rtw8852b_common.c:1099) with rtw8852bx_bb_macid_ctrl_init
// (rtw89_mac_txpwr_write32: PHY 0 keeps the address). The RPL bias bytes are read after the
// BB parameters are loaded (s8 fields); as the JS driver both go to path A's slots.
pub fn bb_sethw<B: Bus + ?Sized>(bus: &mut B, gain: &mut EfuseGain) -> Rtw89Result<()> {
    phy_clr32(bus, R_P0_EN_SOUND_WO_NDP, B_P0_EN_SOUND_WO_NDP)?;
    phy_clr32(bus, R_P1_EN_SOUND_WO_NDP, B_P1_EN_SOUND_WO_NDP)?;
    let mut a = R_AX_PWR_MACID_LMT_TABLE0;
    while a <= R_AX_PWR_MACID_LMT_TABLE127 {
        bus.w32(a, 0)?;
        a += 4;
    }
    let p0 = phy_r32(bus, R_P0_RPL1)?;
    let p1 = phy_r32(bus, R_P1_RPL1)?;
    gain.offset_base[0] = field(p0, B_P0_RPL1_BIAS_MASK) as u8 as i8;
    gain.rssi_base[0] = field(p1, B_P0_RPL1_BIAS_MASK) as u8 as i8;
    Ok(())
}

// Linux: __rtw8852bx_bb_set_plcp_tx (rtw8852b_common.c:1431): rtw8852bx_pmac_ht20_mcs7_tbl
pub fn bb_set_plcp_tx<B: Bus + ?Sized>(bus: &mut B) -> Rtw89Result<()> {
    write_reg3_tbl(bus, PMAC_HT20_MCS7)
}

// Linux: __rtw8852bx_bb_set_pmac_pkt_tx (rtw8852b_common.c:1502) with
// rtw8852bx_bb_set_pmac_tx, rtw8852bx_stop_pmac_tx, rtw8852bx_start_pmac_tx; the mode is
// PKTS_TX (not CCK), `band` the channel's
pub fn bb_set_pmac_pkt_tx<B: Bus + ?Sized>(
    bus: &mut B,
    enable: bool,
    tx_cnt: u32,
    period: u32,
    band: Band,
) -> Rtw89Result<()> {
    if !enable {
        phy_mask32(bus, R_PMAC_TX_PRD, B_PMAC_PTX_EN, 0)?;
        phy_mask32(bus, R_PD_CTRL, B_PD_HIT_DIS, 0)?;
        if band == Band::B2g {
            phy_clr32(bus, R_RXCCA, B_RXCCA_DIS)?;
        }
        return Ok(());
    }
    phy_mask32(bus, R_PMAC_GNT, B_PMAC_GNT_TXEN, 1)?;
    phy_mask32(bus, R_PMAC_GNT, B_PMAC_GNT_RXEN, 1)?;
    phy_mask32(bus, R_PMAC_RX_CFG1, B_PMAC_OPT1_MSK, 0x3f)?;
    phy_mask32(bus, R_RSTB_ASYNC, B_RSTB_ASYNC_ALL, 0)?;
    phy_mask32(bus, R_PD_CTRL, B_PD_HIT_DIS, 1)?;
    phy_set32(bus, R_RXCCA, B_RXCCA_DIS)?;
    phy_mask32(bus, R_RSTB_ASYNC, B_RSTB_ASYNC_ALL, 1)?;
    phy_mask32(bus, R_PMAC_TX_PRD, B_PMAC_PTX_EN, 1)?;
    phy_mask32(bus, R_PMAC_TX_PRD, B_PMAC_TX_PRD_MSK, period)?;
    phy_mask32(bus, R_PMAC_TX_CNT, B_PMAC_TX_CNT_MSK, tx_cnt)?;
    phy_mask32(bus, R_PMAC_TX_CTRL, B_PMAC_TXEN_DIS, 1)?;
    phy_mask32(bus, R_PMAC_TX_CTRL, B_PMAC_TXEN_DIS, 0)?;
    Ok(())
}

// Linux: __rtw8852bx_bb_set_power (rtw8852b_common.c:1519)
pub fn bb_set_power<B: Bus + ?Sized>(bus: &mut B, pwr_dbm: i32) -> Rtw89Result<()> {
    phy_mask32(bus, R_MAC_SEL, B_MAC_SEL_PWR_EN, 1)?;
    phy_mask32(bus, R_TXPWR, B_TXPWR_MSK, pwr_dbm as u32)
}

// Linux: __rtw8852bx_bb_cfg_tx_path (rtw8852b_common.c:1529); `tx_path` an enum rtw89_rf_path
pub fn bb_cfg_tx_path<B: Bus + ?Sized>(bus: &mut B, tx_path: u32) -> Rtw89Result<()> {
    phy_mask32(bus, R_MAC_SEL, B_MAC_SEL_MOD, 7)?;
    let (sel, map) = match tx_path {
        RF_PATH_A => (1, 0),
        RF_PATH_B => (2, 0),
        RF_PATH_AB => (3, 4),
        _ => return Ok(()),
    };
    phy_mask32(bus, R_TXPATH_SEL, B_TXPATH_SEL_MSK, sel)?;
    phy_mask32(bus, R_TXNSS_MAP, B_TXNSS_MAP_MSK, map)
}

// Linux: __rtw8852bx_bb_tx_mode_switch (rtw8852b_common.c:1550): only mode 0 writes
pub fn bb_tx_mode_switch<B: Bus + ?Sized>(bus: &mut B, mode: u32) -> Rtw89Result<()> {
    if mode != 0 {
        return Ok(());
    }
    phy_mask32(bus, R_PMAC_GNT, B_PMAC_GNT_TXEN, 0)?;
    phy_mask32(bus, R_PMAC_GNT, B_PMAC_GNT_RXEN, 0)?;
    phy_mask32(bus, R_PMAC_RX_CFG1, B_PMAC_OPT1_MSK, 0)?;
    phy_mask32(bus, R_PMAC_RXMOD, B_PMAC_RXMOD_MSK, 0)?;
    phy_mask32(bus, R_MAC_SEL, B_MAC_SEL_DPD_EN, 0)?;
    phy_mask32(bus, R_MAC_SEL, B_MAC_SEL_MOD, 0)?;
    phy_mask32(bus, R_MAC_SEL, B_MAC_SEL_PWR_EN, 0)
}

/// Linux: struct rtw8852bx_bb_tssi_bak
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TssiBak {
    pub tx_path: u32,
    pub rx_path: u32,
    pub p0_rfmode: u32,
    pub p0_rfmode_ftm: u32,
    pub p1_rfmode: u32,
    pub p1_rfmode_ftm: u32,
    pub tx_pwr: i32,
}

// Linux: __rtw8852bx_bb_backup_tssi (rtw8852b_common.c:1568)
pub fn bb_backup_tssi<B: Bus + ?Sized>(bus: &mut B) -> Rtw89Result<TssiBak> {
    let v = [
        phy_r32(bus, R_TXPATH_SEL)?,
        phy_r32(bus, R_CHBW_MOD_V1)?,
        phy_r32(bus, R_P0_RFMODE)?,
        phy_r32(bus, R_P0_RFMODE_FTM_RX)?,
        phy_r32(bus, R_P1_RFMODE)?,
        phy_r32(bus, R_P1_RFMODE_FTM_RX)?,
        phy_r32(bus, R_TXPWR)?,
    ];
    Ok(TssiBak {
        tx_path: field(v[0], B_TXPATH_SEL_MSK),
        rx_path: field(v[1], B_ANT_RX_SEG0),
        p0_rfmode: v[2],
        p0_rfmode_ftm: v[3],
        p1_rfmode: v[4],
        p1_rfmode_ftm: v[5],
        // sign_extend32(tmp, 8)
        tx_pwr: ((field(v[6], B_TXPWR_MSK) << 23) as i32) >> 23,
    })
}

// Linux: __rtw8852bx_bb_restore_tssi (rtw8852b_common.c:1584)
pub fn bb_restore_tssi<B: Bus + ?Sized>(bus: &mut B, bak: &TssiBak) -> Rtw89Result<()> {
    phy_mask32(bus, R_TXPATH_SEL, B_TXPATH_SEL_MSK, bak.tx_path)?;
    let map = if bak.tx_path == RF_AB { 0x4 } else { 0x0 };
    phy_mask32(bus, R_TXNSS_MAP, B_TXNSS_MAP_MSK, map)?;
    phy_mask32(bus, R_CHBW_MOD_V1, B_ANT_RX_SEG0, bak.rx_path)?;
    phy_mask32(bus, R_MAC_SEL, B_MAC_SEL_PWR_EN, 1)?;
    phy_mask32(bus, R_P0_RFMODE, 0xffff_ffff, bak.p0_rfmode)?;
    phy_mask32(bus, R_P0_RFMODE_FTM_RX, 0xffff_ffff, bak.p0_rfmode_ftm)?;
    phy_mask32(bus, R_P1_RFMODE, 0xffff_ffff, bak.p1_rfmode)?;
    phy_mask32(bus, R_P1_RFMODE_FTM_RX, 0xffff_ffff, bak.p1_rfmode_ftm)?;
    phy_mask32(bus, R_TXPWR, B_TXPWR_MSK, bak.tx_pwr as u32)
}

// Linux: __rtw8852bx_get_thermal (rtw8852b_common.c:1769); `tssi_mode` is
// rtwdev->is_tssi_mode[path] (the RF calibration's)
pub fn get_thermal<B: Bus + ?Sized>(bus: &mut B, path: u32, tssi_mode: bool) -> Rtw89Result<u32> {
    if tssi_mode {
        let v = phy_r32(bus, 0x1c10u32.wrapping_add(path.wrapping_shl(13)))?;
        return Ok(field(v, 0x3F00_0000));
    }
    rf_w(bus, path, RR_TM, RR_TM_TRI, 0x1)?;
    rf_w(bus, path, RR_TM, RR_TM_TRI, 0x0)?;
    rf_w(bus, path, RR_TM, RR_TM_TRI, 0x1)?;
    bus.delay_us(200)?;
    rf_r(bus, path, RR_TM, RR_TM_VAL)
}

// Linux: __rtw8852bx_ctrl_nbtg_bt_tx (rtw8852b_common.c:1601) with rtw89_phy_write_reg3_tbl
pub fn ctrl_nbtg_bt_tx<B: Bus + ?Sized>(bus: &mut B, en: bool) -> Rtw89Result<()> {
    write_reg3_tbl(bus, if en { PREAGC_EN } else { PREAGC_DIS })
}

// Linux: __rtw8852bx_ctrl_btg_bt_rx (rtw8852b_common.c:1608)
pub fn ctrl_btg_bt_rx<B: Bus + ?Sized>(bus: &mut B, en: bool) -> Rtw89Result<()> {
    let v = |on: u32, off: u32| if en { on } else { off };
    phy_mask32(bus, R_PATH0_BT_SHARE_V1, B_PATH0_BT_SHARE_V1, v(1, 0))?;
    phy_mask32(bus, R_PATH0_BTG_PATH_V1, B_PATH0_BTG_PATH_V1, 0)?;
    phy_mask32(
        bus,
        R_PATH1_G_LNA6_OP1DB_V1,
        B_PATH1_G_LNA6_OP1DB_V1,
        v(0x20, 0x1a),
    )?;
    phy_mask32(
        bus,
        R_PATH1_G_TIA0_LNA6_OP1DB_V1,
        B_PATH1_G_TIA0_LNA6_OP1DB_V1,
        v(0x30, 0x2a),
    )?;
    phy_mask32(bus, R_PATH1_BT_SHARE_V1, B_PATH1_BT_SHARE_V1, v(1, 0))?;
    phy_mask32(bus, R_PATH1_BTG_PATH_V1, B_PATH1_BTG_PATH_V1, v(1, 0))?;
    phy_mask32(bus, R_PMAC_GNT, B_PMAC_GNT_P1, v(0, 0xc))?;
    phy_mask32(bus, R_CHBW_MOD_V1, B_BT_SHARE, v(1, 0))?;
    phy_mask32(bus, R_FC0_BW_V1, B_ANT_RX_BT_SEG0, v(2, 0))?;
    phy_mask32(bus, R_BT_DYN_DC_EST_EN_V1, B_BT_DYN_DC_EST_EN_MSK, 1)?;
    phy_mask32(bus, R_GNT_BT_WGT_EN, B_GNT_BT_WGT_EN, v(1, 0))
}

// Linux: rtw8852bx_bb_ctrl_rf_mode_rx_path (rtw8852b_common.c:1708); `rx_path` an enum
// rtw89_rf_path_bit (RF_A, RF_B, RF_AB); any other writes nothing
pub fn bb_ctrl_rf_mode_rx_path<B: Bus + ?Sized>(bus: &mut B, rx_path: u32) -> Rtw89Result<()> {
    let p0 = rx_path == RF_A || rx_path == RF_AB;
    let p1 = rx_path == RF_B || rx_path == RF_AB;
    if !p0 && !p1 {
        return Ok(());
    }
    let tx = |on: bool| if on { 0x1233312 } else { 0x1111111 };
    let rx = |on: bool| if on { 0x333 } else { 0x111 };
    phy_mask32(bus, R_P0_RFMODE, B_P0_RFMODE_ORI_TXRX_FTM_TX, tx(p0))?;
    phy_mask32(bus, R_P0_RFMODE_FTM_RX, B_P0_RFMODE_FTM_RX, rx(p0))?;
    phy_mask32(bus, R_P1_RFMODE, B_P1_RFMODE_ORI_TXRX_FTM_TX, tx(p1))?;
    phy_mask32(bus, R_P1_RFMODE_FTM_RX, B_P1_RFMODE_FTM_RX, rx(p1))
}

// clamp_t(s32, v, lo, hi) of Linux, as the register value it becomes
fn clamp_s32(v: i32, lo: i32, hi: i32) -> u32 {
    v.clamp(lo, hi) as u32
}

// Linux: rtw89_subband_to_gain_offset_band_of_ofdm (phy.h:801), 2 and 5 GHz
fn gain_offset_band_of_ofdm(subband: u32) -> u32 {
    match subband {
        RTW89_CH_5G_BAND_1 => RTW89_GAIN_OFFSET_5G_LOW,
        RTW89_CH_5G_BAND_3 => RTW89_GAIN_OFFSET_5G_MID,
        RTW89_CH_5G_BAND_4 => RTW89_GAIN_OFFSET_5G_HIGH,
        _ => RTW89_GAIN_OFFSET_2G_OFDM,
    }
}

// Linux: rtw8852bx_set_gain_offset (rtw8852b_common.c:632), PHY 0; `subband` an enum
// rtw89_subband, `antenna_rx` hal->antenna_rx. The ext-loss part is the RTL8852BT's.
pub fn set_gain_offset<B: Bus + ?Sized>(
    bus: &mut B,
    subband: u32,
    gain: &EfuseGain,
    antenna_rx: u32,
) -> Rtw89Result<()> {
    let sb = subband as usize;
    if gain.comp_valid {
        for (path, addr) in [R_P0_AGC_RSVD, R_P1_AGC_RSVD].into_iter().enumerate() {
            let tmp = i32::from(gain.comp[path].get(sb).copied().unwrap_or(0)) << 2;
            phy_mask32(bus, addr, MASKBYTE0, clamp_s32(tmp, -128, 127))?;
        }
    }
    if !gain.offset_valid {
        return Ok(());
    }
    let band = gain_offset_band_of_ofdm(subband) as usize;
    let off = |p: usize, b: usize| -i32::from(gain.offset[p].get(b).copied().unwrap_or(0));
    let base = i32::from(gain.offset_base[0]);
    let (offset_a, offset_b) = (off(0, band), off(1, band));
    for (addr, o) in [
        (R_PATH0_G_TIA1_LNA6_OP1DB_V1, offset_a),
        (R_PATH1_G_TIA1_LNA6_OP1DB_V1, offset_b),
    ] {
        let tmp = -((o << 2) + (base >> 2));
        phy_mask32(bus, addr, B_PATH0_R_G_OFST_MASK, clamp_s32(tmp, -128, 127))?;
    }
    let p = if antenna_rx == RF_B { 1 } else { 0 };
    let (offset_ofdm, offset_cck) = (off(p, band), off(p, RTW89_GAIN_OFFSET_2G_CCK as usize));
    phy_mask32(
        bus,
        R_P0_RPL1,
        B_P0_RPL1_BIAS_MASK,
        clamp_s32((offset_ofdm << 4) + base, -128, 127),
    )?;
    phy_mask32(
        bus,
        R_P1_RPL1,
        B_P0_RPL1_BIAS_MASK,
        clamp_s32((offset_ofdm << 4) + i32::from(gain.rssi_base[0]), -128, 127),
    )?;
    if subband == RTW89_CH_2G {
        let tmp = (offset_cck << 3) + (base >> 1);
        phy_mask32(
            bus,
            R_RX_RPL_OFST,
            B_RX_RPL_OFST_CCK_MASK,
            clamp_s32(tmp, -128 >> 1, 127 >> 1),
        )?;
    }
    Ok(())
}

// Linux: __rtw8852bx_bb_ctrl_rx_path (rtw8852b_common.c:1653); `rx_path` an enum
// rtw89_rf_path_bit, `band`/`subband` the channel's (chan's cfg_txrx_path and rfk2's TSSI
// alignment call it)
pub fn ctrl_rx_path<B: Bus + ?Sized>(
    bus: &mut B,
    rx_path: u32,
    band: Band,
    subband: u32,
    gain: &EfuseGain,
    antenna_rx: u32,
) -> Rtw89Result<()> {
    let sel = match rx_path {
        RF_A => 1,
        RF_B => 2,
        RF_AB => 3,
        _ => 0,
    };
    if sel != 0 {
        let two = u32::from(sel == 3);
        phy_mask32(bus, R_CHBW_MOD_V1, B_ANT_RX_SEG0, sel)?;
        phy_mask32(bus, R_FC0_BW_V1, B_ANT_RX_1RCCA_SEG0, sel)?;
        phy_mask32(bus, R_FC0_BW_V1, B_ANT_RX_1RCCA_SEG1, sel)?;
        phy_mask32(bus, R_RXHT_MCS_LIMIT, B_RXHT_MCS_LIMIT, two)?;
        phy_mask32(bus, R_RXVHT_MCS_LIMIT, B_RXVHT_MCS_LIMIT, two)?;
        phy_mask32(bus, R_RXHE, B_RXHE_USER_MAX, 4)?;
        phy_mask32(bus, R_RXHE, B_RXHE_MAX_NSS, two)?;
        phy_mask32(bus, R_RXHE, B_RXHETB_MAX_NSS, two)?;
    }
    set_gain_offset(bus, subband, gain, antenna_rx)?;
    ctrl_btg_bt_rx(
        bus,
        band == Band::B2g && (rx_path == RF_B || rx_path == RF_AB),
    )?;
    let (reg, mask) = if rx_path == RF_A {
        (R_P0_TXPW_RSTB, B_P0_TXPW_RSTB_MANON | B_P0_TXPW_RSTB_TSSI)
    } else {
        (R_P1_TXPW_RSTB, B_P1_TXPW_RSTB_MANON | B_P1_TXPW_RSTB_TSSI)
    };
    phy_mask32(bus, reg, mask, 1)?;
    phy_mask32(bus, reg, mask, 3)
}
