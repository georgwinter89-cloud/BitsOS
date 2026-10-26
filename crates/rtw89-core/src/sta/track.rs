//! rtw89_track_work (core.c:5503), the driver's 2-second work, and the traffic statistics it
//! starts from (JS js/sta-track.mjs). rtw89_core_start queues it (`track_start`), rtw89_core_stop
//! cancels it (`track_stop`). BitsOS has no work queue: the station's next RX poll after its time
//! runs it (`track_due`), as the coex act1 work. The 2-second rhythm is Linux's.

use crate::btc::WlStaLink;
use crate::btc::state::Traffic as BtcTraffic;
use crate::bus::{Bus, field};
use crate::error::Rtw89Result;
use crate::phy::dig::{ApRssi, UlTbLink};
use crate::phy::io::phy_mask32;
use crate::phy::{dig, env, trk};
use crate::tables::regs::*;

use super::h2c;
use super::io::Chip;
use super::ra::{self, hci_recalc_int_mit};
use super::{Sta, StaEvent};

/// RTW89_TRACK_WORK_PERIOD, HZ * 2
pub const TRACK_MS: u64 = 2000;

/// Linux: struct rtw89_traffic_stats (core.h:1616), zeroed: the fields rtw89 reads back
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Traffic {
    pub tx_unicast: u64,
    pub rx_unicast: u64,
    pub tx_cnt: u32,
    pub rx_cnt: u32,
    pub tx_throughput: u32,
    pub rx_throughput: u32,
    pub tx_throughput_raw: u32,
    pub rx_throughput_raw: u32,
    pub rx_tf_acc: u32,
    pub rx_tf_periodic: u32,
    pub tx_tfc_lv: u32,
    pub rx_tfc_lv: u32,
    pub tx_ewma_tp: u64,
    pub rx_ewma_tp: u64,
    pub tx_avg_len: u32,
    pub rx_avg_len: u32,
}

/// The track work's time and rtwdev->stats
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Track {
    /// the next run (`None`: not queued)
    pub due: Option<u64>,
    /// the next rtw89_phy_cfo_track_work (`None`: not queued)
    pub cfo_due: Option<u64>,
    pub stats: Traffic,
    /// rtw89_core_rfkill_poll's last state
    pub rfkill: bool,
}

// Linux: wiphy_delayed_work_queue(track_work, RTW89_TRACK_WORK_PERIOD) of rtw89_core_start
// (core.c:6658)
pub fn track_start(sta: &mut Sta, now: u64) {
    sta.track = Track {
        due: Some(now + TRACK_MS),
        cfo_due: None,
        stats: Traffic::default(),
        rfkill: sta.track.rfkill,
    };
}

// Linux: wiphy_delayed_work_cancel(track_work / cfo_track_work) of rtw89_core_stop (core.c:6727)
pub fn track_stop(sta: &mut Sta) {
    sta.track.due = None;
    sta.track.cfo_due = None;
}

/// Which statistics a frame counts in (rtwdev's, the vif's)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Accu {
    pub tx: bool,
    pub dev: bool,
    pub vif: bool,
}

// Linux: rtw89_traffic_stats_accu (core.c:373) with __rtw89_traffic_stats_accu (core.c:360):
// unicast data frames only; `len`: skb->len (TX: the 802.11 frame; RX: with the FCS)
pub fn traffic_accu(sta: &mut Sta, frame: &[u8], len: usize, a: Accu) {
    if sta.track.due.is_none() || frame.len() < 10 {
        return;
    }
    let fc = u16::from_le_bytes([frame[0], frame[1]]);
    if (fc >> 2) & 3 != 2 || frame[4] & 1 != 0 {
        return; // ieee80211_is_data, addr1 a group address
    }
    let add = |st: &mut Traffic| {
        if a.tx {
            st.tx_cnt = st.tx_cnt.wrapping_add(1);
            st.tx_unicast = st.tx_unicast.wrapping_add(len as u64);
        } else {
            st.rx_cnt = st.rx_cnt.wrapping_add(1);
            st.rx_unicast = st.rx_unicast.wrapping_add(len as u64);
        }
    };
    if a.dev {
        add(&mut sta.track.stats);
    }
    if a.vif {
        add(&mut sta.vif.stats);
    }
}

// DECLARE_EWMA(tp, 10, 2) (core.h:1610): precision 10, weight 1/2
fn ewma_tp_add(e: u64, v: u64) -> u64 {
    if e != 0 {
        (e * 2 - e + (v << 10)) / 2
    } else {
        v << 10
    }
}

// rtw89_get_traffic_level (core.c:5152), RTW89_TFC_INTERVAL_2SEC (100 packets)
fn traffic_level(tp: u32, cnt: u32) -> u32 {
    if cnt < 100 {
        RTW89_TFC_IDLE
    } else if tp > 50 {
        RTW89_TFC_HIGH
    } else if tp > 10 {
        RTW89_TFC_MID
    } else if tp > 2 {
        RTW89_TFC_LOW
    } else {
        RTW89_TFC_ULTRA_LOW
    }
}

// rtw89_traffic_stats_calc (core.c:5212), 2 s interval (rtw89_bytes_to_mbps: bytes >> 18);
// rtw89_calc_vif_active_histogram only feeds statistics. Returns whether a level changed.
fn stats_calc(st: &mut Traffic) -> bool {
    let (tx_lv, rx_lv) = (st.tx_tfc_lv, st.rx_tfc_lv);
    st.tx_throughput_raw = (st.tx_unicast >> 18) as u32;
    st.rx_throughput_raw = (st.rx_unicast >> 18) as u32;
    st.tx_ewma_tp = ewma_tp_add(st.tx_ewma_tp, st.tx_throughput_raw.into());
    st.rx_ewma_tp = ewma_tp_add(st.rx_ewma_tp, st.rx_throughput_raw.into());
    st.tx_throughput = (st.tx_ewma_tp >> 10) as u32;
    st.rx_throughput = (st.rx_ewma_tp >> 10) as u32;
    st.tx_tfc_lv = traffic_level(st.tx_throughput, st.tx_cnt);
    st.rx_tfc_lv = traffic_level(st.rx_throughput, st.rx_cnt);
    st.tx_avg_len = if st.tx_cnt != 0 {
        (st.tx_unicast / u64::from(st.tx_cnt)) as u32
    } else {
        0
    };
    st.rx_avg_len = if st.rx_cnt != 0 {
        (st.rx_unicast / u64::from(st.rx_cnt)) as u32
    } else {
        0
    };
    st.tx_unicast = 0;
    st.rx_unicast = 0;
    st.tx_cnt = 0;
    st.rx_cnt = 0;
    st.rx_tf_periodic = st.rx_tf_acc;
    st.rx_tf_acc = 0;
    tx_lv != st.tx_tfc_lv || rx_lv != st.rx_tfc_lv
}

// rtw89_traffic_stats_track (core.c:5252): rtwdev's, then the vif's with rtw89_fw_h2c_tp_offload
// (fw.c:5518: only an associated station, -EINVAL otherwise)
fn traffic_stats_track<B: Bus + ?Sized>(
    bus: &mut B,
    chip: &mut Chip,
    sta: &mut Sta,
) -> Rtw89Result<bool> {
    let changed = stats_calc(&mut sta.track.stats);
    if sta.state != super::LinkState::Down {
        stats_calc(&mut sta.vif.stats);
        if sta.vif.net_type == RTW89_NET_TYPE_INFRA {
            let st = sta.vif.stats;
            chip.h2c(
                bus,
                &h2c::h2c_tp_offload(sta.vif.mac_id, st.tx_throughput, st.rx_throughput),
            )?;
        }
    }
    Ok(changed)
}

// the AP's link as the coexistence's station iteration sees it (rtw89_btc_ntfy_wl_sta)
fn btc_link(sta: &Sta) -> Option<WlStaLink> {
    let ap = sta.ap.as_ref()?;
    let s = sta.vif.stats;
    Some(WlStaLink {
        port: sta.vif.port,
        avg_rssi: ap.avg_rssi.read(),
        stats: BtcTraffic {
            tx_tfc_lv: s.tx_tfc_lv,
            rx_tfc_lv: s.rx_tfc_lv,
            tx_throughput: s.tx_throughput,
            rx_throughput: s.rx_throughput,
            tx_rate: 0,
            rx_rate: 0,
        },
        tx_hw_rate: 0, // ra_report: the RA report C2H is not ported
        rx_hw_rate: ap.rx_hw_rate,
    })
}

// rtw89_phy_ra_update (phy.c:724) -> rtw89_phy_ra_update_sta_link (phy.c:505) for the AP's link,
// IEEE80211_RC_SUPP_RATES_CHANGED: the rate mask again (the RSSI moves it), upd_mask set
fn ra_update<B: Bus + ?Sized>(bus: &mut B, chip: &mut Chip, sta: &mut Sta) -> Rtw89Result<()> {
    let band_2g = sta.vif.band == crate::phy::Band::B2g;
    let tx_nss = match chip.run.phy.hal.tx_nss {
        0 => 2,
        n => n,
    };
    let Some(ap) = sta.ap.as_mut() else {
        return Ok(());
    };
    let mut info = ra::ra_sta_update(ap, band_2g, tx_nss);
    info.upd_mask = 1;
    ap.ra = Some(info);
    chip.h2c(bus, &h2c::h2c_ra(&info))
}

// rtw89_phy_tx_path_div_track (phy.c:8026) with __rtw89_phy_tx_path_div_sta_iter (phy.c:7963):
// only when the firmware reported one TX stream for two antennas (hal->tx_path_diversity)
fn tx_path_div_track<B: Bus + ?Sized>(
    bus: &mut B,
    chip: &mut Chip,
    sta: &mut Sta,
) -> Rtw89Result<()> {
    let hal = chip.run.phy.hal.clone();
    let Some(ap) = sta.ap.as_ref() else {
        return Ok(());
    };
    if !hal.tx_path_diversity || sta.vif.wifi_role != RTW89_WIFI_ROLE_STATION {
        return Ok(());
    }
    let (a, b, th) = (
        ap.rssi[0].read(),
        ap.rssi[1].read(),
        RTW89_TX_DIV_RSSI_RAW_TH,
    );
    let cand = if a > b + th {
        RF_A
    } else if b > a + th {
        RF_B
    } else {
        return Ok(());
    };
    if hal.antenna_tx == cand {
        return Ok(());
    }
    chip.run.phy.hal.antenna_tx = cand;
    let m = h2c::h2c_txpath_cmac_tbl(ap, cand);
    chip.h2c(bus, &m)?;
    let (p0, p1) = if cand == RF_A {
        (0x12, 0x11)
    } else {
        (0x11, 0x12)
    };
    phy_mask32(bus, R_P0_RFMODE, B_P0_RFMODE_MUX, p0)?;
    phy_mask32(bus, R_P1_RFMODE, B_P1_RFMODE_MUX, p1)
}

// rtw89_core_rfkill_poll (core.c:7359) with force false. mac80211 would take the radio down
// when blocked (wiphy_rfkill_set_hw_state); BitsOS logs it and keeps the state.
fn rfkill_poll<B: Bus + ?Sized>(bus: &mut B, sta: &mut Sta) -> Rtw89Result<()> {
    let blocked = field(bus.r8(R_AX_GPIO_EXT_CTRL)?.into(), B_AX_GPIO_IN_9) == 0;
    if blocked != sta.track.rfkill {
        sta.track.rfkill = blocked;
        sta.push(StaEvent::Rfkill { blocked });
    }
    Ok(())
}

// Linux: rtw89_track_work (core.c:5503), one run; the next one is queued first
pub fn track_work<B: Bus + ?Sized>(
    bus: &mut B,
    chip: &mut Chip,
    sta: &mut Sta,
    now: u64,
) -> Rtw89Result<()> {
    sta.track.due = Some(now + TRACK_MS);
    let tfc_changed = traffic_stats_track(bus, chip, sta)?;
    if sta.scanning {
        return Ok(());
    }
    // rtw89_leave_lps: BitsOS never enters LPS
    if tfc_changed {
        let st = sta.track.stats;
        hci_recalc_int_mit(bus, sta.scanning, (st.tx_tfc_lv, st.rx_tfc_lv))?;
        let l = btc_link(sta);
        chip.btc_wl_sta(bus, l.as_ref())?;
    }
    // rtw89_mac_bf_monitor_track: RTW89_FLAG_BFEE_MON is never set (no beamforming)
    // rtw89_core_bcn_track: needs LPS (lps_enabled false)
    // rtw89_phy_stat_track (phy.c:6250): thermal; thermal_prot_vmax/_th are 0; bb_stat_cfg
    // (BE only); path_diff_update is NULL for the 8852B
    trk::stat_thermal_update(bus, &mut chip.run.phy)?;
    let mut ap_rssi = sta.ap.as_ref().map(|a| ApRssi {
        avg_rssi_internal: a.avg_rssi.internal,
        mac_id: a.mac_id,
        prev_rssi: a.prev_rssi,
    });
    let rssi_changed = dig::stat_rssi_update(&mut chip.run.phy, ap_rssi.as_mut());
    if let (Some(a), Some(r)) = (sta.ap.as_mut(), ap_rssi) {
        a.prev_rssi = r.prev_rssi;
    }
    if rssi_changed {
        let l = btc_link(sta);
        chip.btc_wl_sta(bus, l.as_ref())?;
    }
    env::env_monitor_track(bus, &mut chip.run.phy.env)?;
    let total = sta.dev.total_sta_assoc;
    let chan = sta.cur_chan();
    dig::phy_dig(bus, &mut chip.run.phy, total, &chan)?;
    chip.rfk_track(bus, Some(chan))?; // rtw89_core_rfk_track: no MCC; AX: no TSSI record
    ra_update(bus, chip, sta)?;
    let st = sta.track.stats;
    trk::cfo_track(
        bus,
        &mut chip.run.phy.cfo,
        total,
        st.tx_throughput,
        st.rx_tf_periodic,
    )?;
    let cfo = &chip.run.phy.cfo;
    if cfo.cfo_trig_by_timer_en && sta.track.cfo_due.is_none() {
        sta.track.cfo_due = Some(now + u64::from(cfo.cfo_timer_ms)); // cfo_start_work
    }
    tx_path_div_track(bus, chip, sta)?;
    // rtw89_phy_antdiv_track: no antenna diversity
    let link = UlTbLink {
        total_sta_assoc: total,
        station_assoc: sta.vif.wifi_role == RTW89_WIFI_ROLE_STATION && sta.vif.assoc,
        dyn_tb_bedge_en: sta.vif.dyn_tb_bedge_en,
        def_tri_idx: sta.vif.def_tri_idx,
    };
    dig::ul_tb_ctrl_track(bus, &mut chip.run.phy, &link, st.rx_tf_periodic)?;
    dig::edcca_track(bus, &mut chip.run.phy, total, st.tx_throughput)?;
    // rtw89_sar_track: no ACPI SAR, no TAS; rtw89_chanctx_track: no MCC
    rfkill_poll(bus, sta)?;
    // rtw89_core_mlo_track: no MLD; rtw89_enter_lps_track: no LPS
    bus.flush()?;
    Ok(())
}

// Linux: none (BitsOS has no work queue): runs rtw89_track_work and rtw89_phy_cfo_track_work
// (phy.c:5346, re-queued every cfo_timer_ms while cfo_trig_by_timer_en) once they are due
pub fn track_due<B: Bus + ?Sized>(
    bus: &mut B,
    chip: &mut Chip,
    sta: &mut Sta,
    now: u64,
) -> Rtw89Result<()> {
    if let Some(t) = sta.track.cfo_due
        && now >= t
    {
        let total = sta.dev.total_sta_assoc;
        let ran = trk::cfo_track_work(bus, &mut chip.run.phy.cfo, total)?;
        sta.track.cfo_due = ran.then(|| now + u64::from(chip.run.phy.cfo.cfo_timer_ms));
    }
    if let Some(t) = sta.track.due
        && now >= t
    {
        track_work(bus, chip, sta, now)?;
    }
    Ok(())
}
