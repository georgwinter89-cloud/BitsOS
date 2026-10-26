//! The role, RF calibration and packet notifications of the coexistence (JS js/btc.mjs, second
//! half): rtw89_btc_ntfy_role_info for the one station, rtw89_btc_ntfy_wl_rfk and its neighbours,
//! rtw89_btc_ntfy_specific_packet and rtw89_coex_act1_work.

use crate::bus::{Bus, field};
use crate::error::Rtw89Result;
use crate::phy::Band;
use crate::tables::btc::{mlme, rsn, wlink, wscb};
use crate::tables::regs::*;

use super::h2c::h2c_cx_role;
use super::run::{run_coex, write_scbd};
use super::state::{LinkInfo, MAX_ROLE};
use super::{BtcEvent, BtcHooks, BtcState, band_idx, not_ported};

/// The role states btc.mjs's `btcRole` takes (enum btc_role_state)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoleState {
    /// BTC_ROLE_START: add_interface
    Start,
    /// BTC_ROLE_STOP: remove_interface
    Stop,
    /// BTC_ROLE_MSTS_STA_CONN_START: rtw89_core_sta_link_add
    ConnStart,
    /// BTC_ROLE_MSTS_STA_CONN_END: rtw89_core_sta_link_assoc
    ConnEnd,
    /// BTC_ROLE_MSTS_STA_DIS_CONN
    Disconnect,
}

/// What a role notification says about the station: its port and MAC id, the current channel
/// (band, channel, enum rtw89_bandwidth) and its wireless mode bits (0: legacy only)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoleReq {
    pub port: u32,
    pub mac_id: u32,
    pub mode: u32,
    pub band: Band,
    pub ch: u32,
    pub bw: u32,
}

impl Default for RoleReq {
    /// port 0, MAC id 0, legacy, the default chandef (2.4 GHz channel 1, 20 MHz)
    fn default() -> RoleReq {
        RoleReq {
            port: 0,
            mac_id: 0,
            mode: 0,
            band: Band::B2g,
            ch: 1,
            bw: RTW89_CHANNEL_WIDTH_20,
        }
    }
}

/// The packet kinds of rtw89_btc_ntfy_specific_packet (enum btc_pkt_type)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Packet {
    Dhcp,
    Eapol,
    /// rtw89_ops_set_key: the 4-way handshake is done
    EapolEnd,
    /// counted only
    Arp,
    /// counted only
    Icmp,
}

// Linux: _update_wl_non_mlo_info (coex.c:7403) and _modify_role_link_mode (coex.c:7524) for HW
// band 0; only station roles, so no GO/AP or 2-role types (_get_role_link_mode)
fn update_link_mode(st: &mut BtcState) -> Rtw89Result<()> {
    let wl = &mut st.wl;
    let (mut cnt, mut b2g, mut b5g, mut first_role) = (0u32, false, false, 0u32);
    wl.rf_band_map = [0; 2];
    for rl in &wl.role.rlink {
        if !rl.active || rl.connected == 0 {
            continue;
        }
        if cnt == 0 {
            first_role = rl.role;
        }
        cnt += 1;
        if rl.rf_band != RTW89_BAND_2G {
            b5g = true;
        } else {
            b2g = true;
        }
        wl.rf_band_map[0] |= 1u32.wrapping_shl(rl.rf_band);
    }
    let r = &mut wl.role;
    r.pta_req_band = RTW89_PHY_0;
    r.dbcc_en = 0;
    let mode = if cnt == 0 {
        wlink::NOLINK
    } else if b2g && b5g {
        wlink::DB_MCC
    } else if cnt >= 2 {
        return not_ported("two WLAN roles", cnt);
    } else if first_role == RTW89_WIFI_ROLE_STATION {
        wlink::STA
    } else {
        return not_ported("WLAN role", first_role);
    };
    r.link_mode = mode;
    r.link_mode_hb1 = mode;
    r.mrole_type = 0; // BTC_WLMROLE_NONE: no SCC/MCC
    Ok(())
}

// Linux: _update_wl_info (coex.c:7587) for rlink_id 0
fn update_wl_info(st: &mut BtcState, info: &LinkInfo) -> Rtw89Result<()> {
    let Some(rl) = st.wl.role.rlink.get_mut(info.pid as usize) else {
        return Ok(()); // RTW89_BE_BTC_WL_MAX_ROLE_NUMBER
    };
    rl.role = info.role;
    rl.active = info.active;
    rl.pid = info.pid;
    rl.mac_id = info.mac_id;
    rl.phy = info.phy;
    rl.rf_band = info.band;
    rl.ch = info.ch;
    rl.bw = info.bw;
    rl.noa = 0;
    rl.noa_dur = 0;
    rl.client_cnt = 0;
    rl.mode = info.mode;
    match info.connected {
        mlme::NO_LINK => rl.connected = 0,
        mlme::LINKED => {
            rl.connected = 1;
            st.dm.leak_ap = 0;
        }
        _ => return Ok(()),
    }
    let (mut role_map, mut hi_ch, mut bg) = (0u32, 0, 0);
    for x in &st.wl.role.rlink {
        if !x.active || x.connected == 0 {
            continue;
        }
        role_map |= 1u32.wrapping_shl(x.role);
        if x.rf_band == RTW89_BAND_5G && x.ch >= 100 {
            hi_ch = 1;
        }
        if x.mode & ((1 << 0) | (1 << 2)) != 0 {
            bg = 1; // BIT(BTC_WL_MODE_11B) | BIT(BTC_WL_MODE_11G)
        }
    }
    let wl = &mut st.wl;
    let ori = wl.role.link_mode;
    wl.is_5g_hi_ch = hi_ch;
    wl.bg_mode = bg;
    wl.go_client_exist = 0;
    wl.noa_exist = 0;
    wl.role.p2p_exist = 0;
    wl.role.role_map = role_map;
    update_link_mode(st)?;
    // _sync_wl_link_mode_v0 only feeds the coex log
    if st.wl.role.link_mode != ori {
        st.wl.role.link_mode_chg = 1;
        st.wl.link_mode_chg = 1;
    }
    Ok(())
}

// Linux: rtw89_btc_ntfy_role_info (coex.c:9229) with _ntfy_role_info (coex.c:9157); the station is
// legacy-only (no HT/VHT/HE), so mode 0. CONN_START and DIS_CONN also (re)queue the act1 work 1 s
// later in Linux; the caller runs `act1_work` then.
pub fn role<B: Bus + ?Sized>(
    bus: &mut B,
    st: &mut BtcState,
    hooks: &mut dyn BtcHooks<B>,
    state: RoleState,
    req: &RoleReq,
) -> Rtw89Result<()> {
    let start_stop = matches!(state, RoleState::Start | RoleState::Stop);
    let band = band_idx(req.band);
    let mut info = LinkInfo {
        mode: req.mode,
        role: RTW89_WIFI_ROLE_STATION,
        phy: RTW89_PHY_0,
        pid: req.port,
        active: true,
        band,
        ch: req.ch,
        bw: req.bw,
        mac_id: if start_stop { 0 } else { req.mac_id },
        connected: if state == RoleState::ConnEnd {
            mlme::LINKED
        } else {
            mlme::NO_LINK
        },
        traffic: None,
        rssi: None,
        rssi_state: None,
        busy: 0,
        dir: 0,
        rx_rate_drop_cnt: 0,
    };
    let port = req.port as usize;
    if port >= usize::from(st.ver.max_role_num) || port >= MAX_ROLE {
        st.push(BtcEvent::RoleIgnored { port: req.port });
        return Ok(());
    }
    if start_stop {
        info.active = state == RoleState::Start;
        st.wl.rlink_info[port] = Some(info);
        return Ok(());
    }
    st.wl.rlink_info[port] = Some(info.clone());
    let refresh = state == RoleState::ConnEnd;
    st.wl.status.transacting = u32::from(!refresh);
    if refresh {
        update_wl_info(st, &info)?;
        h2c_cx_role(bus, st, hooks)?; // _fw_set_drv_info CXDRVINFO_ROLE, fwlrole 2
    }
    run_coex(bus, st, hooks, rsn::NTFY_ROLE_INFO)
}

// Linux: rtw89_btc_ntfy_role_info (coex.c:9229) around association: CONN_START from
// rtw89_core_sta_link_add, CONN_END from rtw89_core_sta_link_assoc
pub fn connect<B: Bus + ?Sized>(
    bus: &mut B,
    st: &mut BtcState,
    hooks: &mut dyn BtcHooks<B>,
    start: bool,
    req: &RoleReq,
) -> Rtw89Result<()> {
    let state = if start {
        RoleState::ConnStart
    } else {
        RoleState::ConnEnd
    };
    role(bus, st, hooks, state, req)
}

// Linux: rtw89_btc_ntfy_wl_rfk (coex.c:9466) with _ntfy_wl_rfk (coex.c:9400). A START is allowed
// at once, so Linux's 40 us poll loop runs once. Linux also arms a 300 ms watchdog
// (rtw89_coex_rfk_chk_work); BitsOS has none. Returns whether the calibration may go on.
pub fn wl_rfk<B: Bus + ?Sized>(
    bus: &mut B,
    st: &mut BtcState,
    hooks: &mut dyn BtcHooks<B>,
    phy_map: u32,
    typ: u32,
    state: u32,
) -> Rtw89Result<bool> {
    let rfk = &mut st.wl.rfk_info;
    rfk.typ = typ;
    rfk.path_map = field(phy_map, BTC_RFK_PATH_MAP);
    rfk.phy_map = field(phy_map, BTC_RFK_PHY_MAP);
    rfk.band = field(phy_map, BTC_RFK_BAND_MAP);
    match state {
        BTC_WRFK_START | BTC_WRFK_STOP => {
            rfk.state = state;
            write_scbd(bus, st, wscb::WLRFK, state == BTC_WRFK_START)?;
        }
        // allowed, no coex run
        BTC_WRFK_ONESHOT_START | BTC_WRFK_ONESHOT_STOP => return Ok(typ != BTC_WRFKT_RXDCK),
        _ => return Ok(false),
    }
    run_coex(bus, st, hooks, rsn::NTFY_WL_RFK)?;
    Ok(true)
}

// Linux: rtw89_btc_ntfy_conn_rfk (coex.c:13366)
pub fn conn_rfk(st: &mut BtcState, start: bool) {
    st.wl.rfk_info.con_rfk = start;
}

// Linux: rtw89_btc_ntfy_preserve_bt_time (coex.c:13351): waits only while BT streams A2DP, which
// a silent BT side never does (at most `ms`, the caller's bound)
pub fn preserve_bt_time<B: Bus + ?Sized>(bus: &mut B, st: &BtcState, ms: u32) -> Rtw89Result<()> {
    if st.bt0.a2dp != 0 {
        bus.delay_us(ms.saturating_mul(1000))?;
    }
    Ok(())
}

// Linux: rtw89_btc_ntfy_specific_packet (coex.c:8859). Linux runs rtw89_coex_act1_work one second
// after DHCP/EAPOL (each one restarts the second) and EAPOL_END cancels it; the caller keeps that
// time and then runs `act1_work`.
pub fn packet<B: Bus + ?Sized>(
    bus: &mut B,
    st: &mut BtcState,
    hooks: &mut dyn BtcHooks<B>,
    typ: Packet,
) -> Rtw89Result<()> {
    let s = &mut st.wl.status;
    match typ {
        Packet::Dhcp => {
            s.handshake = 0;
            s.four_way = 0;
            s.dhcp = 1;
        }
        Packet::Eapol => {
            s.handshake = 0;
            s.four_way = 1;
            s.dhcp = 0;
        }
        Packet::EapolEnd => s.four_way = 0,
        Packet::Arp | Packet::Icmp => return Ok(()),
    }
    run_coex(bus, st, hooks, rsn::NTFY_SPECIFIC_PACKET)
}

// Linux: rtw89_coex_act1_work (coex.c:7716)
pub fn act1_work<B: Bus + ?Sized>(
    bus: &mut B,
    st: &mut BtcState,
    hooks: &mut dyn BtcHooks<B>,
) -> Rtw89Result<()> {
    let s = &mut st.wl.status;
    if s.handshake != 0 || s.four_way != 0 || s.dhcp != 0 {
        s.four_way = 0;
        s.dhcp = 0;
        s.handshake = 0;
    }
    run_coex(bus, st, hooks, rsn::ACT1_WORK)
}
