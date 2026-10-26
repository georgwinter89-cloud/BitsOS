//! The 2-second tracking of Linux phy.c on BB 0 that follows the link (JS js/phy-dig.mjs):
//! the stations' RSSI minimum (rtw89_phy_stat_rssi_update), DIG (rtw89_phy_dig), EDCCA
//! (rtw89_phy_edcca_track) and the UL TB waveform (rtw89_phy_ul_tb_ctrl_track). The track
//! work runs them in rtw89_track_work's order; their init is dm.rs's. The one station is the
//! AP's link (managed mode).

use crate::bus::Bus;
use crate::error::Rtw89Result;
use crate::tables::regs::*;

use super::dm::{
    Dig, IGI_MAX_PERFORMANCE_MODE, RSSI_NOLINK, dig_dyn_pd_th, dig_update_para, sdagc_follow_pagc,
};
use super::env::Env;
use super::io::phy_mask32;
use super::{Chan, Phy};

// phy.c
const IGI_RSSI_MIN: u32 = 10;
const ABS_IGI_MIN: u32 = 0xc;
const IGI_OFFSET_MAX: u32 = 25;
const IGI_OFFSET_MUL: i32 = 2;
const DIAG_BB_PD_MAX_PERIOD: u32 = 5;
const DIAG_BB_EDCCA_RATIO_TH: u32 = 50;
const DIAG_BB_EDCCA_TX_TH: u32 = 10;

/// Linux: struct rtw89_phy_ch_info's RSSI minimum
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ChInfo {
    pub rssi_min: u32,
    pub rssi_min_macid: u32,
}

/// The AP link's RSSI as the RX path keeps it (rtwsta_link->avg_rssi, prev_rssi)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ApRssi {
    /// DECLARE_EWMA(rssi, 10, 16)'s internal value
    pub avg_rssi_internal: u32,
    pub mac_id: u32,
    pub prev_rssi: u32,
}

/// What rtw89_phy_ul_tb_ctrl_track looks at besides the counters
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct UlTbLink {
    pub total_sta_assoc: u32,
    /// the vif is a station (RTW89_WIFI_ROLE_STATION) and associated
    pub station_assoc: bool,
    /// rtwvif_link->def_tri_idx and dyn_tb_bedge_en (rtw89_phy_ul_tb_assoc)
    pub dyn_tb_bedge_en: bool,
    pub def_tri_idx: u32,
}

// Linux: rtw89_phy_stat_rssi_update (phy.c:5879) with __rtw89_phy_stat_rssi_update_iter
// (phy.c:5839) for the AP's link. Returns rssi_data.rssi_changed: the caller then notifies the
// coexistence (rtw89_btc_ntfy_wl_sta).
pub fn stat_rssi_update(phy: &mut Phy, ap: Option<&mut ApRssi>) -> bool {
    let ch = &mut phy.ch_info;
    ch.rssi_min = 0xff; // U8_MAX
    let Some(ap) = ap else {
        return false;
    };
    let cur = ap.avg_rssi_internal >> 10; // ewma_rssi_read
    if cur < ch.rssi_min {
        ch.rssi_min = cur;
        ch.rssi_min_macid = ap.mac_id;
    }
    if ap.prev_rssi == 0 {
        ap.prev_rssi = cur;
    } else if ap.prev_rssi.abs_diff(cur) > (3 << RSSI_FACTOR) {
        ap.prev_rssi = cur;
        return true;
    }
    false
}

// rtw89_phy_dig_igi_offset_by_env (phy.c:7459)
fn igi_offset_by_env(dig: &mut Dig, env: &Env) {
    let fa_ratio = env
        .ifs_clm_cck_fa_permil
        .wrapping_add(env.ifs_clm_ofdm_fa_permil)
        & 0xffff;
    let noisy = if fa_ratio < dig.fa_th[0] {
        RTW89_DIG_NOISY_LEVEL0
    } else if fa_ratio < dig.fa_th[1] {
        RTW89_DIG_NOISY_LEVEL1 as i32
    } else if fa_ratio < dig.fa_th[2] {
        RTW89_DIG_NOISY_LEVEL2 as i32
    } else if fa_ratio < dig.fa_th[3] {
        RTW89_DIG_NOISY_LEVEL3 as i32
    } else {
        RTW89_DIG_NOISY_LEVEL_MAX as i32
    };
    let ofst = if noisy == RTW89_DIG_NOISY_LEVEL0 && dig.fa_rssi_ofst < 2 {
        0
    } else {
        (dig.fa_rssi_ofst as i32).wrapping_add(noisy.wrapping_mul(IGI_OFFSET_MUL)) as u32 & 0xff
    };
    dig.fa_rssi_ofst = ofst.min(IGI_OFFSET_MAX);
    dig.noisy_lv = noisy;
}

// rtw89_phy_cal_igi_fa_rssi (phy.c:7744) with rtw89_phy_dig_diag (phy.c:7719, a counter)
fn cal_igi_fa_rssi(dig: &mut Dig, env: &Env, is_linked: bool) {
    igi_offset_by_env(dig, env);
    let igi_min = dig.igi_rssi.saturating_sub(IGI_RSSI_MIN) & 0xff;
    dig.dyn_igi_max = (igi_min + IGI_OFFSET_MAX).min(IGI_MAX_PERFORMANCE_MODE);
    dig.dyn_igi_min = igi_min.max(ABS_IGI_MIN);
    if dig.dyn_igi_max >= dig.dyn_igi_min {
        dig.igi_fa_rssi = dig.igi_fa_rssi.wrapping_add(dig.fa_rssi_ofst) & 0xff;
        dig.igi_fa_rssi = dig.igi_fa_rssi.clamp(dig.dyn_igi_min, dig.dyn_igi_max);
    } else {
        dig.igi_fa_rssi = dig.dyn_igi_max;
    }
    if dig.dyn_igi_max != dig.igi_fa_rssi || dig.noisy_lv < RTW89_DIG_NOISY_LEVEL_MAX as i32 {
        dig.dyn_pd_max_cnt = 0;
    } else {
        dig.dyn_pd_max_cnt = dig.dyn_pd_max_cnt.saturating_add(1);
        if dig.dyn_pd_max_cnt >= DIAG_BB_PD_MAX_PERIOD && is_linked {
            dig.diag_pd_cnt = dig.diag_pd_cnt.wrapping_add(1);
        }
    }
}

// Linux: rtw89_phy_dig (phy.c:7955) -> __rtw89_phy_dig (phy.c:7903), BB 0; one channel
// context (no MCC); support_igi is false on the 8852B (no IGI writes)
pub fn phy_dig<B: Bus + ?Sized>(
    bus: &mut B,
    phy: &mut Phy,
    total_sta_assoc: u32,
    chan: &Chan,
) -> Rtw89Result<()> {
    let is_linked = total_sta_assoc > 0;
    let dig = &mut phy.dig;
    if dig.bypass_dig {
        dig.bypass_dig = false;
        return Ok(());
    }
    // rtw89_phy_dig_update_rssi_info (phy.c:7297)
    dig.igi_rssi = if is_linked {
        (phy.ch_info.rssi_min >> 1) & 0xff
    } else {
        RSSI_NOLINK
    };
    if dig.pause_dig {
        return Ok(());
    }
    if dig.is_linked_pre != is_linked {
        // first connected / first disconnected
        dig_update_para(dig, chan, is_linked);
        dig.igi_fa_rssi = dig.igi_rssi;
    }
    dig.is_linked_pre = is_linked;
    cal_igi_fa_rssi(dig, &phy.env, is_linked);
    // rtw89_phy_dig_config_igi: support_igi false
    let (fa, en) = (dig.igi_fa_rssi, dig.dyn_pd_th_en);
    dig_dyn_pd_th(bus, dig, &phy.hal, chan, fa, en)?;
    let follow = dig.dyn_pd_th_en && dig.igi_fa_rssi > dig.dyn_pd_th_max;
    sdagc_follow_pagc(bus, follow)?;
    bus.flush()?;
    Ok(())
}

// Linux: rtw89_phy_edcca_track (phy.c:8853) -> __rtw89_phy_edcca_track (phy.c:8844), BB 0:
// rtw89_phy_edcca_thre_calc with rtw89_phy_edcca_get_thre_by_rssi (rtw8852b_edcca_regs);
// rtw89_phy_edcca_log only runs with debug output on; rtw89_phy_edcca_diag is a counter
pub fn edcca_track<B: Bus + ?Sized>(
    bus: &mut B,
    phy: &mut Phy,
    total_sta_assoc: u32,
    tx_throughput: u32,
) -> Rtw89Result<()> {
    let mut th = EDCCA_MAX;
    if total_sta_assoc > 0 {
        th = ((phy.ch_info.rssi_min >> 1)
            .wrapping_sub(RSSI_UNIT_CONVER)
            .wrapping_add(EDCCA_UNIT_CONVER)
            .wrapping_sub(EDCCA_TH_REF))
            & 0xff;
        th = th.max(EDCCA_TH_L2H_LB);
    }
    if th != phy.edcca_th_old {
        phy.edcca_th_old = th;
        phy_mask32(bus, R_SEG0R_EDCCA_LVL_V1, B_EDCCA_LVL_MSK0, th)?;
        phy_mask32(bus, R_SEG0R_EDCCA_LVL_V1, B_EDCCA_LVL_MSK1, th)?;
        phy_mask32(bus, R_SEG0R_EDCCA_LVL_V1, B_EDCCA_LVL_MSK3, th)?;
    }
    if tx_throughput > DIAG_BB_EDCCA_TX_TH && phy.env.edcca_clm_ratio > DIAG_BB_EDCCA_RATIO_TH {
        phy.diag_edcca_cnt = phy.diag_edcca_cnt.wrapping_add(1);
    }
    bus.flush()?;
    Ok(())
}

// Linux: rtw89_phy_ul_tb_ctrl_track (phy.c:5604) with rtw89_phy_ul_tb_ctrl_check (phy.c:5538)
// and rtw89_phy_ul_tb_waveform_ctrl (phy.c:5565); 8852B: ul_tb_waveform_ctrl true,
// ul_tb_pwr_diff false. `rx_tf_periodic`: the trigger frames of the last period.
pub fn ul_tb_ctrl_track<B: Bus + ?Sized>(
    bus: &mut B,
    phy: &mut Phy,
    link: &UlTbLink,
    rx_tf_periodic: u32,
) -> Rtw89Result<()> {
    if link.total_sta_assoc != 1 || !link.station_assoc {
        return Ok(());
    }
    let high = rx_tf_periodic > UL_TB_TF_CNT_L2H_TH;
    let low = !high && rx_tf_periodic < UL_TB_TF_CNT_H2L_TH;
    let info = phy.ul_tb;
    if link.dyn_tb_bedge_en {
        if high {
            phy_mask32(bus, R_BANDEDGE, B_BANDEDGE_EN, 0)?;
        } else if low {
            phy_mask32(bus, R_BANDEDGE, B_BANDEDGE_EN, info.def_if_bandedge)?;
        }
    }
    if info.dyn_tb_tri_en {
        if high {
            phy_mask32(bus, R_DCFO_OPT, B_TXSHAPE_TRIANGULAR_CFG, 0)?;
        } else if low {
            phy_mask32(bus, R_DCFO_OPT, B_TXSHAPE_TRIANGULAR_CFG, link.def_tri_idx)?;
        }
    }
    bus.flush()?;
    Ok(())
}
