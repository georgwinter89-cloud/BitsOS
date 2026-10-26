//! Joining one AP and staying there (JS js/sta-link.mjs), in the order mac80211's mlme.c (and
//! wpa_supplicant for the keys) drives rtw89's ops: channel, BSSID, station entry, open
//! authentication, association with our RSN element, the EAPOL handshakes, keys into the
//! security CAM, then data frames; `disconnect` undoes it. The decisions of the join (answers,
//! retries, timeouts, the handshakes, the KRACK guard) are the `wifi` crate's state machine;
//! this module does what its actions ask, on the chip, in rtw89's order.

use alloc::vec::Vec;
use core::fmt;

use crate::btc;
use crate::bus::Bus;
use crate::error::{Rtw89Error, Rtw89Result};
use crate::phy::Band;
use crate::tables::regs::*;
use crate::txrx::ieee::Hdr;
use crate::txrx::ieee::{self, AssocReq, T_CTL, T_DATA, T_MGMT};
use crate::txrx::parse::{self, MgmtKind};
use crate::txrx::{BtcPkt, CcmpKey, DataTx, TxOpts, TxSent, eth_to_wlan, wlan_to_eth};

use wifi::Mlme;
use wifi::eapol::{GtkInstall, Random, Supplicant};
use wifi::mlme::{Action, JoinFailure, JoinTiming, Mgmt, reason};
use wifi::replay::{DataInfo, DupInfo, RxPn, data_allowed};
use wifi::rsn::{self, Unusable};
use wifi::scan::Bss;
use wifi::secret::Secret;

use super::WaitClock;
use super::io::{Chip, RxItem};
use super::ops::{self, handle_c2h, ops_flush};
use super::ra::phy_ppdu_iter;
use super::track::{Accu, track_due, traffic_accu};
use super::{
    ApState, LinkState, RxFrame, Sta, StaEnv, StaEvent, StaKey, btc_act1_due, btc_packet, cam,
    conf_tx, config_channel, del_key, link_info_changed, set_key, vif_cfg_changed, wmm_default,
};

const ETH_P_PAE: u16 = 0x888e;
const ZERO_MAC: [u8; 6] = [0; 6];
/// WLAN_CAPABILITY_SHORT_SLOT_TIME
const CAPAB_SHORT_SLOT: u16 = 1 << 10;
/// mac80211 hw.conf.listen_interval: rtw89 never sets max_listen_interval, mac80211's default
const LISTEN_INTERVAL: u16 = 5;
/// The pause between two RX polls of a wait (sta-link.mjs `sleep(5)`)
const POLL_PAUSE_MS: u32 = 5;

/// Why a join or a send did not go through
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkError {
    /// the chip or a module refused (register and value inside)
    Chip(Rtw89Error),
    /// BitsOS does not join this BSS (no WPA2-PSK, TKIP group, PMF required, ...)
    Unusable(Unusable),
    /// the join failed (refused, timed out, ended by the AP)
    Join(JoinFailure),
    /// `send` without a link
    NotConnected,
}

impl From<Rtw89Error> for LinkError {
    fn from(e: Rtw89Error) -> LinkError {
        LinkError::Chip(e)
    }
}

impl fmt::Display for LinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LinkError::Chip(e) => write!(f, "chip: {e:?}"),
            LinkError::Unusable(u) => write!(f, "cannot join this BSS: {u}"),
            LinkError::Join(j) => write!(f, "{j}"),
            LinkError::NotConnected => f.write_str("not connected"),
        }
    }
}

/// What a join gives (sta-link.mjs connect's result)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Joined {
    pub aid: u16,
    pub bssid: [u8; 6],
    pub ch: u8,
    pub group_key: bool,
}

/// The SNonce's randomness from the environment
struct EnvRandom<'e, B: Bus + ?Sized>(&'e mut dyn StaEnv<B>);
impl<B: Bus + ?Sized> Random for EnvRandom<'_, B> {
    fn fill(&mut self, out: &mut [u8]) -> bool {
        self.0.random(out)
    }
}

// Linux: rtw89_ops_tx (mac80211.c:19) / rtw89_core_txq_schedule through chip's tx_frame; the
// traffic statistics first (rtw89_core_tx_write_link, core.c:1414), the work Linux queues for
// EAPOL/DHCP/ARP/ICMP (rtw89_core_tx_btc_spec_pkt_notify) right after
pub fn tx<B: Bus + ?Sized>(
    bus: &mut B,
    chip: &mut Chip,
    sta: &mut Sta,
    env: &mut dyn StaEnv<B>,
    frame: &mut [u8],
    o: &TxOpts,
) -> Rtw89Result<TxSent> {
    traffic_accu(
        sta,
        frame,
        frame.len(),
        Accu {
            tx: true,
            dev: true,
            vif: true,
        },
    );
    let sent = chip.tx_frame(bus, frame, o)?;
    let pkt = match sent.info.btc_pkt {
        Some(BtcPkt::Eapol) => Some(btc::Packet::Eapol),
        Some(BtcPkt::Dhcp) => Some(btc::Packet::Dhcp),
        Some(BtcPkt::Arp) => Some(btc::Packet::Arp),
        Some(BtcPkt::Icmp) => Some(btc::Packet::Icmp),
        None => None,
    };
    if let Some(p) = pkt {
        let now = env.now_ms(bus);
        btc_packet(bus, chip, sta, now, p)?;
    }
    Ok(sent)
}

// get_hdr_bssid (core.h:9051)
fn hdr_bssid(h: &Hdr) -> Option<[u8; 6]> {
    if h.to_ds {
        h.addr1
    } else if h.from_ds {
        h.addr2
    } else {
        h.addr3
    }
}

// rtw89_core_rx_stats (core.c:3170): rtwdev's traffic statistics for every frame, then
// rtw89_vif_rx_stats_iter (core.c:3068) for the station: a beacon of our BSS ->
// rtw89_fw_h2c_rssi_offload (fw.c:5472) with Linux's phy_ppdu (fw has BEACON_FILTER); a frame of
// our BSS to us counts for the vif. A legacy station never gets a trigger frame for its AID
// (rtw89_stats_trigger_frame: HE only).
fn rx_stats<B: Bus + ?Sized>(
    bus: &mut B,
    chip: &mut Chip,
    sta: &mut Sta,
    f: &RxFrame,
) -> Rtw89Result<()> {
    let len = f.entry.frame.len(); // skb->len with the FCS
    let mpdu = f.mpdu();
    traffic_accu(
        sta,
        mpdu,
        len,
        Accu {
            tx: false,
            dev: true,
            vif: false,
        },
    );
    let h = &f.hdr;
    if hdr_bssid(h) != Some(sta.vif.bssid) {
        return Ok(());
    }
    if h.typ == T_MGMT
        && h.stype == ieee::st::BEACON
        && chip.beacon_filter
        && let Some(p) = &f.entry.ppdu
    {
        chip.h2c(
            bus,
            &super::h2c::h2c_rssi_offload(p.mac_id, p.rssi_avg.into()),
        )?;
    }
    if h.addr1 == Some(sta.vif.addr) {
        traffic_accu(
            sta,
            mpdu,
            len,
            Accu {
                tx: false,
                dev: false,
                vif: true,
            },
        );
    }
    Ok(())
}

// rtw89_core_stats_sta_rx_status (core.c:4342): the AP's last RX rate (rx_hw_rate) from a data
// frame's long RX descriptor that matched our address
fn sta_rx_status(sta: &mut Sta, f: &RxFrame) {
    let d = &f.entry.desc;
    let Some(ap) = sta.ap.as_mut() else { return };
    if !d.addr1_match || !d.long_rxdesc || d.frame_type != RTW89_RX_TYPE_DATA {
        return;
    }
    if d.mac_id == ap.mac_id {
        ap.rx_hw_rate = d.data_rate;
    }
}

// mac80211 ieee80211_rx_h_check_dup (linux-6.6/net/mac80211/rx.c:1411, wifi's DupCheck): a
// unicast retransmission of our AP; seqno_idx as ieee80211_parse_qos
// (linux-6.6/net/mac80211/rx.c:875) gives it
pub(super) fn duplicate(sta: &mut Sta, f: &RxFrame) -> bool {
    let Some(ap) = sta.ap.as_mut() else {
        return false;
    };
    let mpdu = f.entry.mpdu();
    let h = &f.hdr;
    if mpdu.len() < 24 || h.addr2 != Some(ap.addr) {
        return false;
    }
    let info = DupInfo {
        control: h.typ == T_CTL,
        group: h.addr1.is_some_and(|a| a[0] & 1 != 0),
        nullfunc: h.typ == T_DATA && ieee::is_any_nullfunc(h.fc),
        retry: h.retry,
        tid: h.qos.and(h.tid).map(|t| t as u8),
        seq_ctrl: u16::from_le_bytes([mpdu[22], mpdu[23]]),
    };
    ap.dup.duplicate(&info)
}

// Linux: none (sta-link.mjs rxFrames): one RX poll (plus what H2C waits left): C2Hs handled, the
// AP's RSSI averaged where rtw89_core_rx_process_phy_sts iterates the stations, rx stats, then
// mac80211's ieee80211_rx_h_check_dup (linux-6.6/net/mac80211/rx.c:1411). Frames an exchange did
// not take come first. The coex act1 work and the track work run first when they are due.
pub fn rx_frames<B: Bus + ?Sized>(
    bus: &mut B,
    chip: &mut Chip,
    sta: &mut Sta,
    env: &mut dyn StaEnv<B>,
) -> Rtw89Result<Vec<RxFrame>> {
    let now = env.now_ms(bus);
    btc_act1_due(bus, chip, sta, now)?;
    track_due(bus, chip, sta, now)?;
    let mut list: Vec<RxItem> = sta.rx_backlog.drain(..).collect();
    list.extend(chip.rx_poll(bus, 64)?);
    let mut out: Vec<RxFrame> = sta.pending.drain(..).collect();
    for r in list {
        let e = match r {
            RxItem::C2h(c) => {
                handle_c2h(bus, chip, sta, &c)?;
                continue;
            }
            RxItem::Ppdu(p) => {
                phy_ppdu_iter(sta.ap.as_mut(), &p);
                continue;
            }
            RxItem::Frame(e) => *e,
        };
        let Some(hdr) = ieee::hdr_parse(e.mpdu()) else {
            continue;
        };
        if hdr.addr2.is_none() || hdr.short {
            continue;
        }
        let f = RxFrame { entry: e, hdr };
        sta_rx_status(sta, &f);
        rx_stats(bus, chip, sta, &f)?;
        if duplicate(sta, &f) {
            continue;
        }
        out.push(f);
    }
    Ok(out)
}

// mac80211 ieee80211_key_replace: a key of the same index is disabled first, then the new one
// enabled. wifi's supplicant never hands the PTK in force out again (KRACK guard).
fn install_ptk<B: Bus + ?Sized>(
    bus: &mut B,
    chip: &mut Chip,
    sta: &mut Sta,
    env: &mut dyn StaEnv<B>,
    tk: &Secret<16>,
) -> Rtw89Result<()> {
    if let Some(old) = sta.keys.ptk.take() {
        del_key(bus, chip, sta, env, old.hw_key_idx, true)?;
    }
    let now = env.now_ms(bus);
    let hw = set_key(bus, chip, sta, now, tk.expose(), 0, true)?;
    sta.keys.ptk = Some(StaKey {
        key_idx: 0,
        pairwise: true,
        hw_key_idx: hw,
        ccmp: CcmpKey::new(0, Some(hw)),
        rx: RxPn::new(0),
    });
    Ok(())
}

fn install_gtk<B: Bus + ?Sized>(
    bus: &mut B,
    chip: &mut Chip,
    sta: &mut Sta,
    env: &mut dyn StaEnv<B>,
    g: &GtkInstall,
) -> Rtw89Result<()> {
    if let Some(i) = sta.keys.gtk.iter().position(|k| k.key_idx == g.id) {
        let old = sta.keys.gtk.remove(i);
        del_key(bus, chip, sta, env, old.hw_key_idx, false)?;
    }
    let now = env.now_ms(bus);
    let hw = set_key(bus, chip, sta, now, g.key.expose(), g.id, false)?;
    let rsc = RxPn::rsc_of(&g.rsc);
    let mut ccmp = CcmpKey::new(g.id, Some(hw));
    ccmp.rsc = rsc;
    sta.keys.gtk.push(StaKey {
        key_idx: g.id,
        pairwise: false,
        hw_key_idx: hw,
        ccmp,
        rx: RxPn::new(rsc),
    });
    Ok(())
}

// ToDS data frame; with the PTK in, txrx writes the CCMP header with the next PN (8852B
// hw_sec_hdr false: IEEE80211_KEY_FLAG_GENERATE_IV) and the WD asks for encryption
fn tx_eth<B: Bus + ?Sized>(
    bus: &mut B,
    chip: &mut Chip,
    sta: &mut Sta,
    env: &mut dyn StaEnv<B>,
    eth: &[u8],
) -> Rtw89Result<()> {
    let bssid = sta.vif.bssid;
    let d = DataTx {
        bssid,
        key: sta.keys.ptk.as_mut().map(|k| &mut k.ccmp),
        qos: None,
        seq: 0,
    };
    let mut f = eth_to_wlan(eth, d)?;
    let o = sta.tx_opts(sta.keys.ptk.as_ref());
    tx(bus, chip, sta, env, &mut f, &o)?;
    Ok(())
}

/// What wifi's Mgmt needs of a parsed management frame of the AP
fn to_mgmt(f: &RxFrame) -> Option<Mgmt> {
    mgmt_of(f.mpdu())
}

// a management frame from the air (without FCS) as wifi's MLME takes it; `None` for a frame it
// does not take (no addresses, another subtype, too short)
pub(super) fn mgmt_of(mpdu: &[u8]) -> Option<Mgmt> {
    let m = parse::mgmt_parse(mpdu)?;
    let (a1, a2) = (m.hdr.addr1?, m.hdr.addr2?);
    Some(match m.kind {
        MgmtKind::Auth {
            alg,
            auth_seq,
            status,
        } => Mgmt::Auth {
            a1,
            a2,
            alg,
            seq: auth_seq,
            status,
        },
        MgmtKind::AssocResp {
            capability,
            status,
            aid,
            ..
        } => Mgmt::AssocResp {
            a1,
            a2,
            status,
            aid,
            capability,
        },
        MgmtKind::Deauth { reason } => Mgmt::Deauth { a1, a2, reason },
        MgmtKind::Disassoc { reason } => Mgmt::Disassoc { a1, a2, reason },
        MgmtKind::Other => return None,
    })
}

/// How a run of actions ended
enum Outcome {
    /// keep going
    Go,
    Connected(Joined),
    Failed(JoinFailure),
    /// the AP ended the link after the join
    Ended,
}

// The actions of wifi's MLME, in its order, on the chip (sta-link.mjs join, eapolFrame,
// installPtk/installGtk, teardown)
fn act<B: Bus + ?Sized>(
    bus: &mut B,
    chip: &mut Chip,
    sta: &mut Sta,
    env: &mut dyn StaEnv<B>,
    bss: Option<&Bss>,
    actions: Vec<Action>,
) -> Rtw89Result<Outcome> {
    for a in actions {
        match a {
            Action::SendAuth { .. } => {
                let mut f = ieee::auth_open(&sta.vif.addr, &sta.vif.bssid, 0);
                let o = sta.tx_opts(None);
                tx(bus, chip, sta, env, &mut f, &o)?;
            }
            Action::SendAssoc { attempt } => {
                if attempt == 1 {
                    if let Some(ap) = sta.ap.as_mut() {
                        ap.state = ApState::Auth; // NONE -> AUTH: nothing for rtw89
                    }
                    sta.state = LinkState::Assoc;
                }
                let Some(b) = bss else { continue };
                let own_ie = rsn::own_ie(b.group_cipher());
                let a = AssocReq {
                    own: sta.vif.addr,
                    bssid: sta.vif.bssid,
                    ssid: &b.ssid,
                    band: sta.vif.band,
                    ap_rate_bytes: Some(&b.rate_bytes),
                    ap_capability: b.capability,
                    rsn: Some(&own_ie),
                    listen_interval: LISTEN_INTERVAL,
                    seq: 0,
                };
                let mut f = ieee::assoc_req(&a);
                let o = sta.tx_opts(None);
                tx(bus, chip, sta, env, &mut f, &o)?;
            }
            Action::Associated { aid, capability } => {
                // ieee80211_assoc_success: link parameters (no WMM: ieee80211_set_wmm_default),
                // AUTH -> ASSOC (rtw89 defers it for a station), ieee80211_set_associated
                // (ASSOC, then ERP_SLOT)
                sta.vif.aid = aid;
                let band_2g = sta.vif.band == Band::B2g;
                for (ac, p) in wmm_default(band_2g && !sta.vif.operating_11g)
                    .iter()
                    .enumerate()
                {
                    conf_tx(bus, chip, sta, ac, *p)?;
                }
                if let Some(ap) = sta.ap.as_mut() {
                    ap.state = ApState::Assoc;
                }
                vif_cfg_changed(bus, chip, sta, env, true)?;
                let short = if band_2g {
                    capability & CAPAB_SHORT_SLOT != 0
                } else {
                    true
                };
                let changed = short != sta.vif.use_short_slot;
                sta.vif.use_short_slot = short;
                link_info_changed(bus, chip, sta, None, changed)?;
                sta.state = LinkState::Handshake;
            }
            Action::SendEapol(pdu) => {
                let mut eth = Vec::with_capacity(14 + pdu.len());
                eth.extend_from_slice(&sta.vif.bssid);
                eth.extend_from_slice(&sta.vif.addr);
                eth.extend_from_slice(&ETH_P_PAE.to_be_bytes());
                eth.extend_from_slice(&pdu);
                tx_eth(bus, chip, sta, env, &eth)?;
            }
            Action::InstallPtk(tk) => install_ptk(bus, chip, sta, env, &tk)?,
            Action::InstallGtk(g) => install_gtk(bus, chip, sta, env, &g)?,
            Action::Connected { aid, group_key } => {
                if let Some(ap) = sta.ap.as_mut() {
                    ap.state = ApState::Authorized; // ASSOC -> AUTHORIZED: nothing for rtw89
                }
                sta.state = LinkState::Connected;
                let ch = sta.chan.map_or(0, |c| c.ch as u8);
                return Ok(Outcome::Connected(Joined {
                    aid,
                    bssid: sta.vif.bssid,
                    ch,
                    group_key,
                }));
            }
            Action::Failed { why, deauth } => {
                teardown(bus, chip, sta, env, deauth.unwrap_or(0))?;
                return Ok(Outcome::Failed(why));
            }
            Action::Ended { reason, .. } => {
                sta.last_reason = reason;
                teardown(bus, chip, sta, env, 0)?;
                return Ok(Outcome::Ended);
            }
        }
    }
    Ok(Outcome::Go)
}

// Linux: none (mac80211 mlme.c ieee80211_mgd_auth/_assoc, ieee80211_assoc_success
// (linux-6.6/net/mac80211/mlme.c:5104)) driving
// rtw89_ops_config (mac80211.c:75), _link_info_changed (:750), _sta_state (:973),
// _vif_cfg_changed (:699), _conf_tx (:912), _set_key (:987). `bss`: an entry of the scan; `sup`:
// the link's handshake context (PMK, addresses, our and the AP's RSN element), closed when the
// link ends (also when the join fails). A link that is up is left first.
pub fn connect<B: Bus + ?Sized>(
    bus: &mut B,
    chip: &mut Chip,
    sta: &mut Sta,
    env: &mut dyn StaEnv<B>,
    bss: &Bss,
    mut sup: Supplicant,
    timing: JoinTiming,
) -> Result<Joined, LinkError> {
    if let Err(u) = bss.usable() {
        sup.close(); // the caller's context ends here too
        return Err(LinkError::Unusable(u));
    }
    if sta.ap.is_some() {
        disconnect(bus, chip, sta, env, reason::DEAUTH_LEAVING)?;
    }
    if let Some(m) = sta.mlme.as_mut() {
        m.close();
    }
    sta.pending.clear();
    match join(bus, chip, sta, env, bss, sup, timing) {
        Ok(Ok(j)) => Ok(j),
        Ok(Err(why)) => {
            // teardown ran with the failure
            if let Some(m) = sta.mlme.as_mut() {
                m.close();
            }
            Err(LinkError::Join(why))
        }
        Err(e) => {
            // an error of ours: undo what got done
            let _ = teardown(bus, chip, sta, env, 0);
            if let Some(m) = sta.mlme.as_mut() {
                m.close();
            }
            Err(LinkError::Chip(e))
        }
    }
}

fn join<B: Bus + ?Sized>(
    bus: &mut B,
    chip: &mut Chip,
    sta: &mut Sta,
    env: &mut dyn StaEnv<B>,
    bss: &Bss,
    sup: Supplicant,
    timing: JoinTiming,
) -> Rtw89Result<Result<Joined, JoinFailure>> {
    // ieee80211_prep_connection: the channel, ieee80211_mgd_setup_link_sta (rates, 11g mode),
    // the beacon's timing (beacon_int; dtim_period ?: 1 when ASSOC is announced)
    let band = if bss.band == wifi::Band::B5g {
        Band::B5g
    } else {
        Band::B2g
    };
    config_channel(bus, chip, sta, env, band, u32::from(bss.ch))?;
    sta.vif.beacon_int = bss.beacon_int;
    sta.vif.dtim_period = bss.dtim_period.filter(|&d| d != 0).unwrap_or(1);
    sta.vif.basic_rates = ieee::rate_bitmap(&bss.rate_bytes, band, true);
    sta.vif.operating_11g =
        band == Band::B2g && bss.rate_bytes.iter().any(|b| u32::from(b & 0x7f) * 5 > 110);
    link_info_changed(bus, chip, sta, Some(bss.bssid), false)?; // BSS_CHANGED_BSSID
    let supp = ieee::rate_bitmap(&bss.rate_bytes, band, false);
    ops::sta_add(bus, chip, sta, env, bss.bssid, supp)?; // NOTEXIST -> NONE
    sta.state = LinkState::Auth;
    let mut clock = WaitClock::new(bus, env);
    let now = clock.now(bus, env);
    let (m, first) = Mlme::start(sta.vif.addr, bss.bssid, sup, timing, now);
    sta.mlme = Some(m);
    if let Some(o) = finish(act(bus, chip, sta, env, Some(bss), first)?) {
        return Ok(o);
    }
    loop {
        let frames = rx_frames(bus, chip, sta, env)?;
        let mut acts = Vec::new();
        let joining = sta
            .mlme
            .as_ref()
            .is_some_and(|m| matches!(m.phase(), wifi::Phase::Auth | wifi::Phase::Assoc));
        let mut moved = false;
        let mut it = frames.into_iter();
        while let Some(f) = it.next() {
            if joining {
                // an exchange: the first answer it takes ends this poll's look; frames after it
                // stay for the next step (sta-link.mjs mlmeExchange)
                let Some(mg) = to_mgmt(&f) else { continue };
                let now = clock.now(bus, env);
                let a = sta
                    .mlme
                    .as_mut()
                    .map(|m| m.frame(&mg, now))
                    .unwrap_or_default();
                if !a.is_empty() {
                    sta.pending.extend(it.by_ref());
                    acts = a;
                    moved = true;
                    break;
                }
            } else {
                match rx_one(bus, sta, env, f)? {
                    RxOne::Eth(e) => sta.rx_eth.push_back(e),
                    RxOne::Actions(a) => {
                        if let Some(o) = finish(act(bus, chip, sta, env, Some(bss), a)?) {
                            return Ok(o);
                        }
                    }
                    RxOne::Nothing => {}
                }
            }
        }
        if let Some(o) = finish(act(bus, chip, sta, env, Some(bss), acts)?) {
            return Ok(o);
        }
        if moved {
            // the next exchange (or the handshake) polls at once, as the JS's next step
            continue;
        }
        let now = clock.now(bus, env);
        let a = sta
            .mlme
            .as_mut()
            .map(|m| m.poll_done(now))
            .unwrap_or_default();
        if let Some(o) = finish(act(bus, chip, sta, env, Some(bss), a)?) {
            return Ok(o);
        }
        clock.sleep(bus, env, POLL_PAUSE_MS);
    }
}

fn finish(o: Outcome) -> Option<Result<Joined, JoinFailure>> {
    match o {
        Outcome::Go => None,
        Outcome::Connected(j) => Some(Ok(j)),
        Outcome::Failed(why) => Some(Err(why)),
        // after the join only; during it the MLME says Failed
        Outcome::Ended => Some(Err(JoinFailure::EndedByAp {
            phase: wifi::Phase::Connected,
            reason: 0,
        })),
    }
}

/// What one received frame of the link gives
enum RxOne {
    Eth(Vec<u8>),
    Actions(Vec<Action>),
    Nothing,
}

// mac80211's RX for an associated link (from the handshake on, sta-link.mjs rxLink/ethOf): a
// deauthentication or disassociation of our AP to the MLME (it ends the link,
// ieee80211_rx_mgmt_deauth (linux-6.6/net/mac80211/mlme.c:3817)), data through the station's
// checks (wifi's data_allowed: decrypted, PN replay per TID as ieee80211_crypto_ccmp_decrypt
// (linux-6.6/net/mac80211/wpa.c:506), the control port of ieee80211_802_1x_port_control
// (linux-6.6/net/mac80211/rx.c:2379) and ieee80211_frame_allowed
// (linux-6.6/net/mac80211/rx.c:2550), ieee80211_drop_unencrypted
// (linux-6.6/net/mac80211/rx.c:2387)), EAPOL to the MLME's supplicant, the rest up as Ethernet
fn rx_one<B: Bus + ?Sized>(
    bus: &mut B,
    sta: &mut Sta,
    env: &mut dyn StaEnv<B>,
    f: RxFrame,
) -> Rtw89Result<RxOne> {
    let h = &f.hdr;
    if h.typ == T_MGMT {
        let Some(mg) = to_mgmt(&f) else {
            return Ok(RxOne::Nothing);
        };
        if !sta.vif.assoc {
            return Ok(RxOne::Nothing);
        }
        if let Mgmt::Deauth { reason, .. } | Mgmt::Disassoc { reason, .. } = mg {
            sta.last_reason = reason;
        }
        let now = env.now_ms(bus);
        let a = sta
            .mlme
            .as_mut()
            .map(|m| m.frame(&mg, now))
            .unwrap_or_default();
        return Ok(if a.is_empty() {
            RxOne::Nothing
        } else {
            RxOne::Actions(a)
        });
    }
    if !h.from_ds || h.to_ds || h.addr2 != Some(sta.vif.bssid) {
        return Ok(RxOne::Nothing);
    }
    let Some(w) = wlan_to_eth(f.mpdu()) else {
        return Ok(RxOne::Nothing);
    };
    let info = DataInfo {
        protected: w.protected,
        decrypted: f.entry.status.decrypted,
        key_id: w.key_id.unwrap_or(0),
        pn: w.pn.unwrap_or(0),
        sec_idx: usize::from(w.sec_idx),
        eapol_to_us: w.eapol && w.dst == sta.vif.addr,
    };
    let authorized = sta
        .ap
        .as_ref()
        .is_some_and(|a| a.state == ApState::Authorized);
    if let Err(why) = data_allowed(&info, authorized, &mut sta.keys) {
        sta.push(StaEvent::DataDropped { why });
        return Ok(RxOne::Nothing);
    }
    if w.eapol {
        if !info.eapol_to_us {
            return Ok(RxOne::Nothing); // the control port is for EAPOL to us
        }
        let Some(m) = sta.mlme.as_mut() else {
            sta.push(StaEvent::EapolWithoutContext);
            return Ok(RxOne::Nothing);
        };
        let a = m.eapol(&w.eth[14..], &mut EnvRandom(&mut *env));
        return Ok(RxOne::Actions(a));
    }
    Ok(RxOne::Eth(w.eth))
}

// Linux: ieee80211_set_disassoc (linux-6.6/net/mac80211/mlme.c:2885) for an associated link:
// with our deauthentication a dropping flush, the frame, a flush; the AP's station destroyed
// (__sta_info_destroy_part1/2: AUTHORIZED -> ASSOC, flush, its keys, ASSOC -> AUTH -> NONE ->
// NOTEXIST); then the ERP reset and one notify (vif_cfg_changed ASSOC, link_info_changed BSSID
// and ERP_SLOT -> rtw89_conf_tx), ieee80211_set_wmm_default (linux-6.6/net/mac80211/mlme.c:3023,
// enable_qos false); cfg80211 deletes the group keys after that. A link never associated ends as
// ieee80211_destroy_auth_data (linux-6.6/net/mac80211/mlme.c:3498): the station, then the BSSID.
// Every step runs even when one before it failed (mac80211's teardown has no error path either):
// the link ends locally in any case (no AP station, no keys, the handshake context wiped), and
// the first error is returned at the end. A failed deauthentication frame or H2C must never leave
// a half link that the supervisor does not rejoin.
pub fn teardown<B: Bus + ?Sized>(
    bus: &mut B,
    chip: &mut Chip,
    sta: &mut Sta,
    env: &mut dyn StaEnv<B>,
    deauth_reason: u16,
) -> Rtw89Result<()> {
    let mut first = None;
    let was_assoc = sta.vif.assoc;
    if let Some(state) = sta.ap.as_ref().map(|a| a.state) {
        if deauth_reason != 0 && state != ApState::None {
            keep(&mut first, ops_flush(bus, chip, sta, true)); // ieee80211_flush_queues(.., true)
            let mut f = ieee::deauth(&sta.vif.addr, &sta.vif.bssid, deauth_reason, 0);
            let o = sta.tx_opts(None);
            keep(&mut first, tx(bus, chip, sta, env, &mut f, &o));
            keep(&mut first, ops_flush(bus, chip, sta, false)); // the deauthentication left
        }
        let mut state = if state == ApState::Authorized {
            ApState::Assoc
        } else {
            state
        };
        keep(&mut first, ops_flush(bus, chip, sta, false)); // __sta_info_destroy_part2
        if let Some(k) = sta.keys.ptk.take() {
            keep(&mut first, del_key(bus, chip, sta, env, k.hw_key_idx, true));
            forget_key(sta, k.hw_key_idx);
        }
        if state == ApState::Assoc {
            keep(&mut first, ops::sta_disassoc(bus, chip, sta));
            state = ApState::Auth;
        }
        if state == ApState::Auth {
            keep(&mut first, ops::sta_disconnect(bus, chip, sta, env));
        }
        keep(&mut first, ops::sta_remove(bus, chip, sta, env));
        sta.ap = None; // sta_remove's, also when its coex notify failed
    }
    sta.vif.aid = 0;
    if sta.state != LinkState::Down {
        sta.state = LinkState::Up;
    }
    if was_assoc {
        sta.vif.use_short_slot = false; // ieee80211_reset_erp_info
        keep(&mut first, vif_cfg_changed(bus, chip, sta, env, false));
        keep(
            &mut first,
            link_info_changed(bus, chip, sta, Some(ZERO_MAC), true),
        );
        let band_2g = sta.vif.band == Band::B2g;
        for (ac, p) in wmm_default(band_2g && !sta.vif.operating_11g)
            .iter()
            .enumerate()
        {
            keep(&mut first, conf_tx(bus, chip, sta, ac, *p)); // the channel context is still there
        }
    } else if sta.vif.bssid != ZERO_MAC {
        keep(
            &mut first,
            link_info_changed(bus, chip, sta, Some(ZERO_MAC), false),
        );
    }
    for k in core::mem::take(&mut sta.keys.gtk) {
        keep(
            &mut first,
            del_key(bus, chip, sta, env, k.hw_key_idx, false),
        );
        forget_key(sta, k.hw_key_idx);
    }
    if let Some(m) = sta.mlme.as_mut() {
        m.close(); // the handshake context is wiped
    }
    match first {
        None => Ok(()),
        Some(e) => {
            sta.push(StaEvent::ChipError {
                during: "teardown",
                err: e.clone(),
            });
            Err(e)
        }
    }
}

// the first error of a teardown is kept, the rest of it runs
fn keep<T>(first: &mut Option<Rtw89Error>, r: Rtw89Result<T>) {
    if let Err(e) = r {
        first.get_or_insert(e);
    }
}

// A key whose DISABLE_KEY failed still leaves the driver's books (its slot and its copy, which
// the entry wipes): the chip's entry is overwritten by the next SET_KEY of that slot
fn forget_key(sta: &mut Sta, hw_key_idx: u32) {
    if sta.dev.cam.sec_entry(hw_key_idx).is_some() {
        cam::detach_sec_cam(&mut sta.vif, hw_key_idx);
        cam::free_sec_entry(&mut sta.dev.cam, hw_key_idx);
    }
}

// Linux: none (mac80211 ieee80211_mgd_deauth -> ieee80211_set_disassoc) driving rtw89_ops_flush
// (mac80211.c:1122), rtw89_ops_sta_state (mac80211.c:973) and rtw89_ops_set_key (mac80211.c:987)
// DISABLE_KEY
pub fn disconnect<B: Bus + ?Sized>(
    bus: &mut B,
    chip: &mut Chip,
    sta: &mut Sta,
    env: &mut dyn StaEnv<B>,
    reason: u16,
) -> Rtw89Result<()> {
    teardown(bus, chip, sta, env, reason)
}

// Linux: none (mac80211 ieee80211_subif_start_xmit for a station); out through rtw89_ops_tx ->
// rtw89_core_tx_write. `eth`: destination, our address, EtherType, payload.
pub fn send<B: Bus + ?Sized>(
    bus: &mut B,
    chip: &mut Chip,
    sta: &mut Sta,
    env: &mut dyn StaEnv<B>,
    eth: &[u8],
) -> Result<(), LinkError> {
    if sta.state != LinkState::Connected {
        return Err(LinkError::NotConnected);
    }
    tx_eth(bus, chip, sta, env, eth)?;
    Ok(())
}

/// What one poll of the link gave
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Polled {
    /// Ethernet frames of the BSS
    pub eth: Vec<Vec<u8>>,
    /// the AP ended the link (deauthentication or disassociation; `Sta::last_reason`)
    pub ended: bool,
}

// Linux: none (mac80211 RX for a station): the Ethernet frames of the BSS. EAPOL (group rekey)
// goes to the link's supplicant; a deauthentication or disassociation of the AP ends the link
// (no frame of ours). Frames that came while joining come first.
pub fn poll<B: Bus + ?Sized>(
    bus: &mut B,
    chip: &mut Chip,
    sta: &mut Sta,
    env: &mut dyn StaEnv<B>,
) -> Rtw89Result<Polled> {
    let mut out = Polled {
        eth: sta.rx_eth.drain(..).collect(),
        ended: false,
    };
    for f in rx_frames(bus, chip, sta, env)? {
        if out.ended {
            continue;
        }
        match rx_one(bus, sta, env, f)? {
            RxOne::Eth(e) => out.eth.push(e),
            RxOne::Actions(a) => match act(bus, chip, sta, env, None, a)? {
                Outcome::Ended | Outcome::Failed(_) => out.ended = true,
                _ => {}
            },
            RxOne::Nothing => {}
        }
    }
    Ok(out)
}
