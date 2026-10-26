//! Parsers of what the AP sends (JS js/txrx-ieee.mjs): the RSN element, the channel of a
//! beacon, beacons and probe responses, authentication, association response and
//! deauthentication. Every read is bounds-checked: a frame from the air never panics.

use alloc::string::String;
use alloc::vec::Vec;

use super::ieee::*;

fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// The RSN element body (802.11-2020 9.4.2.24) as wpa_parse_wpa_ie_rsn reads it
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rsn {
    pub version: u16,
    pub group: u32,
    pub pairwise: Vec<u32>,
    pub akms: Vec<u32>,
    pub caps: u16,
    pub pmkids: Vec<[u8; 16]>,
    pub group_mgmt: Option<u32>,
    /// why the parse stopped early: "version" (then the flags below are not filled, as in the
    /// JS) or "RSN list truncated" (the fields read before it stay)
    pub bad: Option<&'static str>,
    pub mfpc: bool,
    pub mfpr: bool,
    pub psk: bool,
    pub ccmp: bool,
}

// Linux: none (wpa_supplicant's wpa_parse_wpa_ie_rsn): omitted trailing fields take the
// defaults (CCMP-128 group and pairwise, AKM 00-0F-AC:1); `None` without a version field
pub fn rsn_parse(data: &[u8]) -> Option<Rsn> {
    if data.len() < 2 {
        return None;
    }
    let mut r = Rsn {
        version: le16(data, 0),
        group: cipher::CCMP,
        pairwise: alloc::vec![cipher::CCMP],
        akms: alloc::vec![akm::IEEE8021X],
        caps: 0,
        pmkids: Vec::new(),
        group_mgmt: None,
        bad: None,
        mfpc: false,
        mfpr: false,
        psk: false,
        ccmp: false,
    };
    if r.version != 1 {
        r.bad = Some("version");
        return Some(r);
    }
    let mut p = 2;
    // a suite list: count, then the suites; Err when it runs past the element
    let list = |p: &mut usize| -> Result<Option<Vec<u32>>, ()> {
        if *p + 2 > data.len() {
            return Ok(None);
        }
        let n = usize::from(le16(data, *p));
        *p += 2;
        if *p + 4 * n > data.len() {
            return Err(());
        }
        let out = (0..n).map(|i| be32(data, *p + 4 * i)).collect();
        *p += 4 * n;
        Ok(Some(out))
    };
    let body = (|| -> Result<(), ()> {
        if p + 4 <= data.len() {
            r.group = be32(data, p);
            p += 4;
        }
        if let Some(pw) = list(&mut p)? {
            r.pairwise = pw;
        }
        if let Some(ak) = list(&mut p)? {
            r.akms = ak;
        }
        if p + 2 <= data.len() {
            r.caps = le16(data, p);
            p += 2;
        }
        if p + 2 <= data.len() {
            let n = le16(data, p);
            p += 2;
            let mut i = 0;
            while i < n && p + 16 <= data.len() {
                let mut id = [0u8; 16];
                id.copy_from_slice(&data[p..p + 16]);
                r.pmkids.push(id);
                i += 1;
                p += 16;
            }
        }
        if p + 4 <= data.len() {
            r.group_mgmt = Some(be32(data, p));
        }
        Ok(())
    })();
    if body.is_err() {
        r.bad = Some("RSN list truncated");
    }
    r.mfpc = r.caps & rsn_cap::MFPC != 0;
    r.mfpr = r.caps & rsn_cap::MFPR != 0;
    r.psk = r.akms.contains(&akm::PSK);
    r.ccmp = r.pairwise.contains(&cipher::CCMP) && r.group == cipher::CCMP;
    Some(r)
}

// Linux: cfg80211_get_ies_channel_number (net/wireless/scan.c) for 2.4/5 GHz: the DS
// parameter set, else the HT operation's primary channel; `None` when neither
pub fn ies_channel(ies: &Elements) -> Option<u8> {
    if let Some(ds) = ies.find(eid::DS_PARAMS).filter(|e| e.data.len() == 1) {
        return Some(ds.data[0]);
    }
    ies.find(eid::HT_OPERATION)
        .filter(|e| e.data.len() >= 22)
        .map(|e| e.data[0])
}

/// What a beacon's or probe response's HT elements say
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HtInfo {
    pub cap_info: Option<u16>,
    pub primary_ch: Option<u8>,
    pub secondary_offset: Option<u8>,
}

/// A beacon or probe response, the fields cfg80211_inform_bss_frame takes
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Beacon {
    /// a beacon (else a probe response)
    pub beacon: bool,
    pub bssid: Mac,
    pub sa: Mac,
    pub tsf: u64,
    pub beacon_int: u16,
    pub capability: u16,
    pub privacy: bool,
    pub ssid_raw: Vec<u8>,
    /// the SSID as UTF-8 text (invalid bytes replaced)
    pub ssid: String,
    pub ch: Option<u8>,
    /// Supported and Extended Supported Rates, as sent (500 kbit/s units, bit 7 basic)
    pub rate_bytes: Vec<u8>,
    /// the rates in 500 kbit/s units (the JS's Mbit/s times 2), all and only the basic ones
    pub rates_500k: Vec<u8>,
    pub basic_rates_500k: Vec<u8>,
    pub rsn: Option<Rsn>,
    /// the RSN element as sent, with its header (what the 4-way handshake compares)
    pub rsn_ie: Option<Vec<u8>>,
    /// a WPA (vendor 00-50-F2 type 1) element is there
    pub wpa: bool,
    pub ht: Option<HtInfo>,
    pub dtim_period: Option<u8>,
    pub country: Option<[u8; 2]>,
    pub ies: Elements,
}

// Linux: cfg80211_inform_bss_frame (net/wireless/scan.c) takes these fields from a beacon or
// probe response (mpdu without FCS); `None` for anything else
pub fn beacon_parse(f: &[u8]) -> Option<Beacon> {
    if f.len() < 36 {
        return None;
    }
    let fc = le16(f, 0);
    if !is_beacon(fc) && !is_probe_resp(fc) {
        return None;
    }
    let ies = parse_ies(f, 36, f.len());
    let capability = le16(f, 34);
    let ssid_raw = ies
        .find(eid::SSID)
        .map(|e| e.data.clone())
        .unwrap_or_default();
    let mut rate_bytes = Vec::new();
    for id in [eid::SUPP_RATES, eid::EXT_SUPP_RATES] {
        if let Some(e) = ies.find(id) {
            rate_bytes.extend_from_slice(&e.data);
        }
    }
    let ht_cap = ies.find(eid::HT_CAP);
    let ht_op = ies.find(eid::HT_OPERATION).filter(|e| e.data.len() >= 22);
    let ht = (ht_cap.is_some() || ies.find(eid::HT_OPERATION).is_some()).then(|| HtInfo {
        cap_info: ht_cap
            .filter(|e| e.data.len() >= 2)
            .map(|e| le16(&e.data, 0)),
        primary_ch: ht_op.map(|e| e.data[0]),
        secondary_offset: ht_op.map(|e| e.data[1] & 3),
    });
    let rsn_el = ies.find(eid::RSN);
    let wpa = ies
        .list
        .iter()
        .any(|e| e.id == eid::VENDOR && e.data.len() >= 4 && be32(&e.data, 0) == 0x0050_f201);
    let mut tsf = [0u8; 8];
    tsf.copy_from_slice(&f[24..32]);
    Some(Beacon {
        beacon: is_beacon(fc),
        bssid: mac_at(f, 16),
        sa: mac_at(f, 10),
        tsf: u64::from_le_bytes(tsf),
        beacon_int: le16(f, 32),
        capability,
        privacy: capability & cap::PRIVACY != 0,
        ssid: String::from_utf8_lossy(&ssid_raw).into_owned(),
        ssid_raw,
        ch: ies_channel(&ies),
        rates_500k: rate_bytes.iter().map(|b| b & 0x7f).collect(),
        basic_rates_500k: rate_bytes
            .iter()
            .filter(|&&b| b & 0x80 != 0)
            .map(|b| b & 0x7f)
            .collect(),
        rate_bytes,
        rsn: rsn_el.and_then(|e| rsn_parse(&e.data)),
        rsn_ie: rsn_el.map(|e| elem(e.id, &e.data)),
        wpa,
        ht,
        dtim_period: ies
            .find(eid::TIM)
            .filter(|e| e.data.len() >= 2)
            .map(|e| e.data[1]),
        country: ies
            .find(eid::COUNTRY)
            .filter(|e| e.data.len() >= 2)
            .map(|e| [e.data[0], e.data[1]]),
        ies,
    })
}

/// What a management frame from the AP says (the body ieee80211_rx_mgmt_* reads)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MgmtKind {
    Auth {
        alg: u16,
        auth_seq: u16,
        status: u16,
    },
    AssocResp {
        capability: u16,
        status: u16,
        aid: u16,
        ies: Elements,
    },
    Deauth {
        reason: u16,
    },
    Disassoc {
        reason: u16,
    },
    Other,
}

/// A management frame: its header and what its body says
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mgmt {
    pub hdr: Hdr,
    pub kind: MgmtKind,
}

// Linux: the struct ieee80211_mgmt bodies ieee80211_rx_mgmt_auth / _assoc_resp / _deauth read
// (net/mac80211/mlme.c): authentication, (re)association response, deauthentication and
// disassociation; `None` for a frame shorter than 24 bytes or not a management frame
pub fn mgmt_parse(f: &[u8]) -> Option<Mgmt> {
    if f.len() < 24 {
        return None;
    }
    let h = hdr_parse(f)?;
    if h.typ != T_MGMT {
        return None;
    }
    let b = f.get(h.hdr_len..).unwrap_or(&[]);
    let kind = match h.stype {
        st::AUTH if b.len() >= 6 => MgmtKind::Auth {
            alg: le16(b, 0),
            auth_seq: le16(b, 2),
            status: le16(b, 4),
        },
        st::ASSOC_RESP | st::REASSOC_RESP if b.len() >= 6 => MgmtKind::AssocResp {
            capability: le16(b, 0),
            status: le16(b, 2),
            aid: le16(b, 4) & 0x3fff,
            ies: parse_ies(b, 6, b.len()),
        },
        st::DEAUTH if b.len() >= 2 => MgmtKind::Deauth { reason: le16(b, 0) },
        st::DISASSOC if b.len() >= 2 => MgmtKind::Disassoc { reason: le16(b, 0) },
        _ => MgmtKind::Other,
    };
    Some(Mgmt { hdr: h, kind })
}
