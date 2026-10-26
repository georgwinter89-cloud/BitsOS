//! The thermal average and the CFO tracking of Linux phy.c (JS js/phy-trk.mjs:
//! rtw89_phy_stat_thermal_update, rtw89_phy_cfo_*): their init parts run in phy_dm_init, the
//! track parts belong to the track work (every 2 s) and to the RX path (rtw89_phy_cfo_parse
//! per PPDU status). `total_sta_assoc` is rtwdev->total_sta_assoc, kept by the station layer.

use alloc::vec;
use alloc::vec::Vec;

use crate::bringup::power::{xtal_read, xtal_write};
use crate::bus::Bus;
use crate::error::Rtw89Result;
use crate::tables::regs::*;

use super::bbx::get_thermal;
use super::efuse::RF_PATHS;
use super::io::{phy_mask32, phy_r32_mask};
use super::{Phy, PhyEvent};

/// rtw8852b_chip_info.dcfo_comp_sft (rtw8852b.c:1071)
const DCFO_COMP_SFT: u32 = 10;
/// DECLARE_EWMA(thermal, 4, 4) (core.h:6552) with include/linux/average.h: precision 4,
/// weight_rcp 4 (shift ilog2(4) = 2)
const EWMA_PREC: u32 = 4;
const EWMA_SHIFT: u32 = 2;
/// struct rtw89_cfo_tracking_info's per-MAC-ID arrays: CFO_TRACK_MAX_USER (Linux's format)
const CFO_USERS: usize = CFO_TRACK_MAX_USER as usize;

/// Linux: struct rtw89_phy_stat's thermal part
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stat {
    /// the ewma_thermal internals per path
    pub ewma_thermal: [u32; RF_PATHS],
    /// the last reading per path
    pub thermal: [u32; RF_PATHS],
    pub last_thermal_max: u32,
}

/// Linux: struct rtw89_cfo_tracking_info
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cfo {
    pub cfo_timer_ms: u32,
    pub cfo_trig_by_timer_en: bool,
    /// enum rtw89_phy_cfo_status
    pub phy_cfo_status: u32,
    pub cfo_ul_ofdma_acc_mode: u32,
    pub phy_cfo_trk_cnt: u32,
    pub is_adjust: bool,
    pub multi_cfo_mode: u32,
    pub apply_compensation: bool,
    pub crystal_cap: u32,
    pub crystal_cap_default: u32,
    pub def_x_cap: u32,
    pub x_cap_ofst: i32,
    pub x_cap_ub: u32,
    pub x_cap_lb: u32,
    /// what the XTAL SI read back last (sc_xo, sc_xi)
    pub sc_xo: u32,
    pub sc_xi: u32,
    pub divergence_lock_en: bool,
    pub lock_cnt: u32,
    pub residual_cfo_acc: i32,
    pub cfo_tail: Vec<i32>,
    pub cfo_cnt: Vec<u32>,
    pub cfo_avg: Vec<i32>,
    pub pre_cfo_avg: Vec<i32>,
    pub cfo_avg_pre: i32,
    pub dcfo_avg: i32,
    pub dcfo_avg_pre: i32,
    pub packet_count: u32,
    pub packet_count_pre: u32,
}

impl Default for Cfo {
    // zeroed with rtwdev, as Linux's
    fn default() -> Cfo {
        Cfo {
            cfo_timer_ms: 0,
            cfo_trig_by_timer_en: false,
            phy_cfo_status: 0,
            cfo_ul_ofdma_acc_mode: 0,
            phy_cfo_trk_cnt: 0,
            is_adjust: false,
            multi_cfo_mode: 0,
            apply_compensation: false,
            crystal_cap: 0,
            crystal_cap_default: 0,
            def_x_cap: 0,
            x_cap_ofst: 0,
            x_cap_ub: 0,
            x_cap_lb: 0,
            sc_xo: 0,
            sc_xi: 0,
            divergence_lock_en: false,
            lock_cnt: 0,
            residual_cfo_acc: 0,
            cfo_tail: vec![0; CFO_USERS],
            cfo_cnt: vec![0; CFO_USERS],
            cfo_avg: vec![0; CFO_USERS],
            pre_cfo_avg: vec![0; CFO_USERS],
            cfo_avg_pre: 0,
            dcfo_avg: 0,
            dcfo_avg_pre: 0,
            packet_count: 0,
            packet_count_pre: 0,
        }
    }
}

// include/linux/average.h ewma_add with precision 4, weight_rcp 4
fn ewma_add(internal: u32, val: u32) -> u32 {
    if internal != 0 {
        (internal
            .wrapping_shl(EWMA_SHIFT)
            .wrapping_sub(internal)
            .wrapping_add(val.wrapping_shl(EWMA_PREC)))
            >> EWMA_SHIFT
    } else {
        val.wrapping_shl(EWMA_PREC)
    }
}

// Linux: rtw89_phy_stat_thermal_update (phy.c:5812); also the thermal part of
// rtw89_phy_stat_track (phy.c:6250). A reading of 0 is not averaged. `avg_thermal[path]` is
// ewma_thermal_read of phystat.avg_thermal (the DPK tracking reads it).
pub fn stat_thermal_update<B: Bus + ?Sized>(bus: &mut B, phy: &mut Phy) -> Rtw89Result<()> {
    let mut max = 0;
    for path in 0..RF_PATHS {
        let th = get_thermal(bus, path as u32, phy.tssi_mode[path])?;
        let st = &mut phy.stat;
        if th != 0 {
            st.ewma_thermal[path] = ewma_add(st.ewma_thermal[path], th);
        }
        phy.avg_thermal[path] = st.ewma_thermal[path] >> EWMA_PREC;
        st.thermal[path] = th;
        max = max.max(th);
    }
    phy.stat.last_thermal_max = max;
    Ok(())
}

// Linux: rtw89_phy_cfo_set_crystal_cap (phy.c:4974); 8852B: XTAL SI, not the 8852A/8851B
// registers. The cap the chip read back becomes the current one.
pub fn set_crystal_cap<B: Bus + ?Sized>(
    bus: &mut B,
    cfo: &mut Cfo,
    cap: u32,
    force: bool,
) -> Rtw89Result<()> {
    if !force && cfo.crystal_cap == cap {
        return Ok(());
    }
    xtal_write(bus, XTAL_SI_XTAL_SC_XO, cap, XTAL_SC_XO_MASK)?;
    xtal_write(bus, XTAL_SI_XTAL_SC_XI, cap, XTAL_SC_XI_MASK)?;
    cfo.sc_xo = xtal_read(bus, XTAL_SI_XTAL_SC_XO)?;
    cfo.sc_xi = xtal_read(bus, XTAL_SI_XTAL_SC_XI)?;
    cfo.crystal_cap = cfo.sc_xi;
    cfo.x_cap_ofst = i32::from(cfo.crystal_cap.wrapping_sub(cfo.def_x_cap) as u8 as i8);
    Ok(())
}

// Linux: rtw89_phy_cfo_init (phy.c:5073) with rtw89_dcfo_comp_init (phy.c:5052),
// rtw89_cfo_regs_ax; 8852B: cfo_hw_comp true. `xtal_cap` is the efuse's.
pub fn cfo_init<B: Bus + ?Sized>(bus: &mut B, cfo: &mut Cfo, xtal_cap: u8) -> Rtw89Result<()> {
    *cfo = Cfo::default();
    cfo.crystal_cap_default = u32::from(xtal_cap) & B_AX_XTAL_SC_MASK;
    cfo.crystal_cap = cfo.crystal_cap_default;
    cfo.def_x_cap = cfo.crystal_cap;
    cfo.x_cap_ub = (cfo.def_x_cap + CFO_BOUND).min(0x7f);
    cfo.x_cap_lb = cfo.def_x_cap.saturating_sub(CFO_BOUND).max(0x1);
    cfo.multi_cfo_mode = RTW89_TP_BASED_AVG_MODE;
    set_crystal_cap(bus, cfo, cfo.crystal_cap_default, true)?;
    phy_mask32(bus, R_DCFO_OPT, B_DCFO_OPT_EN, 1)?;
    phy_mask32(bus, R_DCFO_WEIGHT, B_DCFO_WEIGHT_MSK, 8)?;
    bus.mask32(R_AX_PWR_UL_CTRL2, B_AX_PWR_UL_CFO_MASK, 0x6)?;
    cfo.cfo_timer_ms = 2000;
    cfo.cfo_trig_by_timer_en = false;
    cfo.phy_cfo_trk_cnt = 0;
    cfo.phy_cfo_status = RTW89_PHY_DCFO_STATE_NORMAL;
    cfo.cfo_ul_ofdma_acc_mode = RTW89_CFO_UL_OFDMA_ACC_ENABLE;
    Ok(())
}

// Linux: rtw89_phy_cfo_parse (phy.c:5429): one PPDU status' CFO (IE01, S(12,2)) of a MAC ID.
// A MAC ID past the arrays is Linux's "mac_id is out of range" warning.
pub fn cfo_parse(phy: &mut Phy, cfo_val: i32, mac_id: u32) {
    let cfo = &mut phy.cfo;
    let i = mac_id as usize;
    let (Some(tail), Some(cnt)) = (cfo.cfo_tail.get_mut(i), cfo.cfo_cnt.get_mut(i)) else {
        phy.push(PhyEvent::CfoMacIdRange { mac_id });
        return;
    };
    *tail = tail.wrapping_add(cfo_val);
    *cnt = cnt.wrapping_add(1) & 0xffff;
    cfo.packet_count = cfo.packet_count.wrapping_add(1);
}

// Linux: rtw89_phy_cfo_reset (phy.c:5006): one step back towards the default cap
fn cfo_reset<B: Bus + ?Sized>(bus: &mut B, cfo: &mut Cfo) -> Rtw89Result<()> {
    cfo.def_x_cap = cfo.crystal_cap_default & B_AX_XTAL_SC_MASK;
    cfo.is_adjust = false;
    if cfo.crystal_cap == cfo.def_x_cap {
        return Ok(());
    }
    let cap = if cfo.crystal_cap > cfo.def_x_cap {
        cfo.crystal_cap.wrapping_sub(1)
    } else {
        cfo.crystal_cap.wrapping_add(1)
    } & 0xff;
    set_crystal_cap(bus, cfo, cap, false)
}

// Linux: rtw89_phy_cfo_crystal_cap_adjust (phy.c:5101)
fn crystal_cap_adjust<B: Bus + ?Sized>(bus: &mut B, cfo: &mut Cfo, curr: i32) -> Rtw89Result<()> {
    if curr == 0 {
        return Ok(());
    }
    let abs = curr.unsigned_abs();
    if !cfo.is_adjust {
        if abs > CFO_TRK_ENABLE_TH {
            cfo.is_adjust = true;
        }
    } else if abs <= CFO_TRK_STOP_TH {
        cfo.is_adjust = false;
    }
    if !cfo.is_adjust {
        return Ok(());
    }
    let sign: i64 = if curr > 0 { 1 } else { -1 };
    let step = if abs > CFO_TRK_STOP_TH_4 || abs > CFO_TRK_STOP_TH_3 {
        3
    } else if abs > CFO_TRK_STOP_TH_2 || abs > CFO_TRK_STOP_TH_1 {
        1
    } else {
        return Ok(());
    };
    let cap = (i64::from(cfo.crystal_cap) + step * sign).clamp(0, 127);
    set_crystal_cap(bus, cfo, cap as u32, false)
}

// phy_div (phy.h:23): C division, 0 for a zero divisor
fn phy_div(a: i64, b: i64) -> i64 {
    if b != 0 { a / b } else { 0 }
}

// Linux: rtw89_phy_average_cfo_calc (phy.c:5143)
fn average_cfo_calc(cfo: &mut Cfo, total_sta_assoc: u32) -> i32 {
    if total_sta_assoc != 1 {
        return 0;
    }
    let (mut khz_all, mut cnt_all, mut avg) = (0i32, 0i64, 0i64);
    for i in 0..cfo.cfo_cnt.len().min(cfo.cfo_tail.len()) {
        if cfo.cfo_cnt[i] == 0 {
            continue;
        }
        khz_all = khz_all.wrapping_add(cfo.cfo_tail[i]);
        cnt_all += i64::from(cfo.cfo_cnt[i]);
        avg = phy_div(i64::from(khz_all), cnt_all);
        if let (Some(p), Some(a)) = (cfo.pre_cfo_avg.get_mut(i), cfo.cfo_avg.get(i)) {
            *p = *a;
        }
        cfo.dcfo_avg = phy_div(i64::from(khz_all.wrapping_shl(DCFO_COMP_SFT)), cnt_all) as i32;
    }
    avg as i32
}

// Linux: rtw89_dcfo_comp (phy.c:5023); 8852B dcfo_comp = { R_DCFO_COMP_S0, B_DCFO_COMP_S0_MSK },
// no 8852A CBV sign flip
fn dcfo_comp<B: Bus + ?Sized>(bus: &mut B, curr: i32, total_sta_assoc: u32) -> Rtw89Result<()> {
    if total_sta_assoc == 0 || curr == 0 {
        return Ok(());
    }
    let comp_val = phy_r32_mask(bus, R_DCFO, B_DCFO)? as i32;
    let sign = if curr > 0 { 1 } else { -1 };
    let avg312 = (curr / 625).wrapping_add(sign * comp_val);
    phy_mask32(bus, R_DCFO_COMP_S0, B_DCFO_COMP_S0_MSK, avg312 as u32)
}

// Linux: rtw89_phy_cfo_dm (phy.c:5284); no MLO (rtw89_is_mlo_1_1). With more than one
// associated station rtw89_phy_multi_sta_cfo_calc would run: not ported, 0 (managed mode
// has one).
fn cfo_dm<B: Bus + ?Sized>(bus: &mut B, cfo: &mut Cfo, total_sta_assoc: u32) -> Rtw89Result<()> {
    let pre_x_cap = cfo.crystal_cap;
    cfo.dcfo_avg = 0;
    if total_sta_assoc == 0 {
        return cfo_reset(bus, cfo);
    }
    if cfo.packet_count == 0 || cfo.packet_count == cfo.packet_count_pre {
        return Ok(());
    }
    let new_cfo = if total_sta_assoc == 1 {
        average_cfo_calc(cfo, total_sta_assoc)
    } else {
        0
    };
    if cfo.divergence_lock_en {
        cfo.lock_cnt = cfo.lock_cnt.wrapping_add(1) & 0xff;
        if cfo.lock_cnt > CFO_PERIOD_CNT {
            cfo.divergence_lock_en = false;
            cfo.lock_cnt = 0;
        } else {
            cfo_reset(bus, cfo)?;
        }
        return Ok(());
    }
    if cfo.crystal_cap >= cfo.x_cap_ub || cfo.crystal_cap <= cfo.x_cap_lb {
        cfo.divergence_lock_en = true;
        return cfo_reset(bus, cfo);
    }
    crystal_cap_adjust(bus, cfo, new_cfo)?;
    cfo.cfo_avg_pre = new_cfo;
    cfo.dcfo_avg_pre = cfo.dcfo_avg;
    if cfo.crystal_cap != pre_x_cap {
        let fine = (CFO_SW_COMP_FINE_TUNE << DCFO_COMP_SFT) as i32;
        cfo.dcfo_avg = if cfo.dcfo_avg > 0 {
            cfo.dcfo_avg.wrapping_sub(fine)
        } else {
            cfo.dcfo_avg.wrapping_add(fine)
        };
    }
    dcfo_comp(bus, cfo.dcfo_avg, total_sta_assoc)?;
    // rtw89_phy_cfo_statistics_reset (phy.c:5273)
    cfo.cfo_tail.iter_mut().for_each(|v| *v = 0);
    cfo.cfo_cnt.iter_mut().for_each(|v| *v = 0);
    cfo.packet_count = 0;
    cfo.packet_count_pre = 0;
    cfo.cfo_avg_pre = 0;
    Ok(())
}

// Linux: rtw89_phy_cfo_track (phy.c:5370), a part of rtw89_track_work (every 2 s);
// `tx_throughput` (Mbps) and `rx_tf_periodic` are rtwdev->stats as the caller counts them.
// In the ENHANCE state the timer work (cfo_track_work) does the tracking instead.
pub fn cfo_track<B: Bus + ?Sized>(
    bus: &mut B,
    cfo: &mut Cfo,
    total_sta_assoc: u32,
    tx_throughput: u32,
    rx_tf_periodic: u32,
) -> Rtw89Result<()> {
    let is_ul_ofdma = rx_tf_periodic > CFO_TF_CNT_TH;
    let ofdma_acc_en = cfo.cfo_ul_ofdma_acc_mode == RTW89_CFO_UL_OFDMA_ACC_ENABLE && is_ul_ofdma;
    match cfo.phy_cfo_status {
        RTW89_PHY_DCFO_STATE_NORMAL => {
            if tx_throughput >= CFO_TP_UPPER {
                cfo.phy_cfo_status = RTW89_PHY_DCFO_STATE_ENHANCE;
                cfo.cfo_trig_by_timer_en = true;
                cfo.cfo_timer_ms = CFO_COMP_PERIOD;
                // rtw89_phy_cfo_start_work: the caller runs cfo_track_work every cfo_timer_ms
            }
        }
        RTW89_PHY_DCFO_STATE_ENHANCE => {
            if tx_throughput <= CFO_TP_LOWER {
                cfo.phy_cfo_status = RTW89_PHY_DCFO_STATE_NORMAL;
            } else if ofdma_acc_en && cfo.phy_cfo_trk_cnt >= CFO_PERIOD_CNT {
                cfo.phy_cfo_status = RTW89_PHY_DCFO_STATE_HOLD;
            } else {
                cfo.phy_cfo_trk_cnt = cfo.phy_cfo_trk_cnt.wrapping_add(1) & 0xff;
            }
            if cfo.phy_cfo_status == RTW89_PHY_DCFO_STATE_NORMAL {
                cfo.phy_cfo_trk_cnt = 0;
                cfo.cfo_trig_by_timer_en = false;
            }
        }
        RTW89_PHY_DCFO_STATE_HOLD => {
            if tx_throughput <= CFO_TP_LOWER {
                cfo.phy_cfo_status = RTW89_PHY_DCFO_STATE_NORMAL;
                cfo.phy_cfo_trk_cnt = 0;
                cfo.cfo_trig_by_timer_en = false;
            } else {
                cfo.phy_cfo_trk_cnt = cfo.phy_cfo_trk_cnt.wrapping_add(1) & 0xff;
            }
        }
        _ => {
            cfo.phy_cfo_status = RTW89_PHY_DCFO_STATE_NORMAL;
            cfo.phy_cfo_trk_cnt = 0;
        }
    }
    if cfo.cfo_trig_by_timer_en {
        return Ok(());
    }
    cfo_dm(bus, cfo, total_sta_assoc)?;
    bus.flush()?;
    Ok(())
}

// Linux: rtw89_phy_cfo_track_work (phy.c:5346) without the re-queue: the caller runs it every
// `cfo_timer_ms` while `cfo_trig_by_timer_en`. Returns whether it ran.
pub fn cfo_track_work<B: Bus + ?Sized>(
    bus: &mut B,
    cfo: &mut Cfo,
    total_sta_assoc: u32,
) -> Rtw89Result<bool> {
    if !cfo.cfo_trig_by_timer_en {
        return Ok(false);
    }
    cfo_dm(bus, cfo, total_sta_assoc)?;
    bus.flush()?;
    Ok(true)
}
