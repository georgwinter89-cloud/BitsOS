//! rtw89's sta_state steps for a station (JS js/sta-ops.mjs; mac80211.c __rtw89_ops_sta_*, core.c
//! rtw89_core_sta_link_*), the firmware packet offload they use, the H2C-and-wait helper with
//! its C2H handling, and rtw89_ops_flush with the TX flushes it is made of.

use crate::btc::RoleState;
use crate::bus::{Bus, field};
use crate::error::Rtw89Result;
use crate::fw::C2h;
use crate::tables::regs::*;
use crate::txrx::ieee;

use super::WaitClock;
use super::h2c::{self, GeneralPkts, H2cMsg, PktDrop};
use super::io::{Chip, RxItem};
use super::ra::{self, Ewma};
use super::{ApLink, ApState, Sta, StaEnv, StaEvent, btc_role, vif_type_mapping};
use wifi::replay::DupCheck;

// Linux: __rtw89_ops_sta_add (mac80211.c:488) with rtw89_core_sta_link_add (core.c:5735),
// station mode: the AP's link takes the vif's MAC ID. `supp_rates`: link_sta->supp_rates[band],
// which mac80211 takes from the BSS before the station is added (ieee80211_mgd_setup_link_sta)
pub fn sta_add<B: Bus + ?Sized>(
    bus: &mut B,
    chip: &mut Chip,
    sta: &mut Sta,
    env: &mut dyn StaEnv<B>,
    ap_addr: [u8; 6],
    supp_rates: u32,
) -> Rtw89Result<()> {
    sta.ap = Some(ApLink {
        mac_id: sta.vif.mac_id,
        addr: ap_addr,
        avg_rssi: Ewma::default(),
        rssi: [Ewma::default(); 2],
        supp_rates,
        rx_nss: 1,
        ra: None,
        er_cap: false,
        assoc_link: false,
        state: ApState::None,
        dup: DupCheck::new(),
        rx_hw_rate: 0,
        prev_rssi: 0,
    });
    // rtw89_reg_6ghz_recalc: 2.4/5 GHz only, nothing changes
    let now = env.now_ms(bus);
    btc_role(bus, chip, sta, now, RoleState::ConnStart)?;
    chip.rfk_channel(bus, sta.cur_chan())?; // rtw89_chip_rfk_channel
    let assoc = sta.dev.total_sta_assoc;
    ra::dig_ctrl(bus, &mut chip.run.phy, assoc, true, false) // rtw89_phy_dig_suspend
    // rtw89_fw_h2c_init_trx_protect -> rtw89_fw_h2c_trx_protect (fw.c:8003): BE chips only
}

// Linux: __rtw89_ops_sta_assoc (mac80211.c:552), station_mode true, with rtw89_core_sta_link_assoc
// (core.c:5878)
pub fn sta_assoc<B: Bus + ?Sized>(
    bus: &mut B,
    chip: &mut Chip,
    sta: &mut Sta,
    env: &mut dyn StaEnv<B>,
) -> Rtw89Result<()> {
    let Some(ap) = sta.ap.clone() else {
        return Ok(());
    };
    vif_type_mapping(&mut sta.vif, true);
    let band_2g = sta.vif.band == crate::phy::Band::B2g;
    chip.h2c(bus, &h2c::h2c_assoc_cmac_tbl(&sta.vif, Some(&ap), band_2g))?;
    chip.h2c(bus, &h2c::h2c_join_info(&sta.vif, Some(&ap), false))?;
    chip.h2c(bus, &h2c::h2c_addr_cam(&sta.vif, Some(&ap), None))?;
    // rtw89_phy_ra_assoc; hal->tx_nss of phy's phycap
    let tx_nss = match chip.run.phy.hal.tx_nss {
        0 => 2,
        n => n,
    };
    let info = ra::ra_assoc(&ap, band_2g, tx_nss);
    chip.h2c(bus, &h2c::h2c_ra(&info))?;
    if let Some(a) = sta.ap.as_mut() {
        a.ra = Some(info);
        // rtw89_mac_bf_assoc: no beamformer capability (no VHT/HE); bf_monitor_calc counts only
        a.er_cap = false; // rtw89_sta_link_can_er: HE only
    }
    let now = env.now_ms(bus);
    btc_role(bus, chip, sta, now, RoleState::ConnEnd)?;
    // rtw89_core_get_no_ul_ofdma_htc: HE only
    ra::phy_ul_tb_assoc(bus, &mut sta.vif)?;
    // rtw89_core_bcn_track_assoc: software thresholds only
    general_pkt(bus, chip, sta, env)?;
    if chip.beacon_filter {
        chip.h2c(bus, &h2c::h2c_bcn_fltr_cfg(&sta.vif, true))?; // its own feature check
    }
    let assoc = sta.dev.total_sta_assoc;
    ra::dig_ctrl(bus, &mut chip.run.phy, assoc, false, false)?; // rtw89_phy_dig_resume
    if let Some(a) = sta.ap.as_mut() {
        a.assoc_link = true;
    }
    sta.dev.total_sta_assoc += 1;
    Ok(())
}

// Linux: __rtw89_ops_sta_disassoc (mac80211.c:582) with rtw89_core_sta_link_disassoc (core.c:5799)
pub fn sta_disassoc<B: Bus + ?Sized>(
    bus: &mut B,
    chip: &mut Chip,
    sta: &mut Sta,
) -> Rtw89Result<()> {
    if let Some(a) = sta.ap.as_mut() {
        a.assoc_link = false;
    }
    if chip.beacon_filter {
        chip.h2c(bus, &h2c::h2c_bcn_fltr_cfg(&sta.vif, false))?;
    }
    // rtw89_core_bcn_track_reset: software only
    sta.dev.total_sta_assoc = sta.dev.total_sta_assoc.saturating_sub(1);
    Ok(())
}

// Linux: __rtw89_ops_sta_disconnect (mac80211.c:609) with rtw89_core_sta_link_disconnect
// (core.c:5818)
pub fn sta_disconnect<B: Bus + ?Sized>(
    bus: &mut B,
    chip: &mut Chip,
    sta: &mut Sta,
    env: &mut dyn StaEnv<B>,
) -> Rtw89Result<()> {
    // rtw89_mac_bf_monitor_calc: counting only; rtw89_mac_bf_disassoc -> rtw89_mac_bfee_ctrl(false)
    bus.clr32(
        R_AX_BFMEE_RESP_OPTION,
        B_AX_BFMEE_HT_NDPA_EN | B_AX_BFMEE_VHT_NDPA_EN | B_AX_BFMEE_HE_NDPA_EN,
    )?;
    vif_type_mapping(&mut sta.vif, false);
    release_general_pkts(bus, chip, sta, env, true)?;
    let ap = sta.ap.clone();
    let band_2g = sta.vif.band == crate::phy::Band::B2g;
    chip.h2c(
        bus,
        &h2c::h2c_assoc_cmac_tbl(&sta.vif, ap.as_ref(), band_2g),
    )?;
    chip.h2c(bus, &h2c::h2c_join_info(&sta.vif, ap.as_ref(), true))?;
    chip.h2c(bus, &h2c::h2c_addr_cam(&sta.vif, ap.as_ref(), None))
}

// Linux: __rtw89_ops_sta_remove (mac80211.c:633) with rtw89_core_sta_link_remove (core.c:5981)
pub fn sta_remove<B: Bus + ?Sized>(
    bus: &mut B,
    chip: &mut Chip,
    sta: &mut Sta,
    env: &mut dyn StaEnv<B>,
) -> Rtw89Result<()> {
    // rtw89_reg_6ghz_recalc(false): nothing changes outside 6 GHz
    let now = env.now_ms(bus);
    btc_role(bus, chip, sta, now, RoleState::Disconnect)?;
    sta.ap = None;
    Ok(())
}

// ---- firmware packet offload and the H2C/C2H wait ----

/// RTW89_WAIT_FOR_COND_TIMEOUT
const WAIT_MS: u64 = 100;

fn rsp_field(c: &C2h, mask: u32) -> u32 {
    let w = c
        .payload
        .get(..4)
        .map_or(0, |b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    field(w, mask)
}

// rtw89_mac_c2h_pkt_ofld_rsp (mac.c:5586): w2 after the C2H header holds id, op, length
fn pkt_ofld_rsp(c: &C2h, id: u32, op: u32) -> bool {
    c.cat == RTW89_C2H_CAT_MAC
        && c.cls == RTW89_MAC_C2H_CLASS_OFLD
        && c.payload.len() >= 4
        && c.func == RTW89_MAC_C2H_FUNC_PKT_OFLD_RSP
        && rsp_field(c, RTW89_C2H_PKT_OFLD_RSP_W2_PTK_ID) == id
        && rsp_field(c, RTW89_C2H_PKT_OFLD_RSP_W2_PTK_OP) == op
}

// Linux: rtw89_h2c_tx_and_wait (fw.c:11234) + rtw89_wait_for_cond_eval (core.c:6546),
// RTW89_WAIT_FOR_COND_TIMEOUT 100 ms, at least 3 RX polls (sta-ops.mjs). Returns the matching
// C2H or `None` on timeout; frames and other C2Hs that come with it are kept or handled. Between
// polls the thread sleeps 1 ms (the JS polled without a pause).
pub fn h2c_and_wait<B: Bus + ?Sized>(
    bus: &mut B,
    chip: &mut Chip,
    sta: &mut Sta,
    env: &mut dyn StaEnv<B>,
    m: &H2cMsg,
    matches: &dyn Fn(&C2h) -> bool,
) -> Rtw89Result<Option<C2h>> {
    chip.h2c(bus, m)?;
    let mut clock = WaitClock::new(bus, env);
    let end = clock.now(bus, env) + WAIT_MS;
    let mut polls = 0;
    loop {
        let mut found = None;
        for r in chip.rx_poll(bus, 64)? {
            match r {
                RxItem::C2h(c) if found.is_none() && matches(&c) => found = Some(c),
                RxItem::C2h(c) => handle_c2h(bus, chip, sta, &c)?,
                other => sta.rx_backlog.push_back(other),
            }
        }
        polls += 1;
        if found.is_some() {
            return Ok(found);
        }
        if polls >= 3 && clock.now(bus, env) >= end {
            return Ok(None);
        }
        clock.sleep(bus, env, 1);
    }
}

// Linux: rtw89_fw_c2h_cmd_handle (fw.c:9087) for what a station gets: category OUTSRC with a BTC
// class goes to rtw89_btc_c2h_handle; rtw89_mac_c2h_bcn_fltr_rpt (mac.c:5446) ->
// rtw89_mac_bcn_fltr_rpt (mac.c:5390): on beacon loss the filter is armed again; mac80211's
// ieee80211_beacon_loss (probe, then give up) is the caller's through `Sta::beacon_loss` (the
// supervisor joins again). RSSI events (no CQM configured) and other C2Hs are ignored.
pub fn handle_c2h<B: Bus + ?Sized>(
    bus: &mut B,
    chip: &mut Chip,
    sta: &mut Sta,
    c: &C2h,
) -> Rtw89Result<()> {
    if c.cat == RTW89_C2H_CAT_OUTSRC
        && (RTW89_PHY_C2H_CLASS_BTC_MIN..=RTW89_PHY_C2H_CLASS_BTC_MAX).contains(&c.cls)
    {
        chip.btc_c2h(c);
        return Ok(());
    }
    if !beacon_loss_report(sta, c) {
        return Ok(());
    }
    if !sta.scanning {
        sta.beacon_loss = true;
        sta.push(StaEvent::BeaconLoss);
    }
    if sta.vif.net_type == RTW89_NET_TYPE_INFRA && chip.beacon_filter {
        chip.h2c(bus, &h2c::h2c_bcn_fltr_cfg(&sta.vif, true))?;
    }
    Ok(())
}

// Linux: rtw89_mac_c2h_bcn_fltr_rpt (mac.c:5446) -> rtw89_mac_bcn_fltr_rpt (mac.c:5390): a
// beacon filter report of this vif's MAC ID that says beacon loss (a torn one is ignored)
pub fn beacon_loss_report(sta: &Sta, c: &C2h) -> bool {
    c.cat == RTW89_C2H_CAT_MAC
        && c.cls == RTW89_MAC_C2H_CLASS_OFLD
        && c.func == RTW89_MAC_C2H_FUNC_BCNFLTR_RPT
        && c.payload.len() >= 4
        && rsp_field(c, RTW89_C2H_MAC_BCNFLTR_RPT_W2_MACID) == sta.vif.mac_id
        && rsp_field(c, RTW89_C2H_MAC_BCNFLTR_RPT_W2_TYPE) == RTW89_BCN_FLTR_BEACON_LOSS
}

// Linux: rtw89_fw_h2c_del_pkt_offload (fw.c:7309)
pub fn del_pkt_offload<B: Bus + ?Sized>(
    bus: &mut B,
    chip: &mut Chip,
    sta: &mut Sta,
    env: &mut dyn StaEnv<B>,
    id: u32,
) -> Rtw89Result<bool> {
    let m = h2c::h2c_pkt_ofld_del(id);
    let c = h2c_and_wait(bus, chip, sta, env, &m, &|c| {
        pkt_ofld_rsp(c, id, RTW89_PKT_OFLD_OP_DEL)
    })?;
    if !c.is_some_and(|c| rsp_field(&c, RTW89_C2H_PKT_OFLD_RSP_W2_PTK_LEN) != 0) {
        return Ok(false);
    }
    if let Some(b) = sta.dev.pkt_offload_map.get_mut(id as usize) {
        *b = false;
    }
    Ok(true)
}

// Linux: rtw89_fw_h2c_add_pkt_offload (fw.c:7405) with __rtw89_fw_h2c_add_pkt_offload (fw.c:7347)
pub fn add_pkt_offload<B: Bus + ?Sized>(
    bus: &mut B,
    chip: &mut Chip,
    sta: &mut Sta,
    env: &mut dyn StaEnv<B>,
    frame: &[u8],
) -> Rtw89Result<Option<u32>> {
    for _attempt in 0..2 {
        let Some(id) = sta.dev.pkt_offload_map.iter().position(|&b| !b) else {
            return Ok(None);
        };
        sta.dev.pkt_offload_map[id] = true;
        let id = id as u32;
        let m = h2c::h2c_pkt_ofld_add(id, frame);
        let c = h2c_and_wait(bus, chip, sta, env, &m, &|c| {
            pkt_ofld_rsp(c, id, RTW89_PKT_OFLD_OP_ADD)
        })?;
        if c.is_some_and(|c| rsp_field(&c, RTW89_C2H_PKT_OFLD_RSP_W2_PTK_LEN) != 0) {
            return Ok(Some(id));
        }
        sta.push(StaEvent::PktOffloadFailed { id });
        // the firmware may have taken it although the answer is missing: delete, then once more
        del_pkt_offload(bus, chip, sta, env, id)?;
        sta.dev.pkt_offload_map[id as usize] = false;
    }
    Ok(None)
}

// Linux: none (mac80211 ieee80211_pspoll_get (linux-6.6/net/mac80211/tx.c:5668)): control
// PS-Poll to the BSSID, AID with its two top bits set
pub fn ps_poll_frame(bssid: &[u8; 6], own: &[u8; 6], aid: u16) -> [u8; 16] {
    let mut f = [0u8; 16];
    f[0..2].copy_from_slice(&0x00a4u16.to_le_bytes()); // IEEE80211_FTYPE_CTL | _STYPE_PSPOLL
    f[2..4].copy_from_slice(&(aid | 0xc000).to_le_bytes());
    f[4..10].copy_from_slice(bssid);
    f[10..16].copy_from_slice(own);
    f
}

// Linux: rtw89_fw_h2c_general_pkt (fw.c:3114) with rtw89_fw_h2c_add_general_pkt (fw.c:3024);
// the frames of mac80211's ieee80211_pspoll_get and ieee80211_nullfunc_get
// (linux-6.6/net/mac80211/tx.c:5703; the AP's station is not WME, so the "QoS null" offload gets
// a plain null frame too)
pub fn general_pkt<B: Bus + ?Sized>(
    bus: &mut B,
    chip: &mut Chip,
    sta: &mut Sta,
    env: &mut dyn StaEnv<B>,
) -> Rtw89Result<()> {
    let (own, bssid, aid) = (sta.vif.addr, sta.vif.bssid, sta.vif.aid);
    let pspoll = ps_poll_frame(&bssid, &own, aid);
    let null = ieee::null_func(&own, &bssid, 0, None);
    let mut ids = GeneralPkts::default();
    for (slot, frame) in [(0, &pspoll[..]), (1, &null[..]), (2, &null[..])] {
        let Some(id) = add_pkt_offload(bus, chip, sta, env, frame)? else {
            continue;
        };
        sta.vif.general_pkts.push(id);
        match slot {
            0 => ids.ps_poll = Some(id),
            1 => ids.null_data = Some(id),
            _ => ids.qos_null = Some(id),
        }
    }
    chip.h2c(bus, &h2c::h2c_general_pkt(sta.vif.mac_id, &ids))
}

// Linux: rtw89_fw_release_general_pkt_list_vif (fw.c:3083)
pub fn release_general_pkts<B: Bus + ?Sized>(
    bus: &mut B,
    chip: &mut Chip,
    sta: &mut Sta,
    env: &mut dyn StaEnv<B>,
    notify_fw: bool,
) -> Rtw89Result<()> {
    let ids = core::mem::take(&mut sta.vif.general_pkts);
    for id in ids {
        if notify_fw {
            del_pkt_offload(bus, chip, sta, env, id)?;
        } else if let Some(b) = sta.dev.pkt_offload_map.get_mut(id as usize) {
            *b = false;
        }
    }
    Ok(())
}

// ---- TX flushes ----

// Linux: rtw89_pci_ops_flush_queues (pci.c:1432) -> __pci_flush_txch (pci.c:1388) per TX ring in
// channel order, FWCMD (CH12) skipped: up to 60 reads of the ring index, 1 us apart; a timeout is
// only logged. The first read of every ring comes first (the JS batched them into one MCP round
// trip); a busy ring is then read again on its own.
pub fn hci_flush_queues<B: Bus + ?Sized>(
    bus: &mut B,
    chip: &mut Chip,
    sta: &mut Sta,
) -> Rtw89Result<()> {
    let rings: alloc::vec::Vec<(u32, u32)> = chip
        .rings()?
        .tx
        .iter()
        .filter(|r| r.name != "CH12")
        .map(|r| (r.idx, r.wp))
        .collect();
    let mut first = alloc::vec::Vec::with_capacity(rings.len());
    for &(idx, _) in &rings {
        first.push(bus.r32(idx)?);
    }
    for (&(idx_reg, wp), &v) in rings.iter().zip(&first) {
        let mut idx = v;
        let mut i = 1;
        while i < 60 && field(idx, TXBD_HW_IDX_MASK) != wp {
            bus.delay_us(1)?;
            idx = bus.r32(idx_reg)?;
            i += 1;
        }
        if field(idx, TXBD_HW_IDX_MASK) != wp {
            sta.push(StaEvent::FlushTimeout { what: "TX ring" });
        }
    }
    Ok(())
}

/// dle_is_txq_empty (mac.c:2388): every WDE/PLE empty bit of the TX queues
const DLE_TXQ_EMPTY: u32 = B_AX_WDE_EMPTY_QUE_CMAC0_ALL_AC
    | B_AX_WDE_EMPTY_QUE_CMAC0_MBH
    | B_AX_WDE_EMPTY_QUE_CMAC1_MBH
    | B_AX_WDE_EMPTY_QUE_CMAC0_WMM0
    | B_AX_WDE_EMPTY_QUE_CMAC0_WMM1
    | B_AX_WDE_EMPTY_QUE_OTHERS
    | B_AX_PLE_EMPTY_QUE_DMAC_MPDU_TX
    | B_AX_PLE_EMPTY_QTA_DMAC_H2C
    | B_AX_PLE_EMPTY_QUE_DMAC_SEC_TX
    | B_AX_WDE_EMPTY_QUE_DMAC_PKTIN
    | B_AX_WDE_EMPTY_QTA_DMAC_HIF
    | B_AX_WDE_EMPTY_QTA_DMAC_WLAN_CPU
    | B_AX_WDE_EMPTY_QTA_DMAC_PKTIN
    | B_AX_WDE_EMPTY_QTA_DMAC_CPUIO
    | B_AX_PLE_EMPTY_QTA_DMAC_B0_TXPL
    | B_AX_PLE_EMPTY_QTA_DMAC_B1_TXPL
    | B_AX_PLE_EMPTY_QTA_DMAC_MPDU_TX
    | B_AX_PLE_EMPTY_QTA_DMAC_CPUIO
    | B_AX_WDE_EMPTY_QTA_DMAC_DATA_CPU
    | B_AX_PLE_EMPTY_QTA_DMAC_WLAN_CPU;

// Linux: rtw89_mac_flush_txq (mac.c:6352): read_poll_timeout 10 ms steps, 200 ms; a timeout is
// only logged
pub fn mac_flush_txq<B: Bus + ?Sized>(bus: &mut B, sta: &mut Sta) -> Rtw89Result<bool> {
    for _ in 0..=20 {
        if bus.r32(R_AX_DLE_EMPTY0)? & DLE_TXQ_EMPTY == DLE_TXQ_EMPTY {
            return Ok(true);
        }
        bus.delay_us(10_000)?;
    }
    sta.push(StaEvent::FlushTimeout {
        what: "DLE TX queues",
    });
    Ok(false)
}

// Linux: rtw89_mac_pkt_drop_vif (mac.c:7299) -> rtw89_mac_pkt_drop_sta (mac.c:7255) for every
// station of the vif: the AP's link
pub fn pkt_drop_vif<B: Bus + ?Sized>(bus: &mut B, chip: &mut Chip, sta: &Sta) -> Rtw89Result<()> {
    let Some(ap) = &sta.ap else { return Ok(()) };
    for sel in [
        RTW89_PKT_DROP_SEL_MACID_BE_ONCE,
        RTW89_PKT_DROP_SEL_MACID_BK_ONCE,
        RTW89_PKT_DROP_SEL_MACID_VI_ONCE,
        RTW89_PKT_DROP_SEL_MACID_VO_ONCE,
    ] {
        let d = PktDrop {
            sel,
            macid: ap.mac_id,
            band: sta.vif.mac_idx,
            port: sta.vif.port,
            mbssid: 0,
            tf_trs: sta.vif.trigger,
        };
        chip.h2c(bus, &h2c::h2c_pkt_drop(&d))?;
    }
    Ok(())
}

// Linux: rtw89_ops_flush (mac80211.c:1122), all queues. With drop the firmware drops what is
// still queued for the AP (8852B: NO_PACKET_DROP is an RTL8852A feature only, fw.c:870)
pub fn ops_flush<B: Bus + ?Sized>(
    bus: &mut B,
    chip: &mut Chip,
    sta: &mut Sta,
    drop: bool,
) -> Rtw89Result<()> {
    hci_flush_queues(bus, chip, sta)?; // drop only silences its timeout message
    if drop {
        pkt_drop_vif(bus, chip, sta) // __rtw89_drop_packets (mac80211.c:1109)
    } else {
        mac_flush_txq(bus, sta).map(|_| ())
    }
}
