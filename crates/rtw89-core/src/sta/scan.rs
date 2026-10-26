//! The software scan (JS js/sta-scan.mjs). With firmware 0.29.29.18 Linux uses the firmware's
//! scan offload (rtw89_ops_hw_scan, SCAN_OFFLOAD >= 0.29.29.0, fw.c:874) and real channel
//! contexts; rtw89 keeps the software path (rtw89_ops_sw_scan_start/_complete) for firmware
//! without it, where mac80211 changes channels through rtw89_ops_config. BitsOS takes that path:
//! the channel loop of mac80211's scan.c, rtw89's hooks around it. The list is wifi's ScanList.

use alloc::vec::Vec;

use crate::bus::Bus;
use crate::error::{Rtw89Error, Rtw89Result};
use crate::phy::Band;
use crate::txrx::TxOpts;
use crate::txrx::ieee::{self, ProbeReq};
use crate::txrx::parse;

use wifi::scan::{Band as WBand, Bss, ScanList};

use super::WaitClock;
use super::io::{Chip, RxItem};
use super::ops::{handle_c2h, ops_flush};
use super::ra::{hci_recalc_int_mit, phy_config_edcca};
use super::track::track_due;
use super::{
    FIF_BCN_PRBRESP_PROMISC, Sta, StaEnv, StaEvent, btc_act1_due, config_channel, configure_filter,
    h2c,
};

/// One channel of a scan: `passive` sends no probe request
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanChan {
    pub ch: u32,
    pub passive: bool,
}

/// What to scan (sta-scan.mjs scan's options)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanReq {
    pub band: Band,
    pub channels: Vec<ScanChan>,
    /// how long each channel is listened to (ms)
    pub dwell_ms: u32,
    /// probe request SSIDs, one probe each (empty = wildcard)
    pub ssids: Vec<Vec<u8>>,
}

impl ScanReq {
    /// A wildcard scan of `channels`
    pub fn new(band: Band, channels: &[u32], dwell_ms: u32) -> ScanReq {
        ScanReq {
            band,
            channels: channels
                .iter()
                .map(|&ch| ScanChan { ch, passive: false })
                .collect(),
            dwell_ms,
            ssids: alloc::vec![Vec::new()],
        }
    }
}

fn wband(b: Band) -> WBand {
    if b == Band::B5g {
        WBand::B5g
    } else {
        WBand::B2g
    }
}

// Linux: rtw89_core_scan_start (core.c:7063), hw_scan false
pub fn core_scan_start<B: Bus + ?Sized>(
    bus: &mut B,
    chip: &mut Chip,
    sta: &mut Sta,
    env: &mut dyn StaEnv<B>,
    mac_addr: [u8; 6],
) -> Rtw89Result<()> {
    sta.scanning = true;
    chip.txrx.scanning = true;
    env.scanning(bus, true);
    sta.vif.mac_addr = mac_addr;
    let chan = sta.cur_chan();
    chip.btc_scan(bus, true, chan.band)?; // rtw89_btc_ntfy_scan_start(phy 0, the channel's band)
    chip.rfk_scan(bus, true, Some(chan))?; // rtw89_chip_rfk_scan -> rtw8852b_wifi_scan_notify
    let st = sta.track.stats;
    hci_recalc_int_mit(bus, sta.scanning, (st.tx_tfc_lv, st.rx_tfc_lv))?; // scanning: no mitigation
    phy_config_edcca(bus, &mut sta.edcca_bak, true)?;
    // rtw89_tas_scan: 8852B support_tas false
    chip.h2c(bus, &h2c::h2c_addr_cam(&sta.vif, None, Some(&mac_addr)))
}

// Linux: rtw89_core_scan_complete (core.c:7083), hw_scan false (no null frame, no IPS)
pub fn core_scan_complete<B: Bus + ?Sized>(
    bus: &mut B,
    chip: &mut Chip,
    sta: &mut Sta,
    env: &mut dyn StaEnv<B>,
) -> Rtw89Result<()> {
    sta.vif.mac_addr = sta.vif.addr;
    chip.h2c(bus, &h2c::h2c_addr_cam(&sta.vif, None, None))?;
    let chan = sta.cur_chan();
    chip.rfk_scan(bus, false, Some(chan))?;
    chip.btc_scan(bus, false, chan.band)?;
    phy_config_edcca(bus, &mut sta.edcca_bak, false)?;
    sta.scanning = false;
    chip.txrx.scanning = false;
    env.scanning(bus, false);
    chip.run.phy.dig.bypass_dig = true; // the next track work's DIG skips one round
    Ok(())
}

// Linux: none (mac80211's ieee80211_start_sw_scan (linux-6.6/net/mac80211/scan.c:512):
// drv_sw_scan_start, the flush, configure_filter; ieee80211_scan_work; __ieee80211_scan_completed
// (linux-6.6/net/mac80211/scan.c:403): hw_config back to the operating channel, configure_filter,
// drv_sw_scan_complete) around rtw89_ops_sw_scan_start (mac80211.c:1231) and
// rtw89_ops_sw_scan_complete (mac80211.c:1252). As mac80211 (ieee80211_scan_state_set_channel):
// a channel the country marks NO-IR or radar is scanned passively; one it does not allow stops
// the scan before it starts. An error ends the scan as an aborted one (the completion steps
// run), then it is returned. The list comes strongest first.
pub fn scan<B: Bus + ?Sized>(
    bus: &mut B,
    chip: &mut Chip,
    sta: &mut Sta,
    env: &mut dyn StaEnv<B>,
    req: &ScanReq,
) -> Rtw89Result<Vec<Bss>> {
    if sta.vif.assoc {
        return Err(Rtw89Error::Invalid {
            what: "scan while associated: not supported (no off-channel return)",
            value: 0,
        });
    }
    if sta.state == super::LinkState::Down {
        return Err(Rtw89Error::Invalid {
            what: "scan: the interface is down",
            value: 0,
        });
    }
    let mut plan = Vec::with_capacity(req.channels.len());
    for c in &req.channels {
        let f = chip.channel_flags(req.band, c.ch);
        if !f.allowed {
            return Err(Rtw89Error::Invalid {
                what: "scan: channel not allowed in this country",
                value: c.ch,
            });
        }
        plan.push(ScanChan {
            ch: c.ch,
            passive: c.passive || f.no_ir || f.radar,
        });
    }
    let (op_band, op_ch) = (sta.vif.band, sta.chan.map(|c| c.ch));
    let mut found = ScanList::new();
    let addr = sta.vif.addr;
    let mut failed = (|| -> Rtw89Result<()> {
        core_scan_start(bus, chip, sta, env, addr)?; // drv_sw_scan_start
        ops_flush(bus, chip, sta, false)?; // ieee80211_flush_queues(local, NULL, false)
        configure_filter(bus, chip, sta, FIF_BCN_PRBRESP_PROMISC)?; // SCAN_SW_SCANNING
        for c in &plan {
            config_channel(bus, chip, sta, env, req.band, c.ch)?; // ieee80211_scan_state_set_channel
            if !c.passive {
                for ssid in &req.ssids {
                    let mut p = ProbeReq::new(sta.vif.mac_addr);
                    p.ssid = ssid;
                    p.band = req.band;
                    p.ch = Some(c.ch as u8);
                    let mut f = ieee::probe_req(&p);
                    let o = TxOpts {
                        mac_id: Some(sta.vif.mac_id),
                        sta: false,
                        band: Some(req.band),
                        ..TxOpts::default()
                    };
                    super::link::tx(bus, chip, sta, env, &mut f, &o)?;
                }
            }
            let mut clock = WaitClock::new(bus, env);
            let end = clock.now(bus, env) + u64::from(req.dwell_ms);
            loop {
                collect(bus, chip, sta, env, c.ch, &mut found)?;
                let now = clock.now(bus, env);
                if now >= end {
                    break;
                }
                clock.sleep(bus, env, (end - now).min(10) as u32);
                if clock.now(bus, env) >= end {
                    break;
                }
            }
        }
        Ok(())
    })()
    .err();
    // __ieee80211_scan_completed: back to the operating channel, the filter, sw_scan_complete
    let done = (|| -> Rtw89Result<()> {
        if failed.is_none()
            && let Some(ch) = op_ch
            && Some(ch) != sta.chan.map(|c| c.ch)
        {
            config_channel(bus, chip, sta, env, op_band, ch)?;
        }
        configure_filter(bus, chip, sta, 0)?; // still masked: rtwdev->scanning is true here
        if sta.scanning {
            core_scan_complete(bus, chip, sta, env)?; // rtw89_ops_sw_scan_complete
        }
        // mac80211 runs configure_filter again on the next RX mode change of the interface
        // (ndo_set_rx_mode); BitsOS has no such event, so once now (NOTES-sta.md decision 5)
        configure_filter(bus, chip, sta, 0)
    })();
    if let Err(e) = done {
        failed.get_or_insert(e);
    }
    sta.scanning = false;
    chip.txrx.scanning = false;
    if let Some(e) = failed {
        sta.push(StaEvent::ScanAborted);
        return Err(e);
    }
    sta.push(StaEvent::ScanDone {
        found: found.len() as u32,
    });
    Ok(found.sorted())
}

// Linux: none (sta-mlme.mjs bssFromFrame, cfg80211_inform_bss_frame): a scan list entry from a
// beacon or probe response (txrx's parser), the RX channel when the frame names none, its RSSI
pub fn bss_from_frame(mpdu: &[u8], rx_ch: u32, rssi: Option<i32>, band: Band) -> Option<Bss> {
    let b = parse::beacon_parse(mpdu)?;
    Some(Bss {
        bssid: b.bssid,
        ssid: b.ssid_raw,
        band: wband(band),
        ch: b.ch.unwrap_or(rx_ch as u8),
        beacon_int: b.beacon_int,
        capability: b.capability,
        rate_bytes: b.rate_bytes,
        rsn_ie: b.rsn_ie,
        dtim_period: b.dtim_period,
        rssi,
    })
}

// beacons and probe responses of one RX poll into the list (sta-scan.mjs collect): the coex
// act1 work and the track work first when due, then the poll; C2Hs handled
fn collect<B: Bus + ?Sized>(
    bus: &mut B,
    chip: &mut Chip,
    sta: &mut Sta,
    env: &mut dyn StaEnv<B>,
    ch: u32,
    found: &mut ScanList,
) -> Rtw89Result<()> {
    let now = env.now_ms(bus);
    btc_act1_due(bus, chip, sta, now)?;
    track_due(bus, chip, sta, now)?; // only its traffic statistics while scanning
    for r in chip.rx_poll(bus, 64)? {
        match r {
            RxItem::C2h(c) => handle_c2h(bus, chip, sta, &c)?,
            RxItem::Frame(e) => {
                let rssi = (e.status.signal != 0 && !e.status.no_signal).then_some(e.status.signal);
                let rx_ch = e.status.ch.unwrap_or(ch);
                if let Some(b) = bss_from_frame(e.mpdu(), rx_ch, rssi, sta.vif.band) {
                    found.add(b);
                }
            }
            RxItem::Ppdu(_) => {}
        }
    }
    Ok(())
}
