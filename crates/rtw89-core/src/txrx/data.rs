//! Ethernet frames <-> 802.11 data frames for a station (JS js/txrx-data.mjs), as mac80211
//! converts them (ieee80211_build_hdr, ieee80211_data_to_8023), with CCMP done by the chip:
//! the 8852B has no hardware security header (hw_sec_hdr false, rtw8852b.c:1007), so rtw89
//! sets IEEE80211_KEY_FLAG_GENERATE_IV (cam.c:523) and the host writes the CCMP header with
//! the PN; the chip adds MIC and FCS. On RX the chip decrypts but leaves CCMP header and MIC
//! in place (only RX_FLAG_DECRYPTED is set, core.c:4399), so they are stripped here, with
//! mac80211's replay check.

use alloc::vec::Vec;

use super::ieee::*;
use crate::error::{Rtw89Error, Rtw89Result};

const RFC1042: [u8; 6] = [0xaa, 0xaa, 0x03, 0x00, 0x00, 0x00];
const BRIDGE_TUNNEL: [u8; 6] = [0xaa, 0xaa, 0x03, 0x00, 0x00, 0xf8];
pub const ETH_P_IP: u16 = 0x0800;
pub const ETH_P_ARP: u16 = 0x0806;
pub const ETH_P_PAE: u16 = 0x888e;
const ETH_P_AARP: u16 = 0x80f3;
const ETH_P_IPX: u16 = 0x8137;
const ETH_P_802_3_MIN: u16 = 0x0600;
pub const CCMP_HDR_LEN: usize = 8;
pub const CCMP_MIC_LEN: usize = 8;
pub const FCS_LEN: usize = 4;
/// IEEE80211_NUM_TIDS. mac80211 keeps NUM_TIDS + 1 replay counters (key->u.ccmp.rx_pn):
/// one per TID, where non-QoS data shares TID 0's (security_idx 0, ieee80211_parse_qos),
/// and the one past the TIDs for management frames (only with PMF, which is not ported)
pub const NUM_TIDS: usize = 16;

/// A CCMP key as txrx needs it (the rest of a key is sta's): its index, the security CAM
/// entry the chip encrypts with, and the packet numbers (key->conf.tx_pn and mac80211's
/// per-TID receive counters, starting at the handshake's RSC)
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CcmpKey {
    pub key_idx: u8,
    /// the security CAM entry; `None`: not in the CAM (a TX with it is refused)
    pub sec_cam_idx: Option<u32>,
    /// the last PN sent (the first frame gets 1)
    pub tx_pn: u64,
    /// the receive sequence counter from the handshake
    pub rsc: u64,
    /// the last PN received per counter: index = mac80211's security_idx, so 0-15 per TID
    /// (0 also for non-QoS data), NUM_TIDS for management frames (PMF, unused so far)
    pub rx_pn: [Option<u64>; NUM_TIDS + 1],
    /// frames dropped as replays
    pub replays: u32,
}

impl CcmpKey {
    pub fn new(key_idx: u8, sec_cam_idx: Option<u32>) -> CcmpKey {
        CcmpKey {
            key_idx,
            sec_cam_idx,
            ..CcmpKey::default()
        }
    }
}

// Linux: ccmp_pn2hdr (net/mac80211/wpa.c): PN0 PN1 rsvd (ExtIV | KeyID << 6) PN2 PN3 PN4 PN5
pub fn ccmp_hdr(pn: u64, key_id: u8) -> [u8; CCMP_HDR_LEN] {
    let p = pn.to_le_bytes();
    [
        p[0],
        p[1],
        0,
        0x20 | ((key_id & 3) << 6),
        p[2],
        p[3],
        p[4],
        p[5],
    ]
}

// Linux: ccmp_hdr2pn (net/mac80211/wpa.c)
pub fn ccmp_pn(h: &[u8; CCMP_HDR_LEN]) -> u64 {
    u64::from_le_bytes([h[0], h[1], h[4], h[5], h[6], h[7], 0, 0])
}

/// How a data frame goes out
#[derive(Debug)]
pub struct DataTx<'a> {
    pub bssid: Mac,
    /// the pairwise key once it is in: the frame is protected and gets the next PN
    pub key: Option<&'a mut CcmpKey>,
    /// QoS data with this TID (the association has no WMM: plain data by default)
    pub qos: Option<u16>,
    pub seq: u16,
}

// Linux: ieee80211_build_hdr (net/mac80211/tx.c) for a station (To DS: addr1 BSSID, addr2
// source, addr3 destination), RFC 1042 / bridge-tunnel encapsulation, and ccmp_encrypt_skb's
// header (net/mac80211/wpa.c) when a key is given: its tx_pn counts like key->conf.tx_pn
// (incremented before use, so the first PN is 1). An Ethernet frame shorter than its header
// is refused.
pub fn eth_to_wlan(eth: &[u8], o: DataTx) -> Rtw89Result<Vec<u8>> {
    if eth.len() < 14 {
        return Err(Rtw89Error::Frame {
            what: "Ethernet frame shorter than its header",
            value: eth.len() as u32,
        });
    }
    let (dst, src) = (mac_at(eth, 0), mac_at(eth, 6));
    let typ = u16::from_be_bytes([eth[12], eth[13]]);
    let flags = fctl::TODS | if o.key.is_some() { fctl::PROTECTED } else { 0 };
    let stype = if o.qos.is_some() {
        st::QOS_DATA
    } else {
        st::DATA
    };
    let mut f = hdr(fc_of(T_DATA, stype, flags), &o.bssid, &src, &dst, o.seq);
    if let Some(tid) = o.qos {
        f.extend_from_slice(&(tid & 7).to_le_bytes());
    }
    if let Some(key) = o.key {
        key.tx_pn = key.tx_pn.wrapping_add(1);
        f.extend_from_slice(&ccmp_hdr(key.tx_pn, key.key_idx));
    }
    if typ >= ETH_P_802_3_MIN {
        let llc = if typ == ETH_P_AARP || typ == ETH_P_IPX {
            BRIDGE_TUNNEL
        } else {
            RFC1042
        };
        f.extend_from_slice(&llc);
        f.extend_from_slice(&typ.to_be_bytes());
    }
    f.extend_from_slice(&eth[14..]);
    Ok(f)
}

/// A data frame from the AP as an Ethernet frame, with what the RX checks need
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EthRx {
    pub eth: Vec<u8>,
    pub dst: Mac,
    pub src: Mac,
    pub bssid: Mac,
    /// the EtherType (or the payload's length for a frame without LLC/SNAP)
    pub typ: u16,
    pub eapol: bool,
    pub protected: bool,
    pub pn: Option<u64>,
    pub key_id: Option<u8>,
    pub seq: u16,
    pub retry: bool,
    pub qos: bool,
    pub tid: u16,
    /// mac80211's security_idx (ieee80211_parse_qos): the TID for QoS data, else 0
    pub sec_idx: u16,
    pub multicast: bool,
}

// Linux: ieee80211_data_to_8023_exthdr (net/wireless/util.c) for a frame from the AP (From
// DS: addr1 destination, addr2 BSSID, addr3 source), after the CCMP header and MIC of a
// decrypted frame are taken off. `mpdu` is the frame without FCS. `None` for frames without
// payload (null data, control, management), 4-address frames, and protected frames too short
// for CCMP or without ExtIV. No A-MSDU (never negotiated without HT).
pub fn wlan_to_eth(mpdu: &[u8]) -> Option<EthRx> {
    if mpdu.len() < 24 {
        return None;
    }
    let fc = le16(mpdu, 0);
    if !is_data(fc) || is_any_nullfunc(fc) || mpdu.len() < hdr_len(fc) {
        return None;
    }
    let h = hdr_parse(mpdu)?;
    if h.to_ds && h.from_ds {
        return None; // 4-address frames are not for a station
    }
    let (a1, a2, a3) = (h.addr1?, h.addr2?, h.addr3?);
    let (da, bssid, sa) = if h.from_ds {
        (a1, a2, a3)
    } else {
        (a1, a3, a2)
    };
    let (mut pos, mut end) = (hdr_len(fc), mpdu.len());
    let (mut pn, mut key_id) = (None, None);
    if h.protected {
        if end < pos + CCMP_HDR_LEN + CCMP_MIC_LEN {
            return None;
        }
        let mut c = [0u8; CCMP_HDR_LEN];
        c.copy_from_slice(&mpdu[pos..pos + CCMP_HDR_LEN]);
        if c[3] & 0x20 == 0 {
            return None; // no ExtIV: not CCMP
        }
        pn = Some(ccmp_pn(&c));
        key_id = Some(c[3] >> 6);
        pos += CCMP_HDR_LEN;
        end -= CCMP_MIC_LEN;
    }
    let payload = &mpdu[pos..end];
    let snap = payload.len() >= 8 && {
        let head = &payload[..6];
        let t = u16::from_be_bytes([payload[6], payload[7]]);
        (head == RFC1042 && t != ETH_P_AARP && t != ETH_P_IPX) || head == BRIDGE_TUNNEL
    };
    let (typ, data) = if snap {
        (u16::from_be_bytes([payload[6], payload[7]]), &payload[8..])
    } else {
        // an 802.3 frame: the length field (a frame of the air is never past 64 KiB)
        (payload.len() as u16, payload)
    };
    let mut eth = Vec::with_capacity(14 + data.len());
    eth.extend_from_slice(&da);
    eth.extend_from_slice(&sa);
    eth.extend_from_slice(&typ.to_be_bytes());
    eth.extend_from_slice(data);
    let tid = h.tid.unwrap_or(0);
    Some(EthRx {
        eth,
        dst: da,
        src: sa,
        bssid,
        typ,
        eapol: typ == ETH_P_PAE,
        protected: h.protected,
        pn,
        key_id,
        seq: h.seq.unwrap_or(0),
        retry: h.retry,
        qos: h.qos.is_some(),
        tid,
        sec_idx: if h.qos.is_some() { tid } else { 0 },
        multicast: da[0] & 1 == 1,
    })
}

// Linux: the replay check of ieee80211_crypto_ccmp_decrypt (net/mac80211/wpa.c) for a frame
// the chip decrypted: a PN not above the last one of that counter is a replay. A counter
// starts at the key's receive sequence counter (the handshake's RSC). Returns true to drop.
// A counter index past the last slot is a replay (never one the parser gives); `wlan_to_eth`
// gives TIDs 0-15 only, the management slot NUM_TIDS is for sta's PMF (not ported).
pub fn ccmp_replay(key: &mut CcmpKey, sec_idx: u16, pn: u64) -> bool {
    let Some(slot) = key.rx_pn.get_mut(usize::from(sec_idx)) else {
        key.replays = key.replays.wrapping_add(1);
        return true;
    };
    let last = slot.unwrap_or(key.rsc);
    if pn <= last {
        key.replays = key.replays.wrapping_add(1);
        return true;
    }
    *slot = Some(pn);
    false
}
