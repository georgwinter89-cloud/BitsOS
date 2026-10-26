//! The parts of Linux rtw89_phy_dm_init (phy.c:8235) that touch the chip, for BB 0 (dbcc
//! off): CCX/IFS-CLM, PHY status IEs, DIG, EDCCA, UL TB and the NCTL pre-init (JS
//! js/phy-dm.mjs). Thermal and CFO are trk.rs's, the IFS threshold helpers env.rs's.

use crate::bus::{Bus, field};
use crate::error::Rtw89Result;
use crate::tables::phy::{FA_TH_2G, FA_TH_5G, FA_TH_NOLINK, IGI_RSSI_TH};
use crate::tables::regs::*;

use super::env::{Env, ifs_clm_setting_init};
use super::io::{phy_mask32, phy_r32, phy_set32, phy_w32};
use super::{Band, Chan, Hal};

// DIG constants of phy.c (static const) and phy.h
pub const RSSI_NOLINK: u32 = 22;
const PD_LOW_TH_OFFSET: u32 = 16;
const DYNAMIC_IGI_MIN: u32 = 0x20;
pub const IGI_MAX_PERFORMANCE_MODE: u32 = 0x5a;

/// struct rtw89_agc_gaincode_set
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Gaincode {
    pub lna_idx: u32,
    pub tia_idx: u32,
    pub rxb_idx: u32,
}

/// Linux: struct rtw89_dig_info, the parts the 8852B uses (support_igi false)
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Dig {
    pub igi_rssi: u32,
    pub igi_fa_rssi: u32,
    pub fa_rssi_ofst: u32,
    pub bypass_dig: bool,
    pub pause_dig: bool,
    pub cur_gaincode: Gaincode,
    pub force_gaincode: Gaincode,
    pub force_gaincode_idx_en: bool,
    pub dyn_igi_max: u32,
    pub dyn_igi_min: u32,
    pub dyn_pd_th_max: u32,
    pub dyn_pd_max_cnt: u32,
    pub dyn_pd_th_en: bool,
    pub pd_low_th_ofst: u32,
    pub is_linked_pre: bool,
    /// lna_gain_g (2.4 GHz) or lna_gain_a
    pub lna_gain_is_g: bool,
    pub fa_th: [u32; 4],
    pub igi_rssi_th: [u32; 5],
    /// the OFDM PD value last written (rtw89_phy_dig_dyn_pd_th's bak_dig)
    pub bak_dig: i32,
    /// enum rtw89_dig_noisy_level (LEVEL0 is -1)
    pub noisy_lv: i32,
    /// rtw89_phy_dig_diag: how often the PD stayed at its maximum (a counter)
    pub diag_pd_cnt: u32,
}

/// Linux: struct rtw89_phy_ul_tb_info
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct UlTb {
    pub dyn_tb_tri_en: bool,
    pub def_if_bandedge: u32,
}

// Linux: rtw89_phy_ccx_top_setting_init (phy.c:6284), rtw89_ccx_regs_ax
fn ccx_top_setting_init<B: Bus + ?Sized>(bus: &mut B, env: &mut Env) -> Rtw89Result<()> {
    env.ccx_manual_ctrl = false;
    env.ccx_ongoing = false;
    env.ccx_rac_lv = RTW89_RAC_RELEASE;
    env.ccx_period = 0;
    env.ccx_unit_idx = RTW89_CCX_32_US;
    phy_mask32(bus, R_CCX, B_CCX_EN_MSK, 1)?;
    phy_mask32(bus, R_CCX, B_CCX_TRIG_OPT_MSK, 1)?;
    phy_mask32(bus, R_CCX, B_MEASUREMENT_TRIG_MSK, 1)?;
    phy_mask32(bus, R_CCX, B_CCX_EDCCA_OPT_MSK, RTW89_CCX_EDCCA_BW20_0)
}

// Linux: rtw89_phy_env_monitor_init (phy.c:8185): a fresh monitor
pub fn env_monitor_init<B: Bus + ?Sized>(bus: &mut B, env: &mut Env) -> Rtw89Result<()> {
    *env = Env::default();
    ccx_top_setting_init(bus, env)?;
    ifs_clm_setting_init(bus, env)
}

// rtw89_physts_ie_page_valid (phy.c:7066) + rtw89_phy_get_ie_bitmap_addr (phy.c:7083), AX
fn ie_bitmap_addr(page: u32) -> Option<u32> {
    if page >= RTW89_PHYSTS_BITMAP_NUM || page == RTW89_RSVD_9 {
        return None;
    }
    let mut page = page;
    if page > RTW89_RSVD_9 && page < RTW89_EHT_PKT {
        page -= 1;
    }
    if page == RTW89_EHT_PKT {
        return None; // the 8852B is AX: no EHT page
    }
    Some(R_PHY_STS_BITMAP_ADDR_START + (page << 2))
}

// Linux: __rtw89_physts_parsing_init (phy.c:7157), not in monitor mode; AX skips
// enable_hdr_2. sta's rtw89_ops_config (CHANGE_MONITOR) runs it again.
pub fn physts_parsing_init<B: Bus + ?Sized>(bus: &mut B) -> Rtw89Result<()> {
    phy_set32(bus, R_PLCP_HISTOGRAM, B_STS_DIS_TRIG_BY_FAIL)?; // enable_fail_report(false)
    phy_set32(bus, R_PLCP_HISTOGRAM, B_STS_DIS_TRIG_BY_BRK)?;
    let ie = |n: u32| 1u32 << n;
    for i in 0..RTW89_PHYSTS_BITMAP_NUM {
        if i == RTW89_RSVD_9 || i == RTW89_EHT_PKT {
            continue;
        }
        let Some(addr) = ie_bitmap_addr(i) else {
            continue;
        };
        let mut val = phy_r32(bus, addr)?;
        if i == RTW89_HE_MU || i == RTW89_VHT_MU {
            val |= ie(RTW89_PHYSTS_IE13_DL_MU_DEF);
        } else if i == RTW89_TRIG_BASE_PPDU {
            val |= ie(RTW89_PHYSTS_IE13_DL_MU_DEF) | ie(RTW89_PHYSTS_IE01_CMN_OFDM);
        } else if i >= RTW89_CCK_PKT {
            val &= !(ie(RTW89_PHYSTS_IE04_CMN_EXT_PATH_A)
                | ie(RTW89_PHYSTS_IE05_CMN_EXT_PATH_B)
                | ie(RTW89_PHYSTS_IE06_CMN_EXT_PATH_C)
                | ie(RTW89_PHYSTS_IE07_CMN_EXT_PATH_D));
            if i == RTW89_CCK_PKT {
                val |= ie(RTW89_PHYSTS_IE01_CMN_OFDM);
            } else if i >= RTW89_HT_PKT {
                val |= ie(RTW89_PHYSTS_IE20_DBG_OFDM_FD_USER_SEG_0);
            }
        }
        phy_mask32(bus, addr, 0xffff_ffff, val)?; // rtw89_phy_write32_idx(MASKDWORD)
    }
    Ok(())
}

// rtw89_phy_dig_cal_under_region (phy.c:7589)
fn dig_under_region(dig: &Dig, chan: &Chan) -> u32 {
    let bw = match chan.bw {
        40 => PD_TH_BW40_CMP_VAL,
        80 => PD_TH_BW80_CMP_VAL,
        160 => PD_TH_BW160_CMP_VAL,
        _ => PD_TH_BW20_CMP_VAL,
    };
    dig.pd_low_th_ofst
        .wrapping_add(PD_TH_SB_FLTR_CMP_VAL)
        .wrapping_add(bw)
        & 0xff
}

// Linux: rtw89_phy_dig_dyn_pd_th (phy.c:7651) with __rtw89_phy_dig_dyn_pd_th;
// rtw8852b_dig_regs, hal.support_cckpd from rtw89_core_setup_phycap; no DM bit disabled
pub fn dig_dyn_pd_th<B: Bus + ?Sized>(
    bus: &mut B,
    dig: &mut Dig,
    hal: &Hal,
    chan: &Chan,
    rssi: u32,
    enable: bool,
) -> Rtw89Result<()> {
    let under = dig_under_region(dig, chan) as i64;
    dig.dyn_pd_th_max = dig.igi_rssi;
    let final_rssi = i64::from(rssi.min(dig.igi_rssi));
    let lo = i64::from(PD_TH_MIN_RSSI) + under;
    let hi = i64::from(PD_TH_MAX_RSSI) + under;
    let ofdm_cca_th = (final_rssi.max(lo).min(hi) & 0xff) as i32;
    let pd_val = if enable {
        (ofdm_cca_th - under as i32 - PD_TH_MIN_RSSI as i32) >> 1
    } else {
        0
    };
    dig.bak_dig = pd_val;
    phy_mask32(
        bus,
        R_SEG0R_PD_V1,
        B_SEG0R_PD_LOWER_BOUND_MSK,
        pd_val as u32,
    )?;
    phy_mask32(
        bus,
        R_SEG0R_PD_V1,
        B_SEG0R_PD_SPATIAL_REUSE_EN_MSK_V1,
        u32::from(enable),
    )?;
    if !hal.support_cckpd {
        return Ok(());
    }
    let final_rssi = rssi.min(dig.igi_rssi);
    let under = dig_under_region(dig, chan);
    // max_t(s8, ...)
    let cck_cca_th = i32::from(final_rssi.wrapping_sub(under) as u8 as i8).max(CCKPD_TH_MIN_RSSI);
    let pd_val = (cck_cca_th - IGI_RSSI_MAX as i32) as u32;
    phy_mask32(
        bus,
        R_BMODE_PDTH_EN_V1,
        B_BMODE_PDTH_LIMIT_EN_MSK_V1,
        u32::from(enable),
    )?;
    phy_mask32(
        bus,
        R_BMODE_PDTH_V1,
        B_BMODE_PDTH_LOWER_BOUND_MSK_V1,
        pd_val,
    )
}

// Linux: rtw89_phy_dig_sdagc_follow_pagc_config (phy.c:7549), AX, rtw8852b_dig_regs
pub fn sdagc_follow_pagc<B: Bus + ?Sized>(bus: &mut B, en: bool) -> Rtw89Result<()> {
    let v = u32::from(en);
    phy_mask32(
        bus,
        R_PATH0_P20_FOLLOW_BY_PAGCUGC_V2,
        B_PATH0_P20_FOLLOW_BY_PAGCUGC_EN_MSK,
        v,
    )?;
    phy_mask32(
        bus,
        R_PATH0_S20_FOLLOW_BY_PAGCUGC_V2,
        B_PATH0_S20_FOLLOW_BY_PAGCUGC_EN_MSK,
        v,
    )?;
    phy_mask32(
        bus,
        R_PATH1_P20_FOLLOW_BY_PAGCUGC_V2,
        B_PATH1_P20_FOLLOW_BY_PAGCUGC_EN_MSK,
        v,
    )?;
    phy_mask32(
        bus,
        R_PATH1_S20_FOLLOW_BY_PAGCUGC_V2,
        B_PATH1_S20_FOLLOW_BY_PAGCUGC_EN_MSK,
        v,
    )
}

// Linux: rtw89_phy_dig_update_para (phy.c:7312); no registers
pub fn dig_update_para(dig: &mut Dig, chan: &Chan, linked: bool) {
    let g = chan.band == Band::B2g;
    dig.lna_gain_is_g = g;
    let th: &[u16] = match (linked, g) {
        (true, true) => FA_TH_2G,
        (true, false) => FA_TH_5G,
        (false, _) => FA_TH_NOLINK,
    };
    for (d, s) in dig.fa_th.iter_mut().zip(th) {
        *d = u32::from(*s);
    }
    dig.force_gaincode_idx_en = !g;
    dig.dyn_pd_th_en = true;
    for (d, s) in dig.igi_rssi_th.iter_mut().zip(IGI_RSSI_TH) {
        *d = u32::from(*s);
    }
}

// Linux: __rtw89_phy_dig_init (phy.c:7369) with rtw89_phy_dig_update_gain_para (support_igi is
// false on the 8852B: no reads) and rtw89_phy_dig_reset. igi_rssi, igi_fa_rssi and
// fa_rssi_ofst keep their values (bb->dig is zeroed once, at allocation).
pub fn dig_init<B: Bus + ?Sized>(
    bus: &mut B,
    dig: &mut Dig,
    hal: &Hal,
    chan: &Chan,
    linked: bool,
) -> Rtw89Result<()> {
    dig.bypass_dig = false;
    // rtw89_phy_dig_para_reset
    dig.cur_gaincode = Gaincode {
        lna_idx: LNA_IDX_MAX,
        tia_idx: TIA_IDX_MAX,
        rxb_idx: RXB_IDX_MAX,
    };
    dig.force_gaincode = dig.cur_gaincode;
    dig.dyn_igi_max = IGI_MAX_PERFORMANCE_MODE;
    dig.dyn_igi_min = DYNAMIC_IGI_MIN;
    dig.dyn_pd_th_max = 0;
    dig.dyn_pd_max_cnt = 0;
    dig.pd_low_th_ofst = PD_LOW_TH_OFFSET;
    dig.is_linked_pre = false;
    // rtw89_phy_dig_set_igi_cr: support_igi is false on the 8852B
    dig_dyn_pd_th(bus, dig, hal, chan, RSSI_NOLINK, false)?;
    sdagc_follow_pagc(bus, false)?;
    dig_update_para(dig, chan, linked);
    Ok(())
}

// Linux: __rtw89_phy_edcca_init (phy.c:8201), rtw8852b_edcca_regs
pub fn edcca_init<B: Bus + ?Sized>(bus: &mut B) -> Rtw89Result<()> {
    phy_mask32(bus, R_TX_COLLISION_T2R_ST, B_TX_COLLISION_T2R_ST_M, 0x29)
}

// Linux: rtw89_phy_ul_tb_info_init (phy.c:5628), ul_tb_waveform_ctrl is true
pub fn ul_tb_info_init<B: Bus + ?Sized>(bus: &mut B) -> Rtw89Result<UlTb> {
    Ok(UlTb {
        dyn_tb_tri_en: true,
        def_if_bandedge: field(phy_r32(bus, R_BANDEDGE)?, B_BANDEDGE_EN),
    })
}

/// The NCTL engine's handshake register (rtw89_phy_nctl_poll)
pub const NCTL_POLL_REG: u32 = 0x8080;

// Linux: rtw89_phy_preinit_rf_nctl_ax (phy.c:2041) with rtw89_phy_nctl_poll: write 4 to
// 0x8080, read it back, every 10 us for 1000 us (read_poll_timeout: 101 rounds stand for its
// 1 ms of wall time, no sleep after the last read). `Some(last read)` is the timeout Linux only
// logs.
pub fn preinit_rf_nctl<B: Bus + ?Sized>(bus: &mut B) -> Rtw89Result<Option<u32>> {
    phy_set32(bus, R_IOQ_IQK_DPK, 0x3)?; // IQK/DPK clock & reset
    phy_set32(bus, R_GNT_BT_WGT_EN, 0x1)?;
    phy_set32(bus, R_P0_PATH_RST, 0x800_0000)?;
    phy_set32(bus, R_P1_PATH_RST, 0x800_0000)?;
    phy_set32(bus, R_IOQ_IQK_DPK, 0x2)?; // 8852B
    phy_w32(bus, R_NCTL_CFG, 0x8)?;
    let mut us = 0;
    loop {
        phy_w32(bus, NCTL_POLL_REG, 0x4)?;
        bus.delay_us(1)?;
        let v = phy_r32(bus, NCTL_POLL_REG)?;
        if v == 0x4 {
            return Ok(None);
        }
        if us >= 1000 {
            return Ok(Some(v));
        }
        bus.delay_us(10)?;
        us += 10;
    }
}
