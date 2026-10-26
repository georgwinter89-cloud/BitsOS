//! The TX side of txrx (JS js/txrx.mjs): the descriptor info of a frame, mac80211's sequence
//! numbers and the way of one frame to its ring.

use crate::bus::Bus;
use crate::error::{Rtw89Error, Rtw89Result};
use crate::pci::{PciRings, TxDescInfo};
use crate::pcitx::{PciTx, Queued};
use crate::phy::Band;
use crate::tables::regs::*;

use super::data::{CCMP_HDR_LEN, ETH_P_ARP, ETH_P_IP, ETH_P_PAE};
use super::desc::{get_ch_dma, get_qsel, get_tid_indicate, wd_info_dwords};
use super::ieee::{
    cipher, fctl, hdr_len, is_any_nullfunc, is_ctl, is_data_qos, is_mgmt, is_qos_nullfunc, le16,
};
use super::{Txrx, data::NUM_TIDS};

/// mac80211's TX counters and rtw89's TX flags (JS `d.txrx` TX part)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxState {
    /// sdata->sequence_number (in seq_ctrl units)
    pub seq_ctrl: u16,
    /// sta->tid_seq[] (one per TID, 802.11's 16)
    pub tid_seq: [u16; NUM_TIDS],
    /// rtwvif_link->last_a_ctrl
    pub last_actrl: bool,
    /// rtwdev->hci.tx_rpt_enabled: never set for PCIe
    pub tx_rpt_enabled: bool,
    /// rtwdev->tx_rpt.sn
    pub tx_rpt_sn: u32,
}

impl Default for TxState {
    fn default() -> TxState {
        TxState {
            seq_ctrl: 0,
            tid_seq: [0; NUM_TIDS],
            last_actrl: false,
            tx_rpt_enabled: false,
            tx_rpt_sn: 0,
        }
    }
}

/// What the TX info is for: the JS `kind` argument
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxKind {
    Mgmt,
    Data,
    /// rtw89_core_tx_update_h2c_info: a firmware command on CH12
    Fwcmd,
}

/// rtw89_core_get_tx_type: how the frame goes
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxType {
    Mgmt,
    Data,
}

/// The packets rtw89_core_tx_btc_spec_pkt_notify hands to the coexistence
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BtcPkt {
    Eapol,
    Arp,
    Dhcp,
    Icmp,
}

/// The key a data frame is encrypted with by the chip
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TxKey {
    pub key_idx: u8,
    /// the security CAM entry (`None`: Linux's "sec cam entry is empty")
    pub sec_cam_idx: Option<u32>,
    /// the cipher suite (only CCMP: this chip has no TKIP)
    pub cipher: u32,
}

impl From<&super::CcmpKey> for TxKey {
    fn from(k: &super::CcmpKey) -> TxKey {
        TxKey {
            key_idx: k.key_idx,
            sec_cam_idx: k.sec_cam_idx,
            cipher: cipher::CCMP,
        }
    }
}

/// What Linux takes from mac80211 and its vif/sta for one frame (JS `opts`)
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TxOpts {
    /// rtwvif_link/rtwsta_link mac_id (default: `Txrx::mac_id`, else 0)
    pub mac_id: Option<u32>,
    /// the AP's station entry exists (rtwsta_link != NULL)
    pub sta: bool,
    /// bss_conf->basic_rates, bitmap over the band's rtw89_bitrates
    pub basic_rates: u32,
    /// link_sta->supp_rates[band], the same bitmap
    pub supp_rates: u32,
    /// the band (default: the channel's)
    pub band: Option<Band>,
    /// for data frames the chip encrypts
    pub key: Option<TxKey>,
    /// skb->priority (0: no WMM, mac80211 sends everything at priority 0)
    pub tid: u32,
    /// IEEE80211_TX_CTL_NO_CCK_RATE
    pub no_cck: bool,
    /// IEEE80211_TX_CTL_REQ_TX_STATUS (only matters with TX reports, USB)
    pub req_tx_status: bool,
    pub er_cap: bool,
    pub stbc: bool,
    pub ldpc: bool,
    /// `tx_frame` keeps the frame's sequence number (JS `assignSeq: false`)
    pub keep_seq: bool,
    /// `tx_frame` queues without the kick (JS `kick: false`)
    pub no_kick: bool,
}

/// struct rtw89_tx_desc_info as rtw89_core_tx_update_desc_info fills it, with the WD info dwords
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TxInfo {
    pub pkt_size: u32,
    /// `None`: a firmware command
    pub tx_type: Option<TxType>,
    pub is_bmc: bool,
    pub wd_page: bool,
    pub ch_dma: u32,
    /// the 802.11 sequence number of the frame
    pub seq: u32,
    pub report: bool,
    pub sn: u32,
    pub tx_cnt_lmt_en: bool,
    pub tx_cnt_lmt: u32,
    pub hiq: bool,
    pub upd_wlan_hdr: bool,
    pub qsel: u32,
    pub sw_mld: bool,
    pub port: u32,
    pub mac_id: u32,
    pub hw_ssn_sel: u32,
    pub hw_seq_mode: u32,
    pub en_wd_info: bool,
    pub use_rate: bool,
    pub dis_data_fb: bool,
    pub data_rate: u32,
    pub tid_indicate: bool,
    pub er_cap: bool,
    pub stbc: bool,
    pub ldpc: bool,
    pub sec_en: bool,
    pub sec_keyid: u32,
    pub sec_type: u32,
    pub sec_cam_idx: u32,
    pub data_retry_lowest_rate: u32,
    /// what Linux queues btc.<pkt>_notify_work for
    pub btc_pkt: Option<BtcPkt>,
    pub a_ctrl_bsr: bool,
    pub bk: bool,
    /// rtw89_core_tx_update_llc_hdr, 2-byte units
    pub hdr_llc_len: u32,
    // never set for a legacy station without A-MPDU: 0 in the WD
    pub data_bw: u32,
    pub gi_ltf: u32,
    pub ampdu_num: u32,
    pub ampdu_density: u32,
    pub agg_en: bool,
    /// the WD info dwords (`Some` exactly when en_wd_info)
    pub wd_info: Option<[u32; 6]>,
}

impl TxInfo {
    /// The WD body fields and WD info pcitx writes (rtw89_core_fill_txdesc's input)
    pub fn desc(&self) -> TxDescInfo {
        TxDescInfo {
            wp_offset: 0,
            ch_dma: self.ch_dma,
            hdr_llc_len: self.hdr_llc_len,
            wd_page: self.wd_page,
            fw_dl: false,
            hw_ssn_sel: self.hw_ssn_sel,
            hw_seq_mode: self.hw_seq_mode,
            tid_indicate: self.tid_indicate,
            qsel: self.qsel,
            pkt_size: self.pkt_size,
            mac_id: self.mac_id,
            seq: self.seq,
            agg_en: self.agg_en,
            bk: self.bk,
            wd_info: if self.en_wd_info { self.wd_info } else { None },
        }
    }
}

fn refuse(what: &'static str, value: u32) -> Rtw89Error {
    Rtw89Error::Frame { what, value }
}

// Linux: rtw89_core_tx_update_desc_info (core.c:1264); `kind` checks the frame's type
// (`None`: as the frame says), `TxKind::Fwcmd` gives rtw89_core_tx_update_h2c_info (core.c:941).
// Refused: a frame shorter than its 24-byte header, a kind other than the frame's, a protected
// frame without key (or the reverse), a key without CAM entry or of another cipher, no band.
pub fn tx_info(
    t: &mut Txrx,
    kind: Option<TxKind>,
    frame: &[u8],
    o: &TxOpts,
) -> Rtw89Result<TxInfo> {
    let mut info = TxInfo {
        pkt_size: frame.len() as u32,
        ..TxInfo::default()
    };
    if kind == Some(TxKind::Fwcmd) {
        info.ch_dma = RTW89_DMA_H2C;
        return Ok(info);
    }
    if frame.len() < 24 {
        return Err(refuse(
            "frame shorter than its 802.11 header",
            frame.len() as u32,
        ));
    }
    let fc = le16(frame, 0);
    let typ = if is_mgmt(fc) || is_any_nullfunc(fc) {
        TxType::Mgmt
    } else {
        TxType::Data
    };
    info.tx_type = Some(typ);
    let want = match kind {
        Some(TxKind::Mgmt) => Some(TxType::Mgmt),
        Some(TxKind::Data) => Some(TxType::Data),
        _ => None,
    };
    if want.is_some_and(|w| w != typ) {
        return Err(refuse("frame of another type than asked", u32::from(fc)));
    }
    // no MLO: desc_info->mlo and upd_wlan_hdr stay false
    info.seq = u32::from((le16(frame, 22) & 0xfff0) >> 4);
    if o.req_tx_status {
        tx_rpt_init(t, &mut info);
    }
    info.is_bmc = frame[4] & 1 == 1; // addr1 broadcast or multicast
    info.wd_page = true;
    info.hiq = false; // IEEE80211_TX_CTL_SEND_AFTER_DTIM: AP only
    info.upd_wlan_hdr = false;
    if typ == TxType::Mgmt {
        update_mgmt_info(t, &mut info, o)?;
    } else {
        // mac80211 never hands a protected frame without its key or the reverse
        if o.key.is_some() != (fc & fctl::PROTECTED != 0) {
            return Err(refuse("Protected bit and key do not agree", u32::from(fc)));
        }
        update_data_info(t, &mut info, o)?;
        let pkt = btc_spec_pkt(frame);
        info.btc_pkt = pkt; // Linux queues btc.<pkt>_notify_work here
        update_he_qos_htc(t, &mut info);
        if pkt == Some(BtcPkt::Eapol) {
            info.bk = true; // rtw89_core_tx_update_ampdu_info; no A-MPDU without HT
        }
        info.hdr_llc_len = (hdr_len(fc) >> 1) as u32;
    }
    // IEEE80211_TX_CTL_INJECTED: monitor injection, never here
    info.wd_info = Some(wd_info_dwords(&info));
    Ok(info)
}

// Linux: rtw89_core_tx_update_mgmt_info (core.c:901)
fn update_mgmt_info(t: &Txrx, info: &mut TxInfo, o: &TxOpts) -> Rtw89Result<()> {
    info.qsel = RTW89_TX_QSEL_B0_MGMT; // rtw89_core_get_qsel_mgmt: MAC 0, no hiq
    info.ch_dma = get_ch_dma(info.qsel);
    info.sw_mld = true;
    info.port = 0; // hiq ? port : 0
    info.mac_id = mac_id_of(t, o);
    info.hw_ssn_sel = RTW89_MGMT_HW_SSN_SEL;
    info.hw_seq_mode = RTW89_MGMT_HW_SEQ_MODE;
    info.en_wd_info = true; // fixed data rate for mgmt frames
    info.use_rate = true;
    info.dis_data_fb = true;
    info.data_rate = mgmt_rate(t, o)?;
    // 8852B: hw_mgmt_tx_encrypt false, so no security fields for management frames
    Ok(())
}

// Linux: rtw89_core_tx_update_data_info (core.c:1122)
fn update_data_info(t: &Txrx, info: &mut TxInfo, o: &TxOpts) -> Rtw89Result<()> {
    let tid = o.tid & 7; // IEEE80211_QOS_CTL_TAG1D_MASK
    info.tid_indicate = get_tid_indicate(tid);
    info.qsel = if info.hiq {
        RTW89_TX_QSEL_B0_HI
    } else {
        get_qsel(tid)
    };
    info.ch_dma = get_ch_dma(info.qsel);
    info.sw_mld = false;
    info.mac_id = mac_id_of(t, o);
    info.port = 0;
    info.er_cap = o.er_cap; // a legacy station: all false
    info.stbc = o.stbc;
    info.ldpc = o.ldpc;
    info.en_wd_info = true; // enable wd_info for AMPDU
    if let Some(key) = &o.key {
        update_sec_key(info, key)?;
    }
    info.data_retry_lowest_rate = data_rate(t, o)?;
    Ok(())
}

// Linux: rtw89_core_tx_update_sec_key (core.c:679), CCMP only (no TKIP in this chip)
fn update_sec_key(info: &mut TxInfo, key: &TxKey) -> Rtw89Result<()> {
    let Some(cam) = key.sec_cam_idx else {
        return Err(refuse("sec cam entry is empty", u32::from(key.key_idx)));
    };
    if key.cipher != cipher::CCMP {
        return Err(refuse("key cipher not supported", key.cipher));
    }
    info.sec_en = true;
    info.sec_keyid = u32::from(key.key_idx);
    info.sec_type = RTW89_SEC_KEY_TYPE_CCMP128;
    info.sec_cam_idx = cam;
    // 8852B: hw_sec_hdr false, so no sec_seq/wp_offset: the host wrote the CCMP header
    Ok(())
}

// Linux: rtw89_core_tx_get_mac_id (core.c:788), no MLO
fn mac_id_of(t: &Txrx, o: &TxOpts) -> u32 {
    o.mac_id.or(t.mac_id).unwrap_or(0)
}

fn band_of(t: &Txrx, o: &TxOpts) -> Rtw89Result<Band> {
    match o.band.or(t.chan.map(|c| c.band)) {
        Some(b @ (Band::B2g | Band::B5g)) => Ok(b),
        Some(b) => Err(refuse("band not supported for TX", b as u32)),
        None => Err(refuse("band unknown (no channel, none given)", 0)),
    }
}

// the lowest set bit's index (`m` is not 0)
fn ffs(m: u32) -> u32 {
    m.trailing_zeros()
}

// Linux: rtw89_core_get_mgmt_rate (core.c:748); rtwvif_link always exists here
fn mgmt_rate(t: &Txrx, o: &TxOpts) -> Rtw89Result<u32> {
    let lowest = if o.no_cck || band_of(t, o)? != Band::B2g {
        RTW89_HW_RATE_OFDM6
    } else {
        RTW89_HW_RATE_CCK1
    };
    if o.basic_rates == 0 || !o.sta {
        return Ok(lowest);
    }
    Ok(ffs(o.basic_rates) + lowest)
}

// Linux: rtw89_core_get_data_rate (core.c:1080); no rate pattern, no p2p
fn data_rate(t: &Txrx, o: &TxOpts) -> Rtw89Result<u32> {
    let lowest = if band_of(t, o)? == Band::B2g {
        RTW89_HW_RATE_CCK1
    } else {
        RTW89_HW_RATE_OFDM6
    };
    if !o.sta || o.supp_rates == 0 {
        return Ok(lowest);
    }
    Ok(ffs(o.supp_rates) + lowest)
}

// Linux: rtw89_tx_rpt_init (mac.h:1834)
fn tx_rpt_init(t: &mut Txrx, info: &mut TxInfo) {
    let s = &mut t.tx;
    if !s.tx_rpt_enabled {
        return;
    }
    info.report = true;
    s.tx_rpt_sn = (s.tx_rpt_sn + 1) & 0xff;
    info.sn = s.tx_rpt_sn & RTW89_MAX_TX_RPTS_MASK; // the firmware keeps a 4-bit sequence number
    info.tx_cnt_lmt_en = true;
    info.tx_cnt_lmt = 8;
}

// Linux: rtw89_core_tx_update_he_qos_htc (core.c:1056). The station never associates with
// HE, so __rtw89_core_tx_check_he_qos_htc is false.
fn update_he_qos_htc(t: &mut Txrx, info: &mut TxInfo) {
    info.a_ctrl_bsr = false;
    if t.tx.last_actrl == info.a_ctrl_bsr {
        return;
    }
    t.tx.last_actrl = info.a_ctrl_bsr;
    info.bk = true;
}

// Linux: rtw89_core_tx_btc_spec_pkt_notify (core.c:1157): EAPOL, ARP, DHCP, ICMP, read from
// the frame (skb->protocol and the IP header in Linux)
fn btc_spec_pkt(f: &[u8]) -> Option<BtcPkt> {
    let fc = le16(f, 0);
    let mut p = hdr_len(fc)
        + if fc & fctl::PROTECTED != 0 {
            CCMP_HDR_LEN
        } else {
            0
        };
    if f.len() < p + 8 || f[p] != 0xaa || f[p + 1] != 0xaa {
        return None;
    }
    let typ = u16::from_be_bytes([f[p + 6], f[p + 7]]);
    p += 8;
    if typ == ETH_P_PAE {
        return Some(BtcPkt::Eapol); // IEEE80211_TX_CTRL_PORT_CTRL_PROTO
    }
    if typ == ETH_P_ARP {
        return Some(BtcPkt::Arp);
    }
    if typ != ETH_P_IP || f.len() < p + 20 {
        return None;
    }
    let (proto, ihl) = (f[p + 9], usize::from(f[p] & 0xf) * 4);
    if proto == 17 && f.len() >= p + ihl + 4 {
        let sp = u16::from_be_bytes([f[p + ihl], f[p + ihl + 1]]);
        let dp = u16::from_be_bytes([f[p + ihl + 2], f[p + ihl + 3]]);
        if ((sp == 67 && dp == 68) || (sp == 68 && dp == 67)) && f.len() > 282 {
            return Some(BtcPkt::Dhcp);
        }
    }
    if proto == 1 {
        return Some(BtcPkt::Icmp);
    }
    None
}

// Linux: ieee80211_tx_h_sequence (net/mac80211/tx.c): the interface counter for management and
// non-QoS data frames, the per-TID counter for unicast QoS data. Control frames and QoS nulls
// keep theirs. A frame too short for its header is refused, unchanged.
pub fn tx_seq(t: &mut Txrx, frame: &mut [u8]) -> Rtw89Result<()> {
    if frame.len() < 2 {
        return Err(refuse("frame shorter than its frame control", 0));
    }
    let fc = le16(frame, 0);
    if is_ctl(fc) || hdr_len(fc) < 24 || is_qos_nullfunc(fc) {
        return Ok(());
    }
    let n = hdr_len(fc);
    let at = n - if fc & fctl::ORDER != 0 { 6 } else { 2 };
    let qos = is_data_qos(fc) && frame.len() > 4 && frame[4] & 1 == 0;
    if frame.len() < 24 || (qos && frame.len() < at + 2) {
        return Err(refuse(
            "frame shorter than its 802.11 header",
            frame.len() as u32,
        ));
    }
    let s = &mut t.tx;
    if !qos {
        frame[22..24].copy_from_slice(&s.seq_ctrl.to_le_bytes());
        s.seq_ctrl = s.seq_ctrl.wrapping_add(0x10);
        return Ok(());
    }
    let tid = usize::from(le16(frame, at) & 0xf);
    frame[22..24].copy_from_slice(&s.tid_seq[tid].to_le_bytes());
    s.tid_seq[tid] = s.tid_seq[tid].wrapping_add(0x10);
    Ok(())
}

/// What `tx_frame` did: the TX info, and where the frame is (`None`: the ring was full, Linux
/// keeps data frames in mac80211's queue and drops management frames: reclaim, retry)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxSent {
    pub info: TxInfo,
    pub queued: Option<Queued>,
}

// Linux: rtw89_ops_tx (mac80211.c:19) for management and null frames (tx_write, kick off),
// rtw89_core_txq_schedule (core.c:4826) for data frames (reclaim check, tx_write, kick off);
// mac80211's sequence number first (unless `keep_seq`).
pub fn tx_frame<B: Bus + ?Sized>(
    bus: &mut B,
    t: &mut Txrx,
    pcitx: &mut PciTx,
    rings: &mut PciRings,
    frame: &mut [u8],
    o: &TxOpts,
) -> Rtw89Result<TxSent> {
    if !o.keep_seq {
        tx_seq(t, frame)?;
    }
    let info = tx_info(t, None, frame, o)?;
    if info.tx_type == Some(TxType::Data) && pcitx.resource(bus, rings, info.ch_dma)? == 0 {
        return Ok(TxSent { info, queued: None });
    }
    let queued = pcitx.tx(bus, rings, info.ch_dma, &info.desc(), frame, !o.no_kick)?;
    Ok(TxSent { info, queued })
}
