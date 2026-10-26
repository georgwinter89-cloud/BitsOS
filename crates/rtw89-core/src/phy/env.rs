//! The environment monitor of Linux phy.c on BB 0 (JS js/phy-env.mjs): IFS-CLM and
//! EDCCA-CLM reports of the CCX block, read and re-armed by rtw89_phy_env_monitor_track
//! every 2 s (the track work). The init is dm.rs's; the IFS threshold helpers here are its too.
//! rtw89_ccx_regs_ax (phy.c:8935) names the registers.

use crate::bus::{Bus, field};
use crate::error::Rtw89Result;
use crate::tables::regs::*;

use super::io::{phy_mask32, phy_r32};

const IFS_N: usize = RTW89_IFS_CLM_NUM as usize;
// phy.h
const PERCENT: u32 = 100;
const PERMIL: u32 = 1000;
const ENV_MNTR_IFSCLM_HIS_MAX: u32 = 127;
const ENV_MNTR_FAIL_DWORD: u32 = 0xffff_ffff;
const CCX_MAX_PERIOD: u32 = 2097;
const CCX_MAX_PERIOD_UNIT: u32 = 32;
const MS_TO_4US_RATIO: u32 = 250;
/// the track's monitor period (rtw89_phy_env_monitor_track: 1900 ms)
pub const ENV_MNTR_TIME_MS: u32 = 1900;

/// Linux: struct rtw89_env_monitor_info, the parts the 8852B uses (no NHM)
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Env {
    pub ccx_manual_ctrl: bool,
    pub ccx_ongoing: bool,
    pub ccx_rac_lv: u32,
    pub ccx_period: u32,
    pub ccx_unit_idx: u32,
    pub ccx_watchdog_result: u32,
    pub ifs_clm_app: u32,
    pub ifs_clm_mntr_time: u32,
    pub ifs_clm_th_l: [u32; IFS_N],
    pub ifs_clm_th_h: [u32; IFS_N],
    pub ifs_clm_tx: u32,
    pub ifs_clm_edcca_excl_cca: u32,
    pub ifs_clm_cckcca_excl_fa: u32,
    pub ifs_clm_ofdmcca_excl_fa: u32,
    pub ifs_clm_cckfa: u32,
    pub ifs_clm_ofdmfa: u32,
    pub ifs_clm_his: [u32; IFS_N],
    pub ifs_clm_avg: [u32; IFS_N],
    pub ifs_clm_cca: [u32; IFS_N],
    pub ifs_clm_total_ifs: u32,
    pub ifs_clm_tx_ratio: u32,
    pub ifs_clm_edcca_excl_cca_ratio: u32,
    pub ifs_clm_cck_fa_ratio: u32,
    pub ifs_clm_ofdm_fa_ratio: u32,
    pub ifs_clm_cck_cca_excl_fa_ratio: u32,
    pub ifs_clm_ofdm_cca_excl_fa_ratio: u32,
    pub ifs_clm_cck_fa_permil: u32,
    pub ifs_clm_ofdm_fa_permil: u32,
    pub ifs_clm_ifs_avg: [u32; IFS_N],
    pub ifs_clm_cca_avg: [u32; IFS_N],
    pub edcca_clm_ratio: u32,
}

// ilog2(CCX_US_BASE_RATIO) + the unit index; JS shifts count mod 32
fn us_shift(env: &Env) -> u32 {
    CCX_US_BASE_RATIO.ilog2().wrapping_add(env.ccx_unit_idx)
}
// rtw89_phy_ccx_us_to_idx (phy.c:6268)
fn us_to_idx(env: &Env, us: u32) -> u32 {
    us.wrapping_shr(us_shift(env)) & 0xffff
}
// rtw89_phy_ccx_idx_to_us (phy.c:6276)
fn idx_to_us(env: &Env, idx: u32) -> u32 {
    idx.wrapping_shl(us_shift(env))
}

// rtw89_phy_ccx_get_report (phy.c:6306)
fn get_report(env: &Env, report: u32, score: u32) -> u32 {
    let numer = u64::from(report) * u64::from(score) + u64::from(env.ccx_period >> 1);
    let ret = if env.ccx_period != 0 {
        (numer / u64::from(env.ccx_period)) as u32 & 0xffff
    } else {
        0
    };
    if ret >= score { score - 1 } else { ret }
}

// Linux: rtw89_phy_ifs_clm_th_update_check (phy.c:6363): the four regions' thresholds in
// index units; true when the application changed (INIT, BACKGROUND, ACS, DBG, DIG,
// TDMA_DIG all take IFS_CLM_TH0_UPPER and IFS_CLM_TH_MUL; no manual mode)
pub fn ifs_clm_th_update_check(env: &mut Env, app: u32) -> bool {
    if env.ifs_clm_app == app {
        return false;
    }
    let mut th_hus = IFS_CLM_TH0_UPPER;
    env.ifs_clm_th_l[0] = 0;
    env.ifs_clm_th_h[0] = us_to_idx(env, th_hus);
    for i in 1..IFS_N {
        env.ifs_clm_th_l[i] = (env.ifs_clm_th_h[i - 1] + 1) & 0xffff;
        th_hus = th_hus.wrapping_mul(IFS_CLM_TH_MUL);
        env.ifs_clm_th_h[i] = us_to_idx(env, th_hus);
    }
    true
}

const IFS_T: [(u32, u32, u32, u32); IFS_N] = [
    (
        R_IFS_T1,
        B_IFS_T1_TH_LOW_MSK,
        B_IFS_T1_TH_HIGH_MSK,
        B_IFS_T1_EN_MSK,
    ),
    (
        R_IFS_T2,
        B_IFS_T2_TH_LOW_MSK,
        B_IFS_T2_TH_HIGH_MSK,
        B_IFS_T2_EN_MSK,
    ),
    (
        R_IFS_T3,
        B_IFS_T3_TH_LOW_MSK,
        B_IFS_T3_TH_HIGH_MSK,
        B_IFS_T3_EN_MSK,
    ),
    (
        R_IFS_T4,
        B_IFS_T4_TH_LOW_MSK,
        B_IFS_T4_TH_HIGH_MSK,
        B_IFS_T4_EN_MSK,
    ),
];

// Linux: rtw89_phy_ifs_clm_set_th_reg (phy.c:6418)
pub fn ifs_clm_set_th_reg<B: Bus + ?Sized>(bus: &mut B, env: &Env) -> Rtw89Result<()> {
    for (i, (r, lo, _, _)) in IFS_T.iter().enumerate() {
        phy_mask32(bus, *r, *lo, env.ifs_clm_th_l[i])?;
    }
    for (i, (r, _, hi, _)) in IFS_T.iter().enumerate() {
        phy_mask32(bus, *r, *hi, env.ifs_clm_th_h[i])?;
    }
    Ok(())
}

// Linux: rtw89_phy_ifs_clm_setting_init (phy.c:6478)
pub fn ifs_clm_setting_init<B: Bus + ?Sized>(bus: &mut B, env: &mut Env) -> Rtw89Result<()> {
    env.ifs_clm_app = RTW89_IFS_CLM_BACKGROUND;
    env.ifs_clm_mntr_time = 0;
    if ifs_clm_th_update_check(env, RTW89_IFS_CLM_INIT) {
        ifs_clm_set_th_reg(bus, env)?;
    }
    phy_mask32(bus, R_IFS_COUNTER, B_IFS_COLLECT_EN, 1)?;
    for (r, _, _, en) in IFS_T {
        phy_mask32(bus, r, en, 1)?;
    }
    Ok(())
}

// rtw89_phy_ifs_clm_get_utility (phy.c:6564)
fn ifs_clm_get_utility(env: &mut Env) {
    env.ifs_clm_tx_ratio = get_report(env, env.ifs_clm_tx, PERCENT);
    env.ifs_clm_edcca_excl_cca_ratio = get_report(env, env.ifs_clm_edcca_excl_cca, PERCENT);
    env.ifs_clm_cck_fa_ratio = get_report(env, env.ifs_clm_cckfa, PERCENT);
    env.ifs_clm_ofdm_fa_ratio = get_report(env, env.ifs_clm_ofdmfa, PERCENT);
    env.ifs_clm_cck_cca_excl_fa_ratio = get_report(env, env.ifs_clm_cckcca_excl_fa, PERCENT);
    env.ifs_clm_ofdm_cca_excl_fa_ratio = get_report(env, env.ifs_clm_ofdmcca_excl_fa, PERCENT);
    env.ifs_clm_cck_fa_permil = get_report(env, env.ifs_clm_cckfa, PERMIL);
    env.ifs_clm_ofdm_fa_permil = get_report(env, env.ifs_clm_ofdmfa, PERMIL);
    for i in 0..IFS_N {
        let his = env.ifs_clm_his[i];
        env.ifs_clm_ifs_avg[i] = if his > ENV_MNTR_IFSCLM_HIS_MAX {
            ENV_MNTR_FAIL_DWORD
        } else {
            idx_to_us(env, env.ifs_clm_avg[i])
        };
        let res = idx_to_us(env, env.ifs_clm_cca[i]).wrapping_add(his >> 1);
        env.ifs_clm_cca_avg[i] = res.checked_div(his).unwrap_or(0);
    }
}

// rtw89_phy_ifs_clm_get_result (phy.c:6749): the done bit first, then the report registers
// in Linux's order
fn ifs_clm_get_result<B: Bus + ?Sized>(bus: &mut B, env: &mut Env) -> Rtw89Result<bool> {
    if field(phy_r32(bus, R_IFSCNT)?, B_IFSCNT_DONE_MSK) == 0 {
        return Ok(false);
    }
    let regs = [
        (R_IFS_CLM_TX_CNT, B_IFS_CLM_TX_CNT_MSK),
        (R_IFS_CLM_TX_CNT, B_IFS_CLM_EDCCA_EXCLUDE_CCA_FA_MSK),
        (R_IFS_CLM_CCA, B_IFS_CLM_CCKCCA_EXCLUDE_FA_MSK),
        (R_IFS_CLM_CCA, B_IFS_CLM_OFDMCCA_EXCLUDE_FA_MSK),
        (R_IFS_CLM_FA, B_IFS_CLM_CCK_FA_MSK),
        (R_IFS_CLM_FA, B_IFS_CLM_OFDM_FA_MSK),
        (R_IFS_HIS, B_IFS_T1_HIS_MSK),
        (R_IFS_HIS, B_IFS_T2_HIS_MSK),
        (R_IFS_HIS, B_IFS_T3_HIS_MSK),
        (R_IFS_HIS, B_IFS_T4_HIS_MSK),
        (R_IFS_AVG_L, B_IFS_T1_AVG_MSK),
        (R_IFS_AVG_L, B_IFS_T2_AVG_MSK),
        (R_IFS_AVG_H, B_IFS_T3_AVG_MSK),
        (R_IFS_AVG_H, B_IFS_T4_AVG_MSK),
        (R_IFS_CCA_L, B_IFS_T1_CCA_MSK),
        (R_IFS_CCA_L, B_IFS_T2_CCA_MSK),
        (R_IFS_CCA_H, B_IFS_T3_CCA_MSK),
        (R_IFS_CCA_H, B_IFS_T4_CCA_MSK),
        (R_IFSCNT, B_IFSCNT_TOTAL_CNT_MSK),
    ];
    let mut v = [0u32; 19];
    for (slot, (r, m)) in v.iter_mut().zip(regs) {
        *slot = field(phy_r32(bus, r)?, m);
    }
    env.ifs_clm_tx = v[0];
    env.ifs_clm_edcca_excl_cca = v[1];
    env.ifs_clm_cckcca_excl_fa = v[2];
    env.ifs_clm_ofdmcca_excl_fa = v[3];
    env.ifs_clm_cckfa = v[4];
    env.ifs_clm_ofdmfa = v[5];
    env.ifs_clm_his.copy_from_slice(&v[6..10]);
    env.ifs_clm_avg.copy_from_slice(&v[10..14]);
    env.ifs_clm_cca.copy_from_slice(&v[14..18]);
    env.ifs_clm_total_ifs = v[18];
    ifs_clm_get_utility(env);
    Ok(true)
}

// rtw89_phy_edcca_clm_get_result (phy.c:6850)
fn edcca_clm_get_result<B: Bus + ?Sized>(bus: &mut B, env: &mut Env) -> Rtw89Result<bool> {
    if field(phy_r32(bus, R_CLM_EDCCA_RDY)?, B_CLM_EDCCA_RDY) == 0 {
        return Ok(false);
    }
    let val = field(phy_r32(bus, R_CLM_EDCCA_RESULT)?, B_CLM_EDCCA_RESULT);
    env.edcca_clm_ratio = get_report(env, val, PERCENT);
    Ok(true)
}

// rtw89_phy_ccx_racing_ctrl (phy.c:6505): 0 (true) or -EINVAL (false)
fn racing_ctrl(env: &mut Env, level: u32) -> bool {
    if level >= RTW89_RAC_MAX_NUM {
        return false;
    }
    if env.ccx_ongoing {
        if level <= env.ccx_rac_lv {
            return false;
        }
        env.ccx_ongoing = false;
    }
    env.ccx_rac_lv = level;
    true
}

// rtw89_phy_ccx_ms_to_period_unit (phy.c:6321): (period, unit index)
fn ms_to_period_unit(ms: u32) -> (u32, u32) {
    let ms = ms.min(CCX_MAX_PERIOD);
    let quotient = (CCX_MAX_PERIOD_UNIT * ms / CCX_MAX_PERIOD) & 0xff;
    let unit = match quotient {
        0..4 => RTW89_CCX_4_US,
        4..8 => RTW89_CCX_8_US,
        8..16 => RTW89_CCX_16_US,
        _ => RTW89_CCX_32_US,
    };
    ((ms * MS_TO_4US_RATIO) >> unit, unit)
}

// rtw89_phy_ifs_clm_set (phy.c:6974); true where Linux returns 0
fn ifs_clm_set<B: Bus + ?Sized>(
    bus: &mut B,
    env: &mut Env,
    mntr_time: u32,
    rac_lv: u32,
    app: u32,
) -> Rtw89Result<bool> {
    if mntr_time == 0 || !racing_ctrl(env, rac_lv) {
        return Ok(false);
    }
    if mntr_time != env.ifs_clm_mntr_time {
        let (period, unit) = ms_to_period_unit(mntr_time);
        phy_mask32(bus, R_IFS_COUNTER, B_IFS_CLM_PERIOD_MSK, period)?;
        phy_mask32(bus, R_IFS_COUNTER, B_IFS_CLM_COUNTER_UNIT_MSK, unit)?;
        env.ifs_clm_mntr_time = mntr_time;
        env.ccx_period = period & 0xffff;
        env.ccx_unit_idx = unit;
    }
    if ifs_clm_th_update_check(env, app) {
        env.ifs_clm_app = app;
        ifs_clm_set_th_reg(bus, env)?;
    }
    Ok(true)
}

// rtw89_phy_ccx_trigger (phy.c:6538); no NHM (support_noise false)
fn ccx_trigger<B: Bus + ?Sized>(bus: &mut B, env: &mut Env) -> Rtw89Result<()> {
    phy_mask32(bus, R_IFS_COUNTER, B_IFS_COUNTER_CLR_MSK, 0)?;
    phy_mask32(bus, R_CCX, B_MEASUREMENT_TRIG_MSK, 0)?;
    phy_mask32(bus, R_IFS_COUNTER, B_IFS_COUNTER_CLR_MSK, 1)?;
    phy_mask32(bus, R_CCX, B_MEASUREMENT_TRIG_MSK, 1)?;
    env.ccx_ongoing = true;
    Ok(())
}

// Linux: rtw89_phy_env_monitor_track (phy.c:7058) -> __rtw89_phy_env_monitor_track
// (phy.c:7019), BB 0: last period's IFS-CLM and EDCCA-CLM, then a new 1900 ms period
pub fn env_monitor_track<B: Bus + ?Sized>(bus: &mut B, env: &mut Env) -> Rtw89Result<()> {
    env.ccx_watchdog_result = RTW89_PHY_ENV_MON_CCX_FAIL;
    if env.ccx_manual_ctrl {
        return Ok(());
    }
    if ifs_clm_get_result(bus, env)? {
        env.ccx_watchdog_result |= RTW89_PHY_ENV_MON_IFS_CLM;
    }
    if edcca_clm_get_result(bus, env)? {
        env.ccx_watchdog_result |= RTW89_PHY_ENV_MON_EDCCA_CLM;
    }
    // rtw89_phy_ccx_racing_release (phy.c:6350)
    env.ccx_ongoing = false;
    env.ccx_rac_lv = RTW89_RAC_RELEASE;
    env.ifs_clm_app = RTW89_IFS_CLM_BACKGROUND;
    let mut chk = RTW89_PHY_ENV_MON_CCX_FAIL;
    if ifs_clm_set(
        bus,
        env,
        ENV_MNTR_TIME_MS,
        RTW89_RAC_LV_1,
        RTW89_IFS_CLM_BACKGROUND,
    )? {
        chk |= RTW89_PHY_ENV_MON_IFS_CLM;
    }
    if chk != 0 {
        ccx_trigger(bus, env)?;
    }
    bus.flush()?;
    Ok(())
}
