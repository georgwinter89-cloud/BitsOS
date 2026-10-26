//! IQK (TX/RX IQ imbalance and LO leakage) of rtw8852b_rfk.c, path A then path B, each wrapped
//! in register backup and restore (JS js/rfk1-iqk.mjs).

use crate::bus::Bus;
use crate::error::Rtw89Result;
use crate::phy::Chan;
use crate::phy::io::{MASKDWORD, phy_mask32, phy_r32, phy_r32_mask, rf_w};
use crate::tables::regs::*;
use crate::tables::rfk1 as t;

use super::iqkstep::{
    iqk_check_cal, iqk_lok, iqk_rxclk_setting, iqk_rxk_setting, iqk_txclk_setting, iqk_txk_setting,
    lok_res_table, rxk_group_sel, txk_group_sel,
};
use super::util::{
    btc_phymap, chan_get, debug_bb_read, debug_rf_read, kpath, rfk_backup_bb_reg,
    rfk_backup_rf_reg, rfk_parser, rfk_restore_bb_reg, rfk_restore_rf_reg, wait_rx_mode,
};
use super::{RfkEvent, RfkHooks, RfkState, path_idx};

/// rtw8852b_rfk.c:18 RTW8852B_IQK_VER
pub const IQK_VER: u32 = 0x2a;
/// rtw8852b_rfk.c:19 RTW8852B_IQK_SS: the paths IQK keeps results for
const IQK_SS: usize = 2;
const RF_PATH_A: u32 = 0;
const RF_PATH_B: u32 = 1;

// Linux: _iqk_info_iqk (rtw8852b_rfk.c:1316): results and fail flags into the IQKINF
// registers the firmware reads
fn iqk_info_iqk<B: Bus + ?Sized>(bus: &mut B, st: &mut RfkState, path: u32) -> Rtw89Result<()> {
    let p = path_idx(path)?;
    let sh = |m: u32| m << (path * 4);
    let iqk = &mut st.iqk;
    phy_mask32(
        bus,
        R_IQKINF,
        sh(B_IQKINF_FCOR),
        u32::from(iqk.lok_cor_fail[0][p]),
    )?;
    phy_mask32(
        bus,
        R_IQKINF,
        sh(B_IQKINF_FFIN),
        u32::from(iqk.lok_fin_fail[0][p]),
    )?;
    phy_mask32(
        bus,
        R_IQKINF,
        sh(B_IQKINF_FTX),
        u32::from(iqk.iqk_tx_fail[0][p]),
    )?;
    phy_mask32(
        bus,
        R_IQKINF,
        sh(B_IQKINF_F_RX),
        u32::from(iqk.iqk_rx_fail[0][p]),
    )?;
    iqk.bp_iqkenable[p] = phy_r32(bus, R_IQK_RES + (path << 8))?;
    iqk.bp_txkresult[p] = phy_r32(bus, R_TXIQC + (path << 8))?;
    iqk.bp_rxkresult[p] = phy_r32(bus, R_RXIQC + (path << 8))?;
    phy_mask32(bus, R_IQKINF2, B_IQKINF2_KCNT, iqk.iqk_times)?;
    if phy_r32_mask(bus, R_IQKINF, sh(B_IQKINF_FAIL))? != 0 {
        iqk.iqk_fail_cnt = iqk.iqk_fail_cnt.wrapping_add(1);
    }
    phy_mask32(bus, R_IQKINF2, sh(B_IQKINF2_FCNT), iqk.iqk_fail_cnt)
}

// Linux: _iqk_by_path (rtw8852b_rfk.c:1347) via _iqk_start_iqk (:1434). 8852B: is_nbiqk is set
// false by _iqk_init and never changed, so TXK and RXK always take the group (wide-band) way;
// _iqk_nbtxk/_iqk_nbrxk never run.
fn iqk_by_path<B: Bus + ?Sized>(
    bus: &mut B,
    st: &mut RfkState,
    phy_idx: u32,
    path: u32,
) -> Rtw89Result<()> {
    let p = path_idx(path)?;
    let mut lok_is_fail = false;
    iqk_txclk_setting(bus, path)?;
    // LOK, three tries with ibias 1, 2, 3
    for ibias in 0x1..=0x3 {
        if ibias > 0x1 {
            st.push(RfkEvent::LokRetry { path, ibias });
        }
        lok_res_table(bus, st, path, ibias)?;
        iqk_txk_setting(bus, st, path)?;
        lok_is_fail = iqk_lok(bus, st, phy_idx, path)?;
        if !lok_is_fail {
            break;
        }
    }
    st.iqk.lok_fail[p] = lok_is_fail; // Linux only prints it
    if lok_is_fail {
        st.push(RfkEvent::LokFailed { path });
    }
    let tx_fail = txk_group_sel(bus, st, phy_idx, path)?;
    st.iqk.iqk_tx_fail[0][p] = tx_fail;
    if tx_fail {
        st.push(RfkEvent::TxkFailed { path });
    }
    iqk_rxclk_setting(bus, st, path)?;
    iqk_rxk_setting(bus, st, path)?;
    let rx_fail = rxk_group_sel(bus, st, phy_idx, path)?;
    st.iqk.iqk_rx_fail[0][p] = rx_fail;
    if rx_fail {
        st.push(RfkEvent::RxkFailed { path });
    }
    iqk_info_iqk(bus, st, path)
}

// Linux: _iqk_get_ch_info (rtw8852b_rfk.c:1386)
fn iqk_get_ch_info<B: Bus + ?Sized>(
    bus: &mut B,
    st: &mut RfkState,
    chan: Option<&Chan>,
    path: u32,
) -> Rtw89Result<()> {
    let p = path_idx(path)?;
    let idx = st.mcc.slot()?;
    let c = chan_get(chan)?;
    debug_rf_read(bus, path, RR_CFGCH)?; // debug read: reg_rf18
    let reg35c = phy_r32_mask(bus, R_CIRST, B_CIRST_SYN)?;
    let iqk = &mut st.iqk;
    iqk.iqk_band[p] = c.band_type;
    iqk.iqk_bw[p] = c.band_width;
    iqk.iqk_ch[p] = c.channel;
    iqk.iqk_mcc_ch[idx][p] = c.channel;
    iqk.iqk_table_idx[p] = idx as u32;
    iqk.syn1to2 = u32::from(reg35c == 0x01);
    phy_mask32(bus, R_IQKINF, B_IQKINF_VER, IQK_VER)?;
    phy_mask32(bus, R_IQKCH, B_IQKCH_BAND << (path * 16), iqk.iqk_band[p])?;
    phy_mask32(bus, R_IQKCH, B_IQKCH_BW << (path * 16), iqk.iqk_bw[p])?;
    phy_mask32(bus, R_IQKCH, B_IQKCH_CH << (path * 16), iqk.iqk_ch[p])
}

// Linux: _iqk_restore (rtw8852b_rfk.c:1439)
fn iqk_restore<B: Bus + ?Sized>(bus: &mut B, st: &mut RfkState, path: u32) -> Rtw89Result<()> {
    let p = path_idx(path)?;
    phy_mask32(bus, R_TXIQC + (path << 8), MASKDWORD, st.iqk.nb_txcfir[p])?;
    phy_mask32(bus, R_RXIQC + (path << 8), MASKDWORD, st.iqk.nb_rxcfir[p])?;
    phy_mask32(bus, R_NCTL_CFG, MASKDWORD, 0x0000_0e19 + (path << 4))?;
    let fail = iqk_check_cal(bus, st, path)?;
    st.iqk.restore_fail[p] = fail; // Linux only prints it
    if fail {
        st.push(RfkEvent::IqkRestoreFailed { path });
    }
    phy_mask32(bus, R_NCTL_N1, B_NCTL_N1_CIP, 0x00)?;
    phy_mask32(bus, R_NCTL_RPT, MASKDWORD, 0x0000_0000)?;
    phy_mask32(bus, R_KIP_SYSCFG, MASKDWORD, 0x8000_0000)?;
    phy_mask32(bus, R_CFIR_SYS, B_IQK_RES_K, 0x0)?;
    phy_mask32(bus, R_IQRSN, B_IQRSN_K1, 0x0)?;
    phy_mask32(bus, R_IQRSN, B_IQRSN_K2, 0x0)?;
    rf_w(bus, path, RR_LUTWE, RR_LUTWE_LOK, 0x0)?;
    rf_w(bus, path, RR_LUTWE, RR_LUTWE_LOK, 0x0)?;
    rf_w(bus, path, RR_MOD, RR_MOD_MASK, 0x3)?;
    rf_w(bus, path, RR_RSV1, RR_RSV1_RST, 0x1)?;
    rf_w(bus, path, RR_BBDC, RR_BBDC_SEL, 0x1)
}

// Linux: _iqk_afebb_restore (rtw8852b_rfk.c:1467); 8852B: kpath is RF_AB, so the nondbcc table
fn iqk_afebb_restore<B: Bus + ?Sized>(bus: &mut B, phy_idx: u32) -> Rtw89Result<()> {
    let kp = kpath(phy_idx);
    if kp == RF_A || kp == RF_B {
        return Ok(());
    }
    rfk_parser(bus, t::RESTORE_NONDBCC_PATH01)
}

// Linux: _iqk_preset (rtw8852b_rfk.c:1493)
fn iqk_preset<B: Bus + ?Sized>(bus: &mut B, st: &RfkState, path: u32) -> Rtw89Result<()> {
    let idx = u32::from(st.mcc.table_idx);
    phy_mask32(bus, R_COEF_SEL + (path << 8), B_COEF_SEL_IQC, idx)?;
    phy_mask32(bus, R_CFIR_LUT + (path << 8), B_CFIR_LUT_G3, idx)?;
    rf_w(bus, path, RR_RSV1, RR_RSV1_RST, 0x0)?;
    rf_w(bus, path, RR_BBDC, RR_BBDC_SEL, 0x0)?;
    phy_mask32(bus, R_NCTL_RPT, MASKDWORD, 0x0000_0080)?;
    phy_mask32(bus, R_KIP_SYSCFG, MASKDWORD, 0x81ff_010a)?;
    debug_bb_read(bus, R_CFIR_LUT + (path << 8))?; // debug read: 0x8x54
    debug_bb_read(bus, R_COEF_SEL + (path << 8)) // debug read: 0x8x04
}

// Linux: _iqk_macbb_setting (rtw8852b_rfk.c:1514); 8852B: kpath is RF_AB, so the nondbcc table
fn iqk_macbb_setting<B: Bus + ?Sized>(bus: &mut B, phy_idx: u32) -> Rtw89Result<()> {
    let kp = kpath(phy_idx);
    if kp == RF_A || kp == RF_B {
        return Ok(());
    }
    rfk_parser(bus, t::SET_NONDBCC_PATH01)
}

// Linux: _iqk_init (rtw8852b_rfk.c:1538)
fn iqk_init<B: Bus + ?Sized>(bus: &mut B, st: &mut RfkState) -> Rtw89Result<()> {
    phy_mask32(bus, R_IQKINF, MASKDWORD, 0x0)?;
    let iqk = &mut st.iqk;
    if iqk.is_iqk_init {
        return Ok(());
    }
    iqk.is_iqk_init = true;
    iqk.is_nbiqk = false;
    iqk.iqk_fft_en = false;
    iqk.iqk_sram_en = false;
    iqk.iqk_cfir_en = false;
    iqk.iqk_xym_en = false;
    iqk.iqk_times = 0x0;
    for idx in 0..RTW89_IQK_CHS_NR as usize {
        iqk.iqk_channel[idx] = 0x0;
        for path in 0..IQK_SS {
            iqk.lok_cor_fail[idx][path] = false;
            iqk.lok_fin_fail[idx][path] = false;
            iqk.iqk_tx_fail[idx][path] = false;
            iqk.iqk_rx_fail[idx][path] = false;
            iqk.iqk_mcc_ch[idx][path] = 0x0;
            iqk.iqk_table_idx[path] = 0x0;
        }
    }
    Ok(())
}

// Linux: _doiqk (rtw8852b_rfk.c:1596)
fn doiqk<B: Bus + ?Sized>(
    bus: &mut B,
    st: &mut RfkState,
    chan: Option<&Chan>,
    hooks: &mut dyn RfkHooks<B>,
    phy_idx: u32,
    path: u32,
) -> Rtw89Result<()> {
    let phy_map = btc_phymap(chan, phy_idx, RF_AB)?;
    hooks.btc_wl_rfk(bus, phy_map, BTC_WRFKT_IQK, BTC_WRFK_ONESHOT_START)?;
    st.iqk.iqk_times = st.iqk.iqk_times.wrapping_add(1) & 0xff; // u8 in Linux
    st.iqk.version = IQK_VER;
    iqk_get_ch_info(bus, st, chan, path)?;
    let bb_backup = rfk_backup_bb_reg(bus)?;
    let rf_backup = rfk_backup_rf_reg(bus, path)?;
    iqk_macbb_setting(bus, phy_idx)?;
    iqk_preset(bus, st, path)?;
    iqk_by_path(bus, st, phy_idx, path)?;
    iqk_restore(bus, st, path)?;
    iqk_afebb_restore(bus, phy_idx)?;
    rfk_restore_bb_reg(bus, &bb_backup)?;
    rfk_restore_rf_reg(bus, &rf_backup, path)?;
    hooks.btc_wl_rfk(bus, phy_map, BTC_WRFKT_IQK, BTC_WRFK_ONESHOT_STOP)
}

// Linux: _iqk (rtw8852b_rfk.c:1628)
fn iqk_paths<B: Bus + ?Sized>(
    bus: &mut B,
    st: &mut RfkState,
    chan: Option<&Chan>,
    hooks: &mut dyn RfkHooks<B>,
    phy_idx: u32,
) -> Rtw89Result<()> {
    match kpath(phy_idx) {
        RF_A => doiqk(bus, st, chan, hooks, phy_idx, RF_PATH_A),
        RF_B => doiqk(bus, st, chan, hooks, phy_idx, RF_PATH_B),
        RF_AB => {
            doiqk(bus, st, chan, hooks, phy_idx, RF_PATH_A)?;
            doiqk(bus, st, chan, hooks, phy_idx, RF_PATH_B)
        }
        _ => Ok(()),
    }
}

// Linux: rtw8852b_iqk (rtw8852b_rfk.c:3761)
pub fn iqk<B: Bus + ?Sized>(
    bus: &mut B,
    st: &mut RfkState,
    chan: Option<&Chan>,
    hooks: &mut dyn RfkHooks<B>,
    phy_idx: u32,
) -> Rtw89Result<()> {
    let phy_map = btc_phymap(chan, phy_idx, 0)?;
    hooks.btc_wl_rfk(bus, phy_map, BTC_WRFKT_IQK, BTC_WRFK_START)?;
    let tx_en = hooks.stop_sch_tx(bus)?; // MAC 0: phy_idx 0 is mac_idx 0 here
    wait_rx_mode(bus, kpath(phy_idx), &mut st.events)?;
    iqk_init(bus, st)?;
    iqk_paths(bus, st, chan, hooks, phy_idx)?;
    hooks.resume_sch_tx(bus, tx_en)?;
    hooks.btc_wl_rfk(bus, phy_map, BTC_WRFKT_IQK, BTC_WRFK_STOP)?;
    bus.flush()?;
    Ok(())
}
