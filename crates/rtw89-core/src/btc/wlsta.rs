//! The coexistence notification of the 2-second track work, rtw89_btc_ntfy_wl_sta (coex.c:9655),
//! for the one station link (JS js/btc-wlsta.mjs): its RSSI level against the 8852B thresholds,
//! its traffic level and direction, the rates; the WLBUSY scoreboard bit; a coex run when the
//! station changed.

use crate::bus::Bus;
use crate::error::Rtw89Result;
use crate::tables::btc::{WL_RSSI_THRES, mlme, rsn, wscb};
use crate::tables::regs::*;

use super::h2c::h2c_cx_role;
use super::run::{run_coex, write_scbd};
use super::state::Traffic;
use super::{BtcHooks, BtcState};

/// rtw8852b_chip_info.rssi_tol (rtw8852b.c:1040)
const RSSI_TOL: u32 = 2;
/// enum btc_rssi_st (coex.h:102)
const ST_LOW: u32 = 0;
const ST_HIGH: u32 = 1;
const ST_STAY_LOW: u32 = 2;
const ST_STAY_HIGH: u32 = 3;
/// BTC_NHM_CHK_INTVL
const NHM_CHK_INTVL: u32 = 20;

/// The AP's link as the coexistence's station iteration sees it
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WlStaLink {
    pub port: u32,
    /// ewma_rssi_read of the link's average RSSI (RSSI_FACTOR fixed point)
    pub avg_rssi: u32,
    /// the vif's traffic statistics (levels and throughput; the rates are not read)
    pub stats: Traffic,
    /// ra_report.hw_rate and the last RX rate
    pub tx_hw_rate: u32,
    pub rx_hw_rate: u32,
}

#[derive(Default)]
struct IterData {
    rssi_map_all: u32,
    busy_all: u32,
    dir_all: u32,
    is_sta_change: bool,
    is_traffic_change: bool,
}

fn is_low(s: u32) -> bool {
    s == ST_LOW || s == ST_STAY_LOW // BTC_RSSI_LOW
}
fn is_change(s: u32) -> bool {
    s == ST_LOW || s == ST_HIGH // BTC_RSSI_CHANGE
}

// _update_rssi_state (coex.c:7068)
fn update_rssi_state(pre: u32, rssi: u32, thresh: u32) -> u32 {
    if pre == ST_LOW || pre == ST_STAY_LOW {
        if rssi >= thresh.saturating_add(RSSI_TOL) {
            ST_HIGH
        } else {
            ST_STAY_LOW
        }
    } else if rssi < thresh {
        ST_LOW
    } else {
        ST_STAY_HIGH
    }
}

// __rtw89_btc_ntfy_wl_sta_iter (coex.c:9514) for the AP's link (port, PHY 0). The link's traffic
// statistics are a copy of the vif's (link_info->stat.traffic = *stats), so the "last" rates read
// back as the vif's tx_rate/rx_rate, which rtw89 never writes (0), as in Linux.
fn sta_iter(st: &mut BtcState, link: &WlStaLink, data: &mut IterData) {
    let dedicated = st.ant_type == BTC_ANT_DEDICATED;
    let Some(Some(info)) = st.wl.rlink_info.get_mut(link.port as usize) else {
        return;
    };
    let stats = link.stats;
    info.traffic = Some(Traffic {
        tx_rate: 0,
        rx_rate: 0,
        ..stats
    });
    info.rssi = None;
    if info.connected == mlme::NO_LINK {
        info.rx_rate_drop_cnt = 0;
        return;
    }
    let rssi = (link.avg_rssi >> RSSI_FACTOR) & 0xff;
    info.rssi = Some(rssi);
    let rs = info.rssi_state.get_or_insert([0; 4]);
    let (mut rssi_map, mut sta_change, mut traffic_change) = (0u32, false, false);
    for (i, (s, &th)) in rs.iter_mut().zip(WL_RSSI_THRES.iter()).enumerate() {
        *s = update_rssi_state(*s, rssi, th.into());
        if is_low(*s) {
            rssi_map |= 1 << i;
        }
        if dedicated && is_change(*s) {
            sta_change = true;
        }
    }
    data.rssi_map_all |= rssi_map;
    let t = info.traffic.get_or_insert(stats);
    let (last_tx_rate, last_rx_rate) = (t.tx_rate, t.rx_rate);
    let (last_tx_lvl, last_rx_lvl) = (t.tx_tfc_lv, t.rx_tfc_lv);
    let busy = u32::from(stats.tx_tfc_lv != RTW89_TFC_IDLE || stats.rx_tfc_lv != RTW89_TFC_IDLE);
    let dir = if stats.tx_tfc_lv > stats.rx_tfc_lv {
        RTW89_TFC_UL
    } else {
        RTW89_TFC_DL
    };
    if info.busy != busy || info.dir != dir {
        sta_change = true;
        info.busy = busy;
        info.dir = dir;
    }
    data.busy_all |= busy;
    data.dir_all |= 1 << dir;
    if link.rx_hw_rate <= RTW89_HW_RATE_CCK2
        && last_rx_rate > RTW89_HW_RATE_CCK2
        && t.rx_tfc_lv > RTW89_TFC_IDLE
    {
        info.rx_rate_drop_cnt = info.rx_rate_drop_cnt.saturating_add(1);
    }
    if last_tx_rate != link.tx_hw_rate
        || last_rx_rate != link.rx_hw_rate
        || last_tx_lvl != t.tx_tfc_lv
        || last_rx_lvl != t.rx_tfc_lv
    {
        traffic_change = true;
    }
    t.tx_rate = link.tx_hw_rate;
    t.rx_rate = link.rx_hw_rate;
    let ti = &mut st.dm.trx_info;
    ti.tx_lvl = stats.tx_tfc_lv;
    ti.rx_lvl = stats.rx_tfc_lv;
    ti.tx_rate = link.tx_hw_rate;
    ti.rx_rate = link.rx_hw_rate;
    ti.tx_tp = stats.tx_throughput;
    ti.rx_tp = stats.rx_throughput;
    // the readbacks differ from what coex set up: run coex again
    let dm = &st.dm;
    if (dm.wl_btg_rx_rb != dm.wl_btg_rx && dm.wl_btg_rx_rb != BTC_BTGCTRL_BB_GNT_NOTFOUND)
        || (dm.wl_pre_agc_rb != dm.wl_pre_agc && dm.wl_pre_agc_rb != BTC_PREAGC_NOTFOUND)
    {
        data.is_sta_change = true;
    }
    if sta_change {
        data.is_sta_change = true;
    }
    if traffic_change {
        data.is_traffic_change = true;
    }
}

// Linux: rtw89_btc_ntfy_wl_sta (coex.c:9655). `link`: the AP's link as the stations iteration
// sees it, or None without a station.
pub fn wl_sta<B: Bus + ?Sized>(
    bus: &mut B,
    st: &mut BtcState,
    hooks: &mut dyn BtcHooks<B>,
    link: Option<&WlStaLink>,
) -> Rtw89Result<()> {
    let mut data = IterData::default();
    if let Some(l) = link {
        sta_iter(st, l, &mut data);
    }
    st.wl.rssi_level = 0;
    st.dm.cnt_notify_wl_sta = st.dm.cnt_notify_wl_sta.wrapping_add(1);
    for i in (1..=WL_RSSI_THRES.len() as u32).rev() {
        if data.rssi_map_all & (1 << (i - 1)) != 0 {
            st.wl.rssi_level = i;
            break;
        }
    }
    st.dm.trx_info.wl_rssi = st.wl.rssi_level;
    write_scbd(bus, st, wscb::WLBUSY, st.wl.status.busy != 0)?;
    if data.is_traffic_change {
        h2c_cx_role(bus, st, hooks)?; // _fw_set_drv_info(CXDRVINFO_ROLE)
    }
    let dm = &mut st.dm;
    if data.is_sta_change {
        st.wl.status.busy = data.busy_all;
        st.wl.status.traffic_dir = data.dir_all;
        return run_coex(bus, st, hooks, rsn::NTFY_WL_STA);
    } else if dm.cnt_notify_wl_sta >= dm.cnt_dm_wl_sta_last.saturating_add(NHM_CHK_INTVL)
        || dm.cnt_notify_wl_sta < dm.cnt_dm_wl_sta_last
    {
        dm.cnt_dm_wl_sta_last = dm.cnt_notify_wl_sta;
    }
    Ok(())
}
