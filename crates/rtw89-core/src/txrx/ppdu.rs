//! The PPDU status packets of the RXQ ring and the rx_status of a frame (JS js/txrx-rx.mjs):
//! rtw89_core_rx_process_ppdu_sts and its MAC/PHY parts, the pairing of waiting frames with
//! their PPDU status, and the fields mac80211 would get. Every read is bounds-checked.

use alloc::vec::Vec;

use crate::bus::field;
use crate::phy::{self, Band, Phy};
use crate::tables::regs::*;
use crate::tables::txrx::{PHYST_IE_LEN, VAR_LEN};

use super::Txrx;
use super::desc::RxDesc;
use super::ieee::{is_beacon, is_probe_resp, le16, parse_ies};
use super::parse::ies_channel;
use super::rx::{RxEntry, RxResult, RxType};

/// rtw8852b_chip_info.ppdu_max_usr
const PPDU_MAX_USR: u32 = 4;

fn le32(b: &[u8], at: usize) -> Option<u32> {
    let s = b.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

/// The OFDM part of a PPDU status (IE01)
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PpduOfdm {
    pub has: bool,
    pub avg_snr: u32,
    pub evm_max: u32,
    pub evm_min: u32,
}

/// struct rtw89_rx_phy_ppdu: one PPDU status as rtw89_core_rx_process_ppdu_sts reads it
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PhyPpdu {
    /// the PPDU status packet
    pub buf: Vec<u8>,
    /// where its PHY status starts and how long it is
    pub start: usize,
    pub len: usize,
    pub valid: bool,
    pub to_self: bool,
    pub rate: u32,
    pub mac_id: u32,
    pub phy_idx: u32,
    pub rssi: [u8; 4],
    pub rssi_avg: u8,
    pub rpl_avg: u8,
    pub chan_idx: u8,
    pub ie: u32,
    pub rx_path_en: u32,
    pub ofdm: PpduOfdm,
    pub has_data: bool,
    pub has_bcn: bool,
    /// where IE09 / IE10 start in `buf`
    pub ie09: Option<usize>,
    pub ie10: Option<usize>,
    /// `Some` once rtw89_core_rx_process_phy_ppdu got past its checks (only then does Linux
    /// run rtw89_core_rx_process_phy_sts and its station iteration)
    pub hdr2_en: Option<bool>,
    pub ldpc: bool,
    pub stbc: bool,
    pub bf: bool,
    pub su: bool,
    /// IE01's FD CFO (S(12,2)), for an OFDM frame to us
    pub cfo: Option<i32>,
}

/// How the frame was sent (rx_status->encoding)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Encoding {
    #[default]
    Legacy,
    Ht,
    Vht,
    He,
    Invalid,
}

/// The HE guard interval (rtw89_rxdesc_to_nl_he_gi)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HeGi {
    Gi08,
    Gi16,
    #[default]
    Gi32,
}

/// The rx_status mac80211 would get (struct ieee80211_rx_status, the fields rtw89 fills)
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RxStatus {
    pub band: Option<Band>,
    pub ch: Option<u32>,
    pub freq: Option<u32>,
    /// ICV or CRC32 error: mac80211 drops the frame
    pub failed_fcs: bool,
    /// RX_FLAG_DECRYPTED: the chip decrypted it (CCMP header and MIC still in place)
    pub decrypted: bool,
    pub ampdu: bool,
    pub ampdu_ref: Option<u32>,
    /// bandwidth in MHz
    pub bw: u32,
    /// dBm; 0 until a matching PPDU status gave one
    pub signal: i32,
    /// RF paths with a chain signal (bit per path)
    pub chains: u32,
    pub chain_signal: [i32; 2],
    pub encoding: Encoding,
    /// the rate index (the band's sband index for legacy rates after rtw89_core_hw_to_sband_rate)
    pub rate_idx: u32,
    pub nss: Option<u32>,
    pub short_gi: bool,
    pub he_gi: HeGi,
    pub mactime: u32,
    /// rtw89_core_validate_rx_signal: no signal (RX_FLAG_NO_SIGNAL_VAL)
    pub no_signal: bool,
}

/// The chip ops rx_parse calls at Linux's points; every one may do nothing (no PHY: raw RSSI,
/// no signal)
pub trait RxOps {
    // rtw8852bx_convert_rpl_to_rssi, at the end of rtw89_core_parse_phy_status
    fn convert_rpl_to_rssi(&mut self, _p: &mut PhyPpdu) {}
    // rtw8852bx_query_ppdu, for a waiting frame its PPDU status matched
    fn query_ppdu(&mut self, _p: &PhyPpdu, _st: &mut RxStatus) {}
    // rtw89_phy_cfo_parse, for IE01 of an OFDM frame to us
    fn cfo_parse(&mut self, _cfo: i32, _p: &PhyPpdu) {}
    // rtw89_core_rx_stats' place in rtw89_core_rx_to_mac80211 (station statistics, the
    // beacon RSSI H2C: sta's), with Linux's phy_ppdu and desc_info
    fn rx_stats(&mut self, _e: &RxEntry, _p: Option<&PhyPpdu>, _desc: Option<&RxDesc>) {}
}

impl RxOps for () {}

/// The 8852B's chip ops from phy (JS sta-io.mjs's `ops` with phy.mjs)
pub struct PhyOps<'a>(pub &'a mut Phy);

fn phy_ppdu(p: &PhyPpdu) -> phy::Ppdu {
    phy::Ppdu {
        rssi_avg: p.rssi_avg,
        rpl_avg: p.rpl_avg,
        rssi: [p.rssi[0], p.rssi[1]],
        chan_idx: p.chan_idx,
        valid: p.valid,
    }
}

impl RxOps for PhyOps<'_> {
    fn convert_rpl_to_rssi(&mut self, p: &mut PhyPpdu) {
        let mut q = phy_ppdu(p);
        phy::convert_rpl_to_rssi(&mut q);
        p.rssi[0] = q.rssi[0];
        p.rssi[1] = q.rssi[1];
        p.rssi_avg = q.rssi_avg;
    }
    // JS phy.mjs queryPpdu: the signal only where none is set, chains added, the channel of
    // the PPDU status when it has one
    fn query_ppdu(&mut self, p: &PhyPpdu, st: &mut RxStatus) {
        let r = phy::query_ppdu(&phy_ppdu(p));
        if st.signal == 0 {
            st.signal = r.signal;
        }
        st.chains |= r.chains;
        st.chain_signal = r.chain_signal;
        if let Some(b) = r.band {
            st.band = Some(b);
            st.ch = r.ch;
            st.freq = r.freq;
        }
    }
    fn cfo_parse(&mut self, cfo: i32, p: &PhyPpdu) {
        phy::trk::cfo_parse(self.0, cfo, p.mac_id);
    }
}

// Linux: rtw89_core_rx_process_ppdu_sts (core.c:4059)
pub(super) fn process_ppdu_sts(
    t: &mut Txrx,
    desc: RxDesc,
    skb: Vec<u8>,
    ops: &mut dyn RxOps,
) -> RxResult {
    let mut p = PhyPpdu {
        start: 0,
        len: skb.len(),
        to_self: desc.addr1_match,
        rate: desc.data_rate,
        mac_id: desc.mac_id,
        phy_idx: u32::from(desc.bb_sel),
        buf: skb,
        ..PhyPpdu::default()
    };
    let mut ok = true;
    if desc.mac_info_valid {
        ok = process_mac_ppdu(&mut p);
    }
    if ok {
        ok = process_phy_ppdu(&mut p);
    }
    if ok {
        // rtw89_core_rx_process_phy_ppdu_iter (the per-station EWMA) is the caller's
        p.valid = parse_phy_sts(&mut p, ops);
    }
    let mut out = RxResult::new(RxType::Ppdu, Some(desc.clone()));
    rx_pending_skb(t, &p, &desc, &mut out.deliver, ops);
    out.ppdu = Some(p);
    out
}

// Linux: rtw89_core_rx_process_mac_ppdu (core.c:1939), AX branches
fn process_mac_ppdu(p: &mut PhyPpdu) -> bool {
    let b = &p.buf;
    let (Some(w0), Some(w1)) = (le32(b, 0), le32(b, 4)) else {
        return false;
    };
    let rx_cnt_valid = w0 & RTW89_RXINFO_W0_RX_CNT_VLD != 0;
    let plcp_size = (field(w1, RTW89_RXINFO_W1_PLCP_LEN) << 3) as usize;
    let usr_num = field(w0, RTW89_RXINFO_W0_USR_NUM);
    if usr_num > PPDU_MAX_USR {
        return false;
    }
    for i in 0..usr_num as usize {
        let Some(u) = le32(b, 8 + 4 * i) else { break };
        if u & RTW89_RXINFO_USER_MAC_ID_VALID == 0 {
            continue;
        }
        // 8852B: not RTL8922A/D, so mac_id stays the one of the RX descriptor
        p.has_data = u & RTW89_RXINFO_USER_DATA != 0;
        p.has_bcn = u & RTW89_RXINFO_USER_BCN != 0;
        break;
    }
    let usr = RTW89_PPDU_MAC_INFO_USR_SIZE as usize;
    let mut pos = RTW89_PPDU_MAC_INFO_SIZE as usize + usr_num as usize * usr;
    if usr_num & 1 == 1 {
        pos += usr; // 8-byte alignment
    }
    if rx_cnt_valid {
        pos += RTW89_PPDU_MAC_RX_CNT_SIZE as usize;
    }
    pos += plcp_size;
    if pos > p.buf.len() {
        return false;
    }
    p.start = pos;
    p.len = p.buf.len() - pos;
    true
}

// Linux: rtw89_core_rx_process_phy_ppdu (core.c:2285) + rtw89_core_update_phy_ppdu (core.c:2268)
fn process_phy_ppdu(p: &mut PhyPpdu) -> bool {
    let o = p.start;
    if p.len < 8 {
        return false;
    }
    let (Some(w0), Some(w1)) = (le32(&p.buf, o), le32(&p.buf, o + 4)) else {
        return false;
    };
    if w0 & RTW89_PHY_STS_HDR_W0_VALID == 0 {
        return false;
    }
    // AX: no PHY_STS_HDR_LEN added
    let len_from_header = (field(w0, RTW89_PHY_STS_HDR_W0_LEN) << 3) as usize;
    if len_from_header != p.len {
        return false;
    }
    p.ie = field(w0, RTW89_PHY_STS_HDR_W0_IE_MAP);
    p.rssi_avg = field(w0, RTW89_PHY_STS_HDR_W0_RSSI_AVG) as u8;
    p.rssi = [
        field(w1, RTW89_PHY_STS_HDR_W1_RSSI_A) as u8,
        field(w1, RTW89_PHY_STS_HDR_W1_RSSI_B) as u8,
        field(w1, RTW89_PHY_STS_HDR_W1_RSSI_C) as u8,
        field(w1, RTW89_PHY_STS_HDR_W1_RSSI_D) as u8,
    ];
    let hdr2 = w0 & RTW89_PHY_STS_HDR_W0_HDR_2_EN != 0;
    p.hdr2_en = Some(hdr2);
    if let (true, Some(w)) = (hdr2 && p.len >= 16, le32(&p.buf, o + 8)) {
        p.rx_path_en = field(w, RTW89_PHY_STS_HDR_V2_W0_PATH_EN);
    }
    true
}

// Linux: rtw89_core_rx_parse_phy_sts (core.c:2310) under rtw89_core_rx_process_phy_sts
// (core.c:2359)
fn parse_phy_sts(p: &mut PhyPpdu, ops: &mut dyn RxOps) -> bool {
    if p.ie < RTW89_CCK_PKT {
        return false; // normal mode: invalid reports bypassed
    }
    let hl = PHY_STS_HDR_LEN as usize;
    let mut pos = p.start + hl;
    if p.hdr2_en == Some(true) {
        pos += hl;
    }
    let end = p.start + p.len;
    while pos < end {
        if end - pos < 4 {
            return false; // a torn IE header: Linux would read past it
        }
        let Some(w0) = le32(&p.buf, pos) else {
            return false;
        };
        if p.ie09.is_some() && end - pos == 8 && w0 == 0 {
            break; // RTL8922D zero padding check
        }
        let typ = field(w0, RTW89_PHY_STS_IEHDR_TYPE);
        // rtw89_core_get_phy_status_ie_len
        let fixed = PHYST_IE_LEN.get(typ as usize).copied().unwrap_or(VAR_LEN);
        let ie_len = if fixed != VAR_LEN {
            usize::from(fixed)
        } else {
            (field(w0, RTW89_PHY_STS_IEHDR_LEN) * VAR_LEN_UNIT) as usize
        };
        let at = pos;
        pos += ie_len;
        if pos > end || ie_len == 0 {
            p.ie09 = None;
            p.ie10 = None;
            return false;
        }
        process_phy_status_ie(p, typ, at, ops);
    }
    ops.convert_rpl_to_rssi(p);
    // rtw89_phy_antdiv_parse: 8852B has two RF paths, hal.ant_diversity stays false
    true
}

// Linux: rtw89_core_process_phy_status_ie (core.c:2221); the IE lies inside the buffer
fn process_phy_status_ie(p: &mut PhyPpdu, typ: u32, at: usize, ops: &mut dyn RxOps) {
    if !(p.to_self || typ == RTW89_PHYSTS_IE01_CMN_OFDM) {
        return;
    }
    let w = |p: &PhyPpdu, i: usize| le32(&p.buf, at + 4 * i).unwrap_or(0);
    let hdr2 = p.hdr2_en == Some(true);
    if typ == RTW89_PHYSTS_IE00_CMN_CCK {
        // rtw89_core_parse_phy_status_ie00
        p.rpl_avg = (field(w(p, 0), RTW89_PHY_STS_IE00_W0_RPL) >> 1) as u8;
        if !hdr2 {
            p.rx_path_en = field(w(p, 3), RTW89_PHY_STS_IE00_W3_RX_PATH_EN);
        }
    } else if typ == RTW89_PHYSTS_IE01_CMN_OFDM {
        // rtw89_core_parse_phy_status_ie01
        let (w0, w1, w2, w3) = (w(p, 0), w(p, 1), w(p, 2), w(p, 3));
        p.chan_idx = field(w0, RTW89_PHY_STS_IE01_W0_CH_IDX) as u8;
        p.ldpc = field(w2, RTW89_PHY_STS_IE01_W2_LDPC) != 0;
        p.stbc = field(w2, RTW89_PHY_STS_IE01_W2_STBC) != 0;
        p.bf = field(w3, RTW89_PHY_STS_IE01_W3_BF) != 0;
        if !hdr2 {
            p.rx_path_en = field(w0, RTW89_PHY_STS_IE01_W0_RX_PATH_EN);
        }
        if p.rate < RTW89_HW_RATE_OFDM6 || !p.to_self {
            return;
        }
        p.rpl_avg = field(w0, RTW89_PHY_STS_IE01_W0_RSSI_AVG_FD) as u8;
        p.su = field(w2, RTW89_PHY_STS_IE01_W2_SU) != 0;
        p.ofdm = PpduOfdm {
            has: true,
            avg_snr: field(w2, RTW89_PHY_STS_IE01_W2_AVG_SNR),
            evm_max: field(w2, RTW89_PHY_STS_IE01_W2_EVM_MAX),
            evm_min: field(w2, RTW89_PHY_STS_IE01_W2_EVM_MIN),
        };
        // 8852B: cfo_src_fd true; S(12,2)
        let v = field(w1, RTW89_PHY_STS_IE01_W1_FD_CFO) as i32;
        let cfo = if v & 0x800 != 0 { v - 0x1000 } else { v };
        p.cfo = Some(cfo);
        ops.cfo_parse(cfo, p);
    } else if typ == RTW89_PHYSTS_IE09_FTR_0 {
        p.ie09 = Some(at); // physt_gen 0: no length check
    } else if typ == RTW89_PHYSTS_IE10_FTR_PLCP_EXT {
        p.ie10 = Some(at);
    }
}

// Linux: rtw89_core_rx_pending_skb (core.c:4036): the match only decides query_ppdu, every
// waiting frame goes up with this PPDU status
fn rx_pending_skb(
    t: &mut Txrx,
    p: &PhyPpdu,
    desc: &RxDesc,
    deliver: &mut Vec<RxEntry>,
    ops: &mut dyn RxOps,
) {
    if t.rx.ppdu_curr > RTW89_MAX_PPDU_CNT {
        return;
    }
    for mut entry in core::mem::take(&mut t.rx.queue) {
        entry.ppdu_match = ppdu_match(t, desc, &entry.status);
        if entry.ppdu_match {
            ops.query_ppdu(p, &mut entry.status);
        }
        correct_cck_chan(t, &mut entry.status);
        to_mac80211(t, &mut entry, ops, Some(p), Some(desc));
        deliver.push(entry);
    }
}

fn rate_mode(rate: u32) -> u32 {
    field(rate, DATA_RATE_MODE_CTRL_MASK) // AX: not the BE mask
}

// rtw89_rxdesc_to_nl_he_gi; `None` where Linux returns U8_MAX (the status takes 3.2 then)
fn he_gi(gi_ltf: u32) -> Option<HeGi> {
    match gi_ltf {
        RTW89_GILTF_SGI_4XHE08 | RTW89_GILTF_2XHE08 | RTW89_GILTF_1XHE08 => Some(HeGi::Gi08),
        RTW89_GILTF_2XHE16 | RTW89_GILTF_1XHE16 => Some(HeGi::Gi16),
        RTW89_GILTF_LGI_4XHE32 => Some(HeGi::Gi32),
        _ => None,
    }
}

// rtw89_hw_to_rate_info_bw, in MHz
fn bw_mhz(bw: u32) -> u32 {
    match bw {
        RTW89_CHANNEL_WIDTH_160 => 160,
        RTW89_CHANNEL_WIDTH_80 => 80,
        RTW89_CHANNEL_WIDTH_40 => 40,
        _ => 20,
    }
}

// the rate index as rtw89_core_update_rx_status / rx_ppdu_match read it
fn rate_idx(rate: u32) -> u32 {
    match rate_mode(rate) {
        DATA_RATE_MODE_NON_HT => field(rate, DATA_RATE_NOT_HT_IDX_MASK),
        DATA_RATE_MODE_HT => field(rate, DATA_RATE_HT_IDX_MASK),
        _ => field(rate, DATA_RATE_VHT_HE_IDX_MASK),
    }
}

// Linux: rtw89_core_rx_ppdu_match (core.c:2437)
fn ppdu_match(t: &Txrx, desc: &RxDesc, st: &RxStatus) -> bool {
    t.rx.ppdu_curr == desc.ppdu_cnt
        && st.rate_idx == rate_idx(desc.data_rate)
        && Some(st.he_gi) == he_gi(desc.gi_ltf)
        && st.bw == bw_mhz(desc.bw)
}

/// Linux: ieee80211_channel_to_frequency (net/wireless/util.c) for 2.4/5 GHz (no band: 2.4)
pub fn ch_to_freq(ch: u32, band: Option<Band>) -> u32 {
    match band {
        Some(Band::B5g) => 5000 + 5 * ch,
        _ if ch == 14 => 2484,
        _ => 2407 + 5 * ch,
    }
}

// Linux: rtw89_core_update_rx_status (core.c:4363). The channel is txrx's (one channel
// context, so also the scan channel of SCAN_OFFLOAD firmware).
pub(super) fn update_rx_status(t: &Txrx, desc: &RxDesc) -> RxStatus {
    let band = t.chan.map(|c| c.band);
    let ch = t.chan.map(|c| c.ch);
    let m = rate_mode(desc.data_rate);
    let encoding = match m {
        DATA_RATE_MODE_NON_HT => Encoding::Legacy,
        DATA_RATE_MODE_HT => Encoding::Ht,
        DATA_RATE_MODE_VHT => Encoding::Vht,
        DATA_RATE_MODE_HE => Encoding::He,
        _ => Encoding::Invalid,
    };
    RxStatus {
        band,
        ch,
        freq: ch.filter(|&c| c != 0).map(|c| ch_to_freq(c, band)),
        failed_fcs: desc.icv_err || desc.crc32_err,
        decrypted: desc.hw_dec && !(desc.sw_dec || desc.icv_err),
        ampdu: desc.ampdu,
        ampdu_ref: desc.ampdu.then_some(desc.ppdu_cnt),
        bw: bw_mhz(desc.bw),
        encoding,
        rate_idx: rate_idx(desc.data_rate),
        nss: matches!(encoding, Encoding::Vht | Encoding::He)
            .then(|| field(desc.data_rate, DATA_RATE_VHT_HE_NSS_MASK) + 1),
        short_gi: matches!(encoding, Encoding::Ht | Encoding::Vht) && desc.gi_ltf != 0,
        // used to match the PPDU, so always filled
        he_gi: he_gi(desc.gi_ltf).unwrap_or(HeGi::Gi32),
        mactime: desc.free_run_cnt,
        // phy_rpt_to_rssi: NULL for 8852B; rtw89_core_stats_sta_rx_status: statistics
        ..RxStatus::default()
    }
}

// Linux: rtw89_correct_cck_chan (core.c:3187): a CCK frame cannot have come on 5 GHz, so the
// channel before the switch (rtw89_chan_rcd) is taken
fn correct_cck_chan(t: &Txrx, st: &mut RxStatus) {
    let prev = t.chan.and_then(|c| c.prev).filter(|&(_, ch)| ch != 0);
    let cck = st.band.is_some_and(|b| b != Band::B2g)
        && st.encoding == Encoding::Legacy
        && st.rate_idx < RTW89_HW_RATE_OFDM6;
    if let (true, Some((band, ch))) = (cck, prev) {
        st.band = Some(band);
        st.ch = Some(ch);
        st.freq = Some(ch_to_freq(ch, Some(band)));
    }
}

// Linux: rtw89_core_rx_to_mac80211 (core.c:4009) up to ieee80211_rx_napi. `ppdu` is Linux's
// phy_ppdu (`None` unless the frame waited for a PPDU status), `desc` the desc_info Linux
// passes along (the PPDU status' one on that path). The caller hands the entry up.
pub(super) fn to_mac80211(
    t: &Txrx,
    entry: &mut RxEntry,
    ops: &mut dyn RxOps,
    ppdu: Option<&PhyPpdu>,
    desc: Option<&RxDesc>,
) {
    entry.ppdu = ppdu.cloned();
    entry.ppdu_desc = ppdu.and(desc).cloned();
    let st = &mut entry.status;
    // rtw89_core_hw_to_sband_rate
    if st.band.is_some_and(|b| b != Band::B2g) && st.encoding == Encoding::Legacy {
        st.rate_idx = if st.rate_idx < RTW89_HW_RATE_OFDM6 {
            0
        } else {
            st.rate_idx - 4
        };
    }
    // rtw89_core_rx_stats: besides counters, rtw89_vif_rx_stats_iter sends the H2C OFLD_RSSI
    // for a beacon of the own BSS; that is sta's
    ops.rx_stats(entry, ppdu, desc);
    // rtw89_core_update_rx_status_by_ppdu, _update_radiotap: monitor mode only
    let st = &mut entry.status;
    if st.signal == 0 {
        st.no_signal = true; // rtw89_core_validate_rx_signal
    }
    update_rx_freq_from_ie(t, &entry.frame, &mut entry.status);
    // rtw89_core_correct_mcc_chan: MCC only. mac80211 trims the FCS (RX_INCLUDES_FCS,
    // core.c:7497) and drops failed-FCS frames: `RxEntry::mpdu`, `status.failed_fcs`
}

// Linux: rtw89_core_update_rx_freq_from_ie (core.c:3822); rx_freq_from_ie is true for 8852B
fn update_rx_freq_from_ie(t: &Txrx, f: &[u8], st: &mut RxStatus) {
    if !t.scanning || f.len() < 2 {
        return;
    }
    let fc = le16(f, 0);
    if !is_beacon(fc) && !is_probe_resp(fc) {
        return;
    }
    let hdr_len = 36; // offsetof(struct ieee80211_mgmt, u.beacon.variable)
    if f.len() <= hdr_len {
        return;
    }
    // skb->len still holds the FCS, as in Linux
    let Some(ch) = ies_channel(&parse_ies(f, hdr_len, f.len())) else {
        return;
    };
    let ch = u32::from(ch);
    let band = if ch > 14 { Band::B5g } else { Band::B2g };
    st.band = Some(band);
    st.ch = Some(ch);
    st.freq = Some(ch_to_freq(ch, Some(band)));
}
