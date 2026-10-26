//! The driver side of mac80211 for one managed station on the RTL8852B (JS js/sta.mjs and its
//! sta-*.mjs, WZ = /home/georg/BitsOS/werkzeuge/plattform/wlan): rtw89 mac80211.c's ops and the
//! core.c rtw89_core_* functions they call, the CAM, the station H2Cs, rate adaptation, the
//! software scan, the join and the link's RX path, the 2-second track work. The 802.11 logic
//! that does not depend on the chip (the join's state machine, the WPA2 handshakes, the RSN
//! element, the receive checks, the supervisor) is the `wifi` crate's; this module drives it.
//!
//! [`Sta`] is the station's vif (struct rtw89_vif_link fields) plus the AP's link
//! (rtw89_sta_link), the keys, and the rtwdev-wide state sta owns (MAC ID and port maps, the CAM,
//! the packet offload map, total_sta_assoc), which survives remove/add_interface. Every chip
//! access goes through [`io::Chip`] in Linux's order (the golden trace checks it).

pub mod cam;
pub mod h2c;
pub mod io;
pub mod link;
pub mod ops;
pub mod ra;
pub mod scan;
pub mod supervise;
pub mod track;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_hostile;

use alloc::collections::VecDeque;
use alloc::vec;
use alloc::vec::Vec;
use core::fmt;

use crate::btc::{self, RoleReq, RoleState};
use crate::bus::Bus;
use crate::error::{Rtw89Error, Rtw89Result};
use crate::events::EventLog;
use crate::macinit;
use crate::phy::{Band, Chan};
use crate::tables::regs::*;
use crate::txrx::ieee::Hdr;
use crate::txrx::ieee::cipher;
use crate::txrx::{CcmpKey, RxEntry, TxKey, TxOpts};

use wifi::replay::{DataDrop, DupCheck, RxPn};
use wifi::{Mlme, WifiEvent};

use cam::{AddrCam, BssidCam, CamInfo};
use io::{Chip, RxItem};
use ra::{AC_TO_FW_IDX, EdccaBak, Ewma, RaInfo, conf_tx_edca_val};
use track::{Track, Traffic};

/// mac80211's FIF_BCN_PRBRESP_PROMISC (include/net/mac80211.h): set while a software scan runs
pub const FIF_BCN_PRBRESP_PROMISC: u32 = 1 << 4;

/// What the station needs besides the chip: the clock, sleeping between RX polls (in the
/// kernel the driver thread sleeps, B4: never a busy wait), randomness for the SNonce, and, for
/// host tests on a fake chip, what the driver set (V1's fake op records FAKE_CHAN/FAKE_SCAN)
pub trait StaEnv<B: Bus + ?Sized> {
    /// ms of a monotonic clock (the bus's by default)
    fn now_ms(&mut self, bus: &mut B) -> u64 {
        bus.now_ms()
    }
    /// The thread waits `ms` (JS `sleep`): no chip access
    fn sleep_ms(&mut self, bus: &mut B, ms: u32);
    /// Random bytes (RDRAND in the kernel); false: none
    fn random(&mut self, out: &mut [u8]) -> bool;
    /// The channel was set (a fake chip's AP listens there)
    fn channel_set(&mut self, _bus: &mut B, _band: Band, _ch: u32) {}
    /// rtwdev->scanning changed
    fn scanning(&mut self, _bus: &mut B, _on: bool) {}
}

/// A wait's clock (B4: every wait ends): the environment's, but never behind the sleeps the wait
/// made, so a clock that does not move cannot make it endless (as bus::poll_loop counts its steps)
pub struct WaitClock {
    start: u64,
    slept: u64,
}

impl WaitClock {
    pub fn new<B: Bus + ?Sized>(bus: &mut B, env: &mut dyn StaEnv<B>) -> WaitClock {
        WaitClock {
            start: env.now_ms(bus),
            slept: 0,
        }
    }
    pub fn now<B: Bus + ?Sized>(&self, bus: &mut B, env: &mut dyn StaEnv<B>) -> u64 {
        env.now_ms(bus).max(self.start + self.slept)
    }
    pub fn sleep<B: Bus + ?Sized>(&mut self, bus: &mut B, env: &mut dyn StaEnv<B>, ms: u32) {
        env.sleep_ms(bus, ms);
        self.slept += u64::from(ms.max(1));
    }
}

/// IEEE80211_AC_* EDCA parameters (struct ieee80211_tx_queue_params' part rtw89 reads)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EdcaParams {
    pub aifs: u32,
    pub cw_min: u32,
    pub cw_max: u32,
    /// in 32 us units
    pub txop: u32,
}

// Linux: none (mac80211 ieee80211_set_wmm_default (linux-6.6/net/mac80211/util.c:1699),
// enable_qos false: a non-QoS link):
// every AC the 802.11 DCF values aCWmin 15 (31 for 11b only), aCWmax 1023, AIFSN 2, no TXOP
pub fn wmm_default(use_11b: bool) -> [EdcaParams; 4] {
    let p = EdcaParams {
        aifs: 2,
        cw_min: if use_11b { 31 } else { 15 },
        cw_max: 1023,
        txop: 0,
    };
    [p; 4]
}

/// Where the station is (JS d.sta.state)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkState {
    Down,
    Up,
    Auth,
    Assoc,
    Handshake,
    Connected,
}

/// mac80211's station state of the AP's link
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApState {
    None,
    Auth,
    Assoc,
    Authorized,
}

/// struct rtw89_vif_link with mac80211's bss_conf, as a station uses them
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Vif {
    pub mac_id: u32,
    pub port: u32,
    pub mac_idx: u32,
    pub phy_idx: u32,
    /// the interface's address
    pub addr: [u8; 6],
    /// the source address in the address CAM (a scan may change it)
    pub mac_addr: [u8; 6],
    pub bssid: [u8; 6],
    pub aid: u16,
    pub wifi_role: u32,
    pub net_type: u32,
    pub self_role: u32,
    pub trigger: u32,
    pub wmm: u32,
    pub hit_rule: u32,
    pub bcn_hit_cond: u32,
    pub lsig_txop: u32,
    pub tgt_ind: u32,
    pub frm_tgt_ind: u32,
    pub wowlan_pattern: u32,
    pub wowlan_uc: u32,
    pub wowlan_magic: u32,
    pub beacon_int: u16,
    pub dtim_period: u8,
    pub use_short_slot: bool,
    pub operating_11g: bool,
    pub assoc: bool,
    pub band: Band,
    /// bss_conf->basic_rates (bitmap over the band's rtw89_bitrates)
    pub basic_rates: u32,
    pub addr_cam: AddrCam,
    pub bssid_cam: BssidCam,
    pub tx_params: [EdcaParams; 4],
    /// the packet offload ids of rtw89_fw_h2c_general_pkt
    pub general_pkts: Vec<u32>,
    pub def_tri_idx: u32,
    pub dyn_tb_bedge_en: bool,
    /// rtwvif->stats
    pub stats: Traffic,
}

impl Vif {
    // a fresh rtw89_vif_link with mac80211's zeroed bss_conf (sta.mjs newSta)
    fn new(addr: [u8; 6]) -> Vif {
        Vif {
            mac_id: 0,
            port: 0,
            mac_idx: 0,
            phy_idx: 0,
            addr,
            mac_addr: addr,
            bssid: [0; 6],
            aid: 0,
            wifi_role: 0,
            net_type: 0,
            self_role: 0,
            trigger: 0,
            wmm: 0,
            hit_rule: 0,
            bcn_hit_cond: 0,
            lsig_txop: 0,
            tgt_ind: 0,
            frm_tgt_ind: 0,
            wowlan_pattern: 0,
            wowlan_uc: 0,
            wowlan_magic: 0,
            beacon_int: 0,
            dtim_period: 0,
            use_short_slot: false,
            operating_11g: false,
            assoc: false,
            band: Band::B2g,
            basic_rates: 0,
            addr_cam: AddrCam::default(),
            bssid_cam: BssidCam::default(),
            tx_params: [EdcaParams::default(); 4],
            general_pkts: Vec::new(),
            def_tri_idx: 0,
            dyn_tb_bedge_en: false,
            stats: Traffic::default(),
        }
    }

    /// The port update's view (macinit's `Vif`)
    pub fn port_vif(&self) -> macinit::Vif {
        let mut v = macinit::Vif::station(self.mac_id as u8, self.port as u8, self.addr);
        v.mac_idx = self.mac_idx as u8;
        v.net_type = self.net_type;
        v.beacon_int = self.beacon_int;
        v.dtim_period = self.dtim_period;
        v
    }

    fn band_2g(&self) -> bool {
        self.band == Band::B2g
    }
}

/// The AP's rtw89_sta_link with mac80211's sta_info parts sta keeps
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApLink {
    pub mac_id: u32,
    pub addr: [u8; 6],
    pub avg_rssi: Ewma,
    pub rssi: [Ewma; 2],
    /// link_sta->supp_rates[band]
    pub supp_rates: u32,
    pub rx_nss: u32,
    pub ra: Option<RaInfo>,
    pub er_cap: bool,
    pub assoc_link: bool,
    pub state: ApState,
    /// mac80211's last sequence control per TID (ieee80211_rx_h_check_dup)
    pub dup: DupCheck,
    /// rx_hw_rate (rtw89_core_stats_sta_rx_status)
    pub rx_hw_rate: u32,
    /// prev_rssi of rtw89_phy_stat_rssi_update
    pub prev_rssi: u32,
}

/// A key in the chip: the txrx side (index, security CAM entry, TX PN) and the receive counters
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaKey {
    pub key_idx: u8,
    pub pairwise: bool,
    /// the security CAM entry (key->hw_key_idx)
    pub hw_key_idx: u32,
    pub ccmp: CcmpKey,
    pub rx: RxPn,
}

impl StaKey {
    pub fn tx_key(&self) -> TxKey {
        TxKey {
            key_idx: self.key_idx,
            sec_cam_idx: Some(self.hw_key_idx),
            cipher: cipher::CCMP,
        }
    }
}

/// The keys of the link (d.sta.keys): the pairwise key and the group keys by ID
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Keys {
    pub ptk: Option<StaKey>,
    pub gtk: Vec<StaKey>,
}

impl Keys {
    pub fn gtk(&self, id: u8) -> Option<&StaKey> {
        self.gtk.iter().find(|k| k.key_idx == id)
    }
}

impl wifi::replay::Keys for Keys {
    fn rx_pn(&mut self, key_id: u8) -> Option<&mut RxPn> {
        if key_id == 0 {
            return self.ptk.as_mut().map(|k| &mut k.rx);
        }
        self.gtk
            .iter_mut()
            .find(|k| k.key_idx == key_id)
            .map(|k| &mut k.rx)
    }
    fn any(&self) -> bool {
        self.ptk.is_some() || !self.gtk.is_empty()
    }
}

/// The rtwdev-wide state sta keeps: it survives remove/add_interface (sta.mjs DEV_FIELDS)
#[derive(Debug, PartialEq, Eq)]
pub struct DevSta {
    /// mac_id_map (RTW89_MAX_MAC_ID_NUM, the chip's)
    pub mac_id_map: Vec<bool>,
    /// hw_port (RTW89_PORT_NUM)
    pub port_map: Vec<bool>,
    pub cam: CamInfo,
    /// pkt_offload (RTW89_MAX_PKT_OFLD_NUM, the firmware's)
    pub pkt_offload_map: Vec<bool>,
    pub total_sta_assoc: u32,
    /// the coex act1 work is due at this ms (`None`: not queued)
    pub act1_at: Option<u64>,
}

impl Default for DevSta {
    fn default() -> DevSta {
        DevSta {
            mac_id_map: vec![false; RTW89_MAX_MAC_ID_NUM as usize],
            port_map: vec![false; RTW89_PORT_NUM as usize],
            cam: CamInfo::default(),
            pkt_offload_map: vec![false; RTW89_MAX_PKT_OFLD_NUM as usize],
            total_sta_assoc: 0,
            act1_at: None,
        }
    }
}

fn acquire(map: &mut [bool], what: &'static str) -> Rtw89Result<u32> {
    let i = map.iter().position(|&b| !b).ok_or(Rtw89Error::Invalid {
        what,
        value: map.len() as u32,
    })?;
    map[i] = true;
    Ok(i as u32)
}

fn release(map: &mut [bool], i: u32) {
    if let Some(b) = map.get_mut(i as usize) {
        *b = false;
    }
}

/// A frame of an RX poll that passed rx stats and the duplicate check (sta-link.mjs rxFrames)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RxFrame {
    pub entry: RxEntry,
    pub hdr: Hdr,
}

impl RxFrame {
    /// the frame without FCS
    pub fn mpdu(&self) -> &[u8] {
        self.entry.mpdu()
    }
}

/// What the station tells the log (B6), besides the station layer's own events
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StaEvent {
    /// the station layer's (join, keys, link)
    Wifi(WifiEvent),
    /// the firmware's beacon filter reported no beacons of our AP
    BeaconLoss,
    /// a data frame of our AP that did not go up
    DataDropped { why: DataDrop },
    /// rtw89_core_rfkill_poll: the hardware switch changed
    Rfkill { blocked: bool },
    /// a scan ended: how many BSS
    ScanDone { found: u32 },
    /// a scan ended early (a channel failed)
    ScanAborted,
    /// rtw89_mac_flush_txq / __pci_flush_txch: a queue did not empty in time (Linux logs it)
    FlushTimeout { what: &'static str },
    /// a packet offload the firmware did not answer
    PktOffloadFailed { id: u32 },
    /// an EAPOL frame came without a handshake context
    EapolWithoutContext,
    /// a chip access of the link failed (`during`: what ran); the link ends locally and the
    /// supervisor joins again by its rhythm
    ChipError {
        during: &'static str,
        err: crate::error::Rtw89Error,
    },
}

impl StaEvent {
    /// The B6 event name (`thing.what`, subsystem "wlan") for klog
    pub fn name(&self) -> &'static str {
        match self {
            StaEvent::Wifi(e) => e.name(),
            StaEvent::BeaconLoss => "link.beacon_loss",
            StaEvent::DataDropped { .. } => "rx.dropped",
            StaEvent::Rfkill { .. } => "rfkill.changed",
            StaEvent::ScanDone { .. } => "scan.done",
            StaEvent::ScanAborted => "scan.aborted",
            StaEvent::FlushTimeout { .. } => "flush.timeout",
            StaEvent::PktOffloadFailed { .. } => "fw.pkt_offload_failed",
            StaEvent::EapolWithoutContext => "eapol.no_context",
            StaEvent::ChipError { .. } => "chip.error",
        }
    }
}

impl fmt::Display for StaEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StaEvent::Wifi(e) => write!(f, "{e}"),
            StaEvent::BeaconLoss => f.write_str("no beacons from the AP (firmware beacon filter)"),
            StaEvent::DataDropped { why } => write!(f, "data frame dropped: {}", why.as_str()),
            StaEvent::Rfkill { blocked } => write!(
                f,
                "rfkill hardware state changed to {}",
                if *blocked { "disable" } else { "enable" }
            ),
            StaEvent::ScanDone { found } => write!(f, "scan done: {found} BSS"),
            StaEvent::ScanAborted => f.write_str("scan aborted"),
            StaEvent::FlushTimeout { what } => write!(f, "flush timed out: {what}"),
            StaEvent::PktOffloadFailed { id } => {
                write!(f, "packet offload {id}: no answer of the firmware")
            }
            StaEvent::EapolWithoutContext => f.write_str("EAPOL frame without a handshake context"),
            StaEvent::ChipError { during, err } => write!(f, "{during}: chip error {err:?}"),
        }
    }
}

/// The station (JS d.sta)
pub struct Sta {
    pub dev: DevSta,
    pub vif: Vif,
    pub ap: Option<ApLink>,
    pub keys: Keys,
    pub state: LinkState,
    /// rtwdev->scanning
    pub scanning: bool,
    /// the channel set last (rtw89_chan of the channel context)
    pub chan: Option<Chan>,
    /// RX items an H2C wait took but did not use
    pub rx_backlog: VecDeque<RxItem>,
    /// frames an MLME exchange left (after its answer) for the next poll
    pub pending: VecDeque<RxFrame>,
    /// Ethernet frames that came while joining, for the next poll
    pub rx_eth: VecDeque<Vec<u8>>,
    /// the firmware reported a beacon loss (cleared by the caller)
    pub beacon_loss: bool,
    /// the reason of the AP's last deauthentication or disassociation
    pub last_reason: u16,
    pub edcca_bak: EdccaBak,
    /// the join and the link (wifi's state machine with the handshake context)
    pub mlme: Option<Mlme>,
    pub track: Track,
    events: EventLog<StaEvent>,
}

impl fmt::Debug for Sta {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sta")
            .field("state", &self.state)
            .field("mac_id", &self.vif.mac_id)
            .field("bssid", &self.vif.bssid)
            .field("aid", &self.vif.aid)
            .field("scanning", &self.scanning)
            .field("chan", &self.chan)
            .field("ptk", &self.keys.ptk.is_some())
            .field("gtks", &self.keys.gtk.len())
            .finish()
    }
}

impl Default for Sta {
    fn default() -> Sta {
        Sta::new()
    }
}

impl Sta {
    pub fn new() -> Sta {
        Sta {
            dev: DevSta::default(),
            vif: Vif::new([0; 6]),
            ap: None,
            keys: Keys::default(),
            state: LinkState::Down,
            scanning: false,
            chan: None,
            rx_backlog: VecDeque::new(),
            pending: VecDeque::new(),
            rx_eth: VecDeque::new(),
            beacon_loss: false,
            last_reason: 0,
            edcca_bak: EdccaBak::default(),
            mlme: None,
            track: Track::default(),
            events: EventLog::new(),
        }
    }

    pub fn take_events(&mut self) -> Vec<StaEvent> {
        self.drain_mlme_events();
        self.events.take()
    }
    pub fn events(&mut self) -> &[StaEvent] {
        self.drain_mlme_events();
        self.events.as_slice()
    }
    pub(crate) fn push(&mut self, e: StaEvent) {
        self.events.push(e);
    }
    fn drain_mlme_events(&mut self) {
        if let Some(m) = self.mlme.as_mut() {
            for e in m.take_events() {
                self.events.push(StaEvent::Wifi(e));
            }
        }
    }

    /// What frames of the station need of mac80211's state (sta-link.mjs txOpts)
    pub fn tx_opts(&self, key: Option<&StaKey>) -> TxOpts {
        TxOpts {
            mac_id: Some(self.vif.mac_id),
            sta: self.ap.is_some(),
            basic_rates: self.vif.basic_rates,
            supp_rates: self.ap.as_ref().map_or(0, |a| a.supp_rates),
            band: Some(self.vif.band),
            key: key.map(StaKey::tx_key),
            ..TxOpts::default()
        }
    }

    /// The channel set last as rtw89 keeps it (2.4 GHz channel 1 before any)
    pub fn cur_chan(&self) -> Chan {
        self.chan.unwrap_or(crate::phy::DEFAULT_CHAN)
    }

    /// The coexistence's role request for this vif on the current channel
    fn role_req(&self) -> RoleReq {
        let c = self.cur_chan();
        RoleReq {
            port: self.vif.port,
            mac_id: self.vif.mac_id,
            mode: 0,
            band: c.band,
            ch: c.ch,
            bw: RTW89_CHANNEL_WIDTH_20,
        }
    }
}

/// rtwdev->coex_act1_work: RTW89_COEX_ACT1_WORK_PERIOD (coex.h:72), one second
const ACT1_MS: u64 = 1000;

// Linux: rtw89_btc_ntfy_role_info (coex.c:9229); CONN_START and DIS_CONN (re)queue the act1 work
// (coex.c:9188, 9201)
pub fn btc_role<B: Bus + ?Sized>(
    bus: &mut B,
    chip: &mut Chip,
    sta: &mut Sta,
    now: u64,
    state: RoleState,
) -> Rtw89Result<()> {
    if matches!(state, RoleState::ConnStart | RoleState::Disconnect) {
        sta.dev.act1_at = Some(now + ACT1_MS);
    }
    let req = sta.role_req();
    chip.btc_role(bus, state, &req)
}

// Linux: rtw89_btc_ntfy_specific_packet (coex.c:8859); DHCP and EAPOL (re)queue the act1 work,
// EAPOL_END cancels it
pub fn btc_packet<B: Bus + ?Sized>(
    bus: &mut B,
    chip: &mut Chip,
    sta: &mut Sta,
    now: u64,
    typ: btc::Packet,
) -> Rtw89Result<()> {
    match typ {
        btc::Packet::Dhcp | btc::Packet::Eapol => sta.dev.act1_at = Some(now + ACT1_MS),
        btc::Packet::EapolEnd => sta.dev.act1_at = None,
        _ => {}
    }
    chip.btc_packet(bus, typ)
}

// Linux: rtw89_coex_act1_work (coex.c:7716) once its second is over. Linux runs it from the
// workqueue; BitsOS at the station's next RX poll outside an op (rx_frames, the scan loop).
pub fn btc_act1_due<B: Bus + ?Sized>(
    bus: &mut B,
    chip: &mut Chip,
    sta: &mut Sta,
    now: u64,
) -> Rtw89Result<()> {
    match sta.dev.act1_at {
        Some(t) if now >= t => {
            sta.dev.act1_at = None;
            chip.btc_act1_work(bus)
        }
        _ => Ok(()),
    }
}

// Linux: rtw89_vif_type_mapping (core.c:5676), NL80211_IFTYPE_STATION without P2P; no HE:
// trigger 0
pub fn vif_type_mapping(vif: &mut Vif, assoc: bool) {
    vif.wifi_role = RTW89_WIFI_ROLE_STATION;
    vif.net_type = if assoc {
        RTW89_NET_TYPE_INFRA
    } else {
        RTW89_NET_TYPE_NO_LINK
    };
    vif.trigger = 0;
    vif.self_role = RTW89_SELF_ROLE_CLIENT;
    vif.addr_cam.sec_ent_mode = RTW89_ADDR_CAM_SEC_NORMAL;
}

// Linux: rtw89_ops_add_interface (mac80211.c:169) with __rtw89_ops_add_iface_link
// (mac80211.c:107), then the rest of mac80211's ieee80211_do_open
// (linux-6.6/net/mac80211/iface.c:1226) for the first interface: the ERP reset (link_info_changed
// ERP_SLOT -> rtw89_conf_tx with the zeroed tx_params), ieee80211_set_wmm_default
// (linux-6.6/net/mac80211/iface.c:1419: enable_qos false for a station), hw_config(~0) ->
// rtw89_ops_config with CHANGE_CHANNEL
// (rtw89_get_default_chandef: 2.4 GHz channel 1) and CHANGE_MONITOR, and the RX filter of the
// netdev's rx-mode work.
pub fn add_interface<B: Bus + ?Sized>(
    bus: &mut B,
    chip: &mut Chip,
    sta: &mut Sta,
    env: &mut dyn StaEnv<B>,
    addr: [u8; 6],
) -> Rtw89Result<()> {
    if sta.state != LinkState::Down {
        return Err(Rtw89Error::Invalid {
            what: "add_interface: the interface is up",
            value: 0,
        });
    }
    if addr[0] & 1 != 0 {
        return Err(Rtw89Error::Invalid {
            what: "add_interface: a group address",
            value: u32::from(addr[0]),
        });
    }
    sta.vif = Vif::new(addr);
    sta.ap = None;
    sta.keys = Keys::default();
    sta.rx_backlog.clear();
    sta.pending.clear();
    sta.rx_eth.clear();
    sta.mlme = None;
    // 8852B fw 0.29.29.18 has BEACON_FILTER: vif driver flags only, no register
    sta.vif.mac_id = acquire(&mut sta.dev.mac_id_map, "no free MAC ID")?;
    sta.vif.port = match acquire(&mut sta.dev.port_map, "no free port") {
        Ok(p) => p,
        Err(e) => {
            release(&mut sta.dev.mac_id_map, sta.vif.mac_id);
            return Err(e);
        }
    };
    vif_type_mapping(&mut sta.vif, false);
    let port_vif = sta.vif.port_vif();
    chip.vif_init(bus, &port_vif, &mut sta.vif, &mut sta.dev.cam)?; // rtw89_mac_add_vif
    let now = env.now_ms(bus);
    btc_role(bus, chip, sta, now, RoleState::Start)?;
    // rtw89_recalc_lps: power saving is never entered on BitsOS
    sta.state = LinkState::Up;
    sta.vif.use_short_slot = false; // ieee80211_reset_erp_info
    link_info_changed(bus, chip, sta, None, true)?;
    for (ac, p) in wmm_default(false).iter().enumerate() {
        conf_tx(bus, chip, sta, ac, *p)?; // no channel context yet: not 11b
    }
    // rtw89_ops_config(~0): CHANGE_IDLE would enter IPS (never on BitsOS)
    config_channel(bus, chip, sta, env, Band::B2g, 1)?;
    crate::phy::dm::physts_parsing_init(bus)?; // CHANGE_MONITOR
    configure_filter(bus, chip, sta, 0)?;
    Ok(())
}

// Linux: rtw89_ops_remove_interface (mac80211.c:243) with rtw89_mac_vif_deinit (mac.c:5087);
// cfg80211 leaves the BSS first (cfg80211_leave -> deauthentication, reason 3)
pub fn remove_interface<B: Bus + ?Sized>(
    bus: &mut B,
    chip: &mut Chip,
    sta: &mut Sta,
    env: &mut dyn StaEnv<B>,
) -> Rtw89Result<()> {
    if sta.state == LinkState::Down {
        return Ok(());
    }
    if sta.ap.is_some() {
        link::disconnect(bus, chip, sta, env, wifi::mlme::reason::DEAUTH_LEAVING)?;
    }
    let now = env.now_ms(bus);
    btc_role(bus, chip, sta, now, RoleState::Stop)?;
    chip.h2c(
        bus,
        &h2c::h2c_role_maintain(&sta.vif, None, RTW89_ROLE_REMOVE),
    )?;
    cam::cam_deinit(&mut sta.dev.cam, &mut sta.vif);
    chip.h2c(bus, &h2c::h2c_addr_cam(&sta.vif, None, None))?;
    release(&mut sta.dev.port_map, sta.vif.port);
    release(&mut sta.dev.mac_id_map, sta.vif.mac_id);
    sta.state = LinkState::Down;
    Ok(())
}

// Linux: rtw89_ops_configure_filter (mac80211.c:308), macinit's rx_filter; `new_flags` from
// mac80211's ieee80211_configure_filter: FIF_BCN_PRBRESP_PROMISC during a scan, nothing else for
// a station
pub fn configure_filter<B: Bus + ?Sized>(
    bus: &mut B,
    chip: &mut Chip,
    sta: &Sta,
    new_flags: u32,
) -> Rtw89Result<()> {
    chip.rx_filter(bus, new_flags & FIF_BCN_PRBRESP_PROMISC != 0, sta.scanning)?;
    Ok(())
}

// Linux: rtw89_ops_conf_tx (mac80211.c:912) with __rtw89_conf_tx (mac80211.c:472); no MU EDCA
pub fn conf_tx<B: Bus + ?Sized>(
    bus: &mut B,
    chip: &mut Chip,
    sta: &mut Sta,
    ac: usize,
    p: EdcaParams,
) -> Rtw89Result<()> {
    let Some(slot) = sta.vif.tx_params.get_mut(ac) else {
        return Err(Rtw89Error::Invalid {
            what: "conf_tx: no such AC",
            value: ac as u32,
        });
    };
    *slot = p;
    let v = conf_tx_edca_val(sta.vif.band_2g(), sta.vif.use_short_slot, &p);
    chip.h2c(bus, &h2c::h2c_set_edca(&sta.vif, AC_TO_FW_IDX[ac], v))
}

// Linux: rtw89_ops_config (mac80211.c:75), IEEE80211_CONF_CHANGE_CHANNEL:
// rtw89_config_entity_chandef + rtw89_set_channel; 20 MHz, primary = centre
pub fn config_channel<B: Bus + ?Sized>(
    bus: &mut B,
    chip: &mut Chip,
    sta: &mut Sta,
    env: &mut dyn StaEnv<B>,
    band: Band,
    ch: u32,
) -> Rtw89Result<()> {
    sta.vif.band = band;
    chip.set_channel(bus, band, ch)?;
    sta.chan = Some(Chan { band, ch, bw: 20 });
    env.channel_set(bus, band, ch);
    Ok(())
}

// Linux: rtw89_ops_link_info_changed (mac80211.c:750), BSS_CHANGED_BSSID and BSS_CHANGED_ERP_SLOT
pub fn link_info_changed<B: Bus + ?Sized>(
    bus: &mut B,
    chip: &mut Chip,
    sta: &mut Sta,
    bssid: Option<[u8; 6]>,
    erp_slot: bool,
) -> Rtw89Result<()> {
    if let Some(b) = bssid {
        sta.vif.bssid = b;
        cam::cam_bssid_changed(&mut sta.vif);
        chip.h2c(bus, &h2c::h2c_addr_cam(&sta.vif, None, None))?;
    }
    if erp_slot {
        for (ac, fw_ac) in AC_TO_FW_IDX.iter().enumerate() {
            let p = sta.vif.tx_params[ac];
            let v = conf_tx_edca_val(sta.vif.band_2g(), sta.vif.use_short_slot, &p);
            chip.h2c(bus, &h2c::h2c_set_edca(&sta.vif, *fw_ac, v))?; // rtw89_conf_tx
        }
    }
    Ok(())
}

// Linux: rtw89_ops_vif_cfg_changed (mac80211.c:699), BSS_CHANGED_ASSOC
pub fn vif_cfg_changed<B: Bus + ?Sized>(
    bus: &mut B,
    chip: &mut Chip,
    sta: &mut Sta,
    env: &mut dyn StaEnv<B>,
    assoc: bool,
) -> Rtw89Result<()> {
    sta.vif.assoc = assoc;
    if !assoc {
        return Ok(()); // no hardware scan to abort
    }
    ops::sta_assoc(bus, chip, sta, env)?; // rtw89_station_mode_sta_assoc
    // __rtw89_ops_bss_link_assoc: bss colour, UL TB offset and OBSS narrow RU need HE
    let pv = sta.vif.port_vif();
    chip.port_update(bus, &pv)
    // rtw89_queue_chanctx_work: one channel context, nothing to switch
}

// Linux: rtw89_ops_set_key (mac80211.c:987) SET_KEY with rtw89_cam_sec_key_add (cam.c:465), CCMP;
// 8852B: hw_mgmt_tx_encrypt false (IEEE80211_KEY_FLAG_SW_MGMT_TX), hw_sec_hdr false
// (_GENERATE_IV). `pairwise`: the AP's link's key. Returns the security CAM entry.
pub fn set_key<B: Bus + ?Sized>(
    bus: &mut B,
    chip: &mut Chip,
    sta: &mut Sta,
    now: u64,
    key: &[u8],
    key_idx: u8,
    pairwise: bool,
) -> Rtw89Result<u32> {
    btc_packet(bus, chip, sta, now, btc::Packet::EapolEnd)?;
    let idx = cam::new_sec_entry(
        &mut sta.dev.cam,
        key,
        u32::from(key_idx),
        pairwise,
        RTW89_SEC_KEY_TYPE_CCMP128,
    )?; // rtw89_cam_sec_key_install
    let cmd = match sta.dev.cam.sec_entry(idx) {
        Some(sc) => cam::sec_key_cmd(sc),
        None => {
            return Err(Rtw89Error::Invalid {
                what: "security CAM entry vanished",
                value: idx,
            });
        }
    };
    let mut m = h2c::h2c_sec_cam(cmd);
    let sent = chip.h2c(bus, &m); // rtw89_cam_send_sec_key_cmd
    wifi::secret::wipe(&mut m.payload); // the key's copy (R8i)
    sent?;
    cam::attach_sec_cam(&mut sta.vif, &sta.dev.cam, idx)?; // __rtw89_cam_attach_sec_cam
    // rtw89_chip_h2c_dctl_sec_cam: NULL for the 8852B
    let ap = if pairwise { sta.ap.as_ref() } else { None };
    chip.h2c(bus, &h2c::h2c_addr_cam(&sta.vif, ap, None))?;
    Ok(idx)
}

// Linux: rtw89_ops_set_key (mac80211.c:987) DISABLE_KEY with rtw89_cam_sec_key_del (cam.c:537),
// inform_fw true
pub fn del_key<B: Bus + ?Sized>(
    bus: &mut B,
    chip: &mut Chip,
    sta: &mut Sta,
    env: &mut dyn StaEnv<B>,
    hw_key_idx: u32,
    pairwise: bool,
) -> Rtw89Result<()> {
    ops::hci_flush_queues(bus, chip, sta)?; // rtw89_hci_flush_queues (all queues, no drop)
    ops::mac_flush_txq(bus, sta)?; // rtw89_mac_flush_txq
    let _ = env;
    if sta.dev.cam.sec_entry(hw_key_idx).is_none() {
        return Err(Rtw89Error::Invalid {
            what: "key not in the security CAM",
            value: hw_key_idx,
        });
    }
    cam::detach_sec_cam(&mut sta.vif, hw_key_idx); // __rtw89_cam_detach_sec_cam
    let ap = if pairwise { sta.ap.as_ref() } else { None };
    chip.h2c(bus, &h2c::h2c_addr_cam(&sta.vif, ap, None))?;
    cam::free_sec_entry(&mut sta.dev.cam, hw_key_idx);
    Ok(())
}
