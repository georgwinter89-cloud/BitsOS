//! The DPK flow of Linux rtw8852b_rfk.c 2408-2717 (JS js/rfk2-dpkflow.mjs): the calibration per
//! path with backup and restore around it, the bypass decision, the thermal tracking of the DPD
//! power scaling, and the DPD backoff of rtw8852b_dpk_init. The single steps are in `dpk`.

use alloc::vec::Vec;

use crate::bus::{Bus, field};
use crate::error::Rtw89Result;
use crate::phy::Phy;
use crate::phy::io::{phy_mask32, phy_r32, phy_r32_mask, rf_r, rf_w};
use crate::rfk1::util::{
    RF_PATH_NUM_8852B, RfkChan, kpath, rfk_backup_bb_reg, rfk_backup_rf_reg, rfk_drf_direct_cntrl,
    rfk_restore_bb_reg, rfk_restore_rf_reg, rfk_rf_direct_cntrl,
};
use crate::tables::regs::*;
use crate::tables::rfk2::KIP_REG;

use super::dpk::*;
use super::{DPK_BKUP_NUM, Rfk2Event, Rfk2State, Steps, phy_ok, s8};

/// RTW8852B_DPK_RF_PATH
const DPK_RF_PATH: u32 = 2;

// Linux: _dpk_reload_check (rtw8852b_rfk.c:2408)
fn dpk_reload_check<B: Bus + ?Sized>(
    bus: &mut B,
    st: &mut Rfk2State,
    path: u32,
    c: &RfkChan,
) -> Rtw89Result<bool> {
    let p = crate::rfk1::path_idx(path)?;
    let mut is_reload = false;
    for idx in 0..DPK_BKUP_NUM {
        let bp = st.dpk.bp[p][idx];
        if c.band_type != bp.band || c.channel != bp.ch {
            continue;
        }
        phy_mask32(bus, R_COEF_SEL + (path << 8), B_COEF_SEL_MDPD, idx as u32)?;
        st.dpk.cur_idx[p] = idx as u32;
        is_reload = true;
    }
    Ok(is_reload)
}

// Linux: _dpk_main (rtw8852b_rfk.c:2435): true when the path failed
fn dpk_main<B: Bus + ?Sized>(
    bus: &mut B,
    st: &mut Rfk2State,
    phy: u32,
    path: u32,
    gain: u32,
    c: &RfkChan,
) -> Rtw89Result<bool> {
    let kidx = st.dpk.slot(path)?;
    let mut txagc = 0x38;
    rfk_rf_direct_cntrl(bus, path, false)?;
    rfk_drf_direct_cntrl(bus, path, false)?;
    dpk_kip_pwr_clk_on(bus, path)?;
    dpk_kip_set_txagc(bus, st, phy, path, txagc)?;
    dpk_rf_setting(bus, st, gain, path, kidx)?;
    dpk_rx_dck(bus, phy, path)?;
    dpk_kip_preset(bus, st, phy, path, kidx)?;
    dpk_kip_set_rxagc(bus, st, phy, path)?;
    dpk_table_select(bus, path, kidx, gain)?;
    txagc = dpk_agc(bus, st, phy, path, kidx, txagc, false, c)?;
    let is_fail = txagc == DPK_TXAGC_INVAL;
    if is_fail {
        let p = path as usize; // slot() checked the path
        st.push(Rfk2Event::DpkFailed {
            path,
            kidx: kidx as u32,
            corr_val: st.dpk.corr_val[p][kidx],
            dc_i: st.dpk.dc_i[p][kidx],
            dc_q: st.dpk.dc_q[p][kidx],
        });
    } else {
        dpk_get_thermal(bus, st, kidx, path)?;
        dpk_idl_mpa(bus, st, phy, path, kidx)?;
        rf_w(bus, path, RR_MOD, RR_MOD_MASK, RR_MOD_V_RX)?;
        dpk_fill_result(bus, st, phy as usize, path, kidx, gain, txagc)?;
    }
    st.dpk.at(path, kidx)?.path_ok = !is_fail;
    Ok(is_fail)
}

// Linux: _dpk_cal_select (rtw8852b_rfk.c:2484). Not Linux: when a step fails after the BB
// backup, the BB/AFE, KIP and RF registers saved so far are put back and the TSSI pause is
// lifted (best effort) before the error goes up.
fn dpk_cal_select<B: Bus + ?Sized>(
    bus: &mut B,
    phy_st: &Phy,
    st: &mut Rfk2State,
    _force: bool,
    phy: u32,
    c: &RfkChan,
) -> Rtw89Result<()> {
    if st.dpk.is_dpk_reload_en {
        for path in 0..DPK_RF_PATH {
            let reloaded = dpk_reload_check(bus, st, path, c)?;
            let p = path as usize;
            if !reloaded && st.dpk.bp[p][0].ch != 0 {
                st.dpk.cur_idx[p] = if st.dpk.cur_idx[p] != 0 { 0 } else { 1 };
            } else {
                dpk_onoff(bus, st, path, false)?;
            }
        }
    } else {
        st.dpk.cur_idx = [0; 2];
    }
    let backup_bb = rfk_backup_bb_reg(bus)?;
    let mut kip_bkup: Vec<Vec<u32>> = Vec::new();
    let mut backup_rf: Vec<Vec<u32>> = Vec::new();
    let r = dpk_cal_paths(bus, phy_st, st, phy, c, &mut kip_bkup, &mut backup_rf);
    let mut steps = Steps::after(st, r);
    steps.step(bus, "dpk bb afe restore", |b| dpk_bb_afe_restore(b, c));
    steps.step(bus, "dpk bb restore", |b| rfk_restore_bb_reg(b, &backup_bb));
    // after a failure only the paths whose KIP and RF registers were saved
    for (path, (kip, rf)) in (0..DPK_RF_PATH).zip(kip_bkup.iter().zip(&backup_rf)) {
        steps.step(bus, "dpk kip restore", |b| dpk_kip_restore(b, path));
        steps.step(bus, "dpk kip reload", |b| {
            dpk_reload_kip(b, KIP_REG, kip, path)
        });
        steps.step(bus, "dpk rf restore", |b| rfk_restore_rf_reg(b, rf, path));
        if phy_st.tssi_mode[path as usize] {
            steps.step(bus, "dpk tssi resume", |b| dpk_tssi_pause(b, path, false));
        }
    }
    steps.finish()
}

// _dpk_cal_select between the BB backup and the restore: the KIP/RF backups (kept in
// `kip_bkup`/`backup_rf` as far as they got) and the calibration of both paths
fn dpk_cal_paths<B: Bus + ?Sized>(
    bus: &mut B,
    phy_st: &Phy,
    st: &mut Rfk2State,
    phy: u32,
    c: &RfkChan,
    kip_bkup: &mut Vec<Vec<u32>>,
    backup_rf: &mut Vec<Vec<u32>>,
) -> Rtw89Result<()> {
    for path in 0..DPK_RF_PATH {
        let kip = dpk_bkup_kip(bus, KIP_REG, path)?;
        let rf = rfk_backup_rf_reg(bus, path)?;
        kip_bkup.push(kip);
        backup_rf.push(rf);
        dpk_information(st, path, c)?;
        if phy_st.tssi_mode[path as usize] {
            dpk_tssi_pause(bus, path, true)?;
        }
    }
    dpk_bb_afe_setting(bus, c)?;
    for path in 0..DPK_RF_PATH {
        let is_fail = dpk_main(bus, st, phy, path, 1, c)?;
        dpk_onoff(bus, st, path, is_fail)?;
    }
    Ok(())
}

// Linux: _dpk_bypass_check (rtw8852b_rfk.c:2539). Linux never fills rtwdev->fem for the 8852B
// (no external PA); 6 GHz is not used.
fn dpk_bypass_check(st: &Rfk2State, c: &RfkChan) -> bool {
    (st.fem.epa_2g && c.band_type == RTW89_BAND_2G)
        || (st.fem.epa_5g && c.band_type == RTW89_BAND_5G)
}

// Linux: _dpk_force_bypass (rtw8852b_rfk.c:2562)
fn dpk_force_bypass<B: Bus + ?Sized>(bus: &mut B, st: &Rfk2State, phy: u32) -> Rtw89Result<()> {
    let kp = kpath(phy);
    for path in 0..DPK_RF_PATH {
        if kp & (1 << path) != 0 {
            dpk_onoff(bus, st, path, true)?;
        }
    }
    Ok(())
}

// Linux: _dpk (rtw8852b_rfk.c:2574)
pub fn dpk_run<B: Bus + ?Sized>(
    bus: &mut B,
    phy_st: &Phy,
    st: &mut Rfk2State,
    phy: u32,
    force: bool,
    c: &RfkChan,
) -> Rtw89Result<()> {
    phy_ok(phy)?;
    if dpk_bypass_check(st, c) {
        st.push(Rfk2Event::DpkSkipped { band: c.band_type });
        dpk_force_bypass(bus, st, phy)
    } else {
        dpk_cal_select(bus, phy_st, st, force, phy, c)
    }
}

// Linux: _dpk_track (rtw8852b_rfk.c:2588). cur_ther is ewma_thermal_read(&rtwdev->phystat.
// avg_thermal[path]): phy keeps that average (`Phy::avg_thermal`, 0 = none yet, as Linux).
// ini_diff keeps its value from path A to path B, as the C declares it outside the loop.
pub fn dpk_track_run<B: Bus + ?Sized>(
    bus: &mut B,
    phy: &Phy,
    st: &mut Rfk2State,
) -> Rtw89Result<()> {
    for path in 0..RF_PATH_NUM_8852B {
        st.dpk.slot(path)?; // both slots in range before the first access
    }
    let mut ini_diff: i32 = 0; // s8
    for path in 0..RF_PATH_NUM_8852B {
        let p = path as usize;
        let kidx = st.dpk.slot(path)?;
        let bp = st.dpk.bp[p][kidx];
        let cur_ther = (phy.avg_thermal[p] & 0xff) as i32;
        let mut delta_ther: i32 = 0;
        if bp.ch != 0 && cur_ther != 0 {
            delta_ther = s8((bp.ther_dpk as i32).wrapping_sub(cur_ther));
        }
        delta_ther = if bp.band == RTW89_BAND_2G {
            s8(delta_ther * 3 / 2)
        } else {
            s8(delta_ther * 5 / 2)
        };
        let txagc_bb_reg = R_TXAGC_BB + (path << 13);
        let txagc_rf = field(phy_r32(bus, txagc_bb_reg)?, 0x0000_003f);
        let (pwsf0, pwsf1, dis);
        if phy.tssi_mode[p] {
            rf_r(bus, path, RR_TXA, RR_TXA_TRK)?; // trk_idx, only logged
            let v = [
                phy_r32(bus, txagc_bb_reg)?,
                phy_r32(bus, R_TXAGC_TP + (path << 13))?,
                phy_r32(bus, txagc_bb_reg)?,
                phy_r32(bus, R_DPD_COM + (path << 8))?,
                phy_r32(bus, R_P0_TXDPD + (path << 13))?,
                phy_r32(bus, R_DPK_TRK)?,
            ];
            let txagc_bb = s8(field(v[0], MASKBYTE2) as i32);
            let txagc_bb_tp = s8(field(v[1], B_TXAGC_TP) as i32);
            let mut txagc_ofst = s8(field(v[2], MASKBYTE3) as i32);
            if field(v[3], B_DPD_COM_OF) == 0x1 {
                txagc_ofst = 0;
            }
            if txagc_rf != 0 && cur_ther != 0 {
                ini_diff = s8(txagc_ofst + delta_ther);
            }
            let pw = bp.pwsf as i32;
            pwsf0 = if field(v[4], B_P0_TXDPD) == 0x0 {
                pw.wrapping_add(txagc_bb_tp)
                    .wrapping_sub(txagc_bb)
                    .wrapping_add(ini_diff) as u32
                    & 0xffff
            } else {
                pw.wrapping_add(ini_diff) as u32 & 0xffff
            };
            pwsf1 = pwsf0;
            dis = field(v[5], B_DPK_TRK_DIS);
        } else {
            pwsf0 = ((bp.pwsf as i32).wrapping_add(delta_ther)) as u32 & 0x1ff;
            pwsf1 = pwsf0;
            dis = phy_r32_mask(bus, R_DPK_TRK, B_DPK_TRK_DIS)?;
        }
        if dis == 0 && txagc_rf != 0 {
            let a = R_DPD_BND + (path << 8) + ((kidx as u32) << 2);
            phy_mask32(bus, a, B_DPD_BND_0, pwsf0)?;
            phy_mask32(bus, a, B_DPD_BND_1, pwsf1)?;
        }
    }
    Ok(())
}

// Linux: _set_dpd_backoff (rtw8852b_rfk.c:2692)
pub fn set_dpd_backoff<B: Bus + ?Sized>(
    bus: &mut B,
    st: &mut Rfk2State,
    phy: u32,
) -> Rtw89Result<()> {
    let ph = phy_ok(phy)?;
    let kp = kpath(phy);
    let a = R_DPD_BF + (phy << 13);
    let bf = phy_r32(bus, a)?;
    let bf2 = phy_r32(bus, a)?; // Linux reads it twice
    let ofdm_bkof = field(bf, B_DPD_BF_OFDM);
    let tx_scale = field(bf2, B_DPD_BF_SCA);
    if ofdm_bkof + tx_scale >= 44 {
        // move dpd backoff to bb, and set dpd backoff to 0
        st.dpk.dpk_gs[ph] = 0x7f;
        for path in 0..RF_PATH_NUM_8852B {
            if kp & (1 << path) == 0 {
                continue;
            }
            phy_mask32(bus, R_DPD_CH0A + (path << 8), B_DPD_CFG, 0x7f7f7f)?;
        }
    } else {
        st.dpk.dpk_gs[ph] = 0x5b;
    }
    Ok(())
}
