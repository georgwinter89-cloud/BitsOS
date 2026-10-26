//! 802.11 management frames for one station (JS js/txrx-ieee.mjs): header helpers, the
//! frames mac80211 builds for a managed interface (probe request, open authentication,
//! association request, deauthentication, null data) and the element helpers. The parsers of
//! what the AP sends are in `parse.rs`. Layouts: IEEE 802.11-2020 clause 9; the choices
//! mac80211 makes are named where they matter. mac80211 is not in the local Linux tree, so
//! its function names below are references, not line cites.

use alloc::string::String;
use alloc::vec::Vec;

use crate::phy::Band;
use crate::tables::txrx::{BITRATES, RATES_5G};

/// A MAC address
pub type Mac = [u8; 6];

pub const T_MGMT: u16 = 0;
pub const T_CTL: u16 = 1;
pub const T_DATA: u16 = 2;

/// Frame subtypes (management, then data)
pub mod st {
    pub const ASSOC_REQ: u16 = 0;
    pub const ASSOC_RESP: u16 = 1;
    pub const REASSOC_RESP: u16 = 3;
    pub const PROBE_REQ: u16 = 4;
    pub const PROBE_RESP: u16 = 5;
    pub const BEACON: u16 = 8;
    pub const DISASSOC: u16 = 10;
    pub const AUTH: u16 = 11;
    pub const DEAUTH: u16 = 12;
    pub const DATA: u16 = 0;
    pub const NULLFUNC: u16 = 4;
    pub const QOS_DATA: u16 = 8;
    pub const QOS_NULLFUNC: u16 = 12;
}

/// Frame control flags
pub mod fctl {
    pub const TODS: u16 = 0x0100;
    pub const FROMDS: u16 = 0x0200;
    pub const MOREFRAGS: u16 = 0x0400;
    pub const RETRY: u16 = 0x0800;
    pub const PM: u16 = 0x1000;
    pub const MOREDATA: u16 = 0x2000;
    pub const PROTECTED: u16 = 0x4000;
    pub const ORDER: u16 = 0x8000;
}

/// Element IDs
pub mod eid {
    pub const SSID: u8 = 0;
    pub const SUPP_RATES: u8 = 1;
    pub const DS_PARAMS: u8 = 3;
    pub const TIM: u8 = 5;
    pub const COUNTRY: u8 = 7;
    pub const ERP: u8 = 42;
    pub const HT_CAP: u8 = 45;
    pub const RSN: u8 = 48;
    pub const EXT_SUPP_RATES: u8 = 50;
    pub const HT_OPERATION: u8 = 61;
    pub const EXT_CAP: u8 = 127;
    pub const VENDOR: u8 = 221;
}

/// Capability information bits
pub mod cap {
    pub const ESS: u16 = 0x0001;
    pub const PRIVACY: u16 = 0x0010;
    pub const SHORT_PREAMBLE: u16 = 0x0020;
    pub const SPECTRUM_MGMT: u16 = 0x0100;
    pub const SHORT_SLOT_TIME: u16 = 0x0400;
}

// RSN suite selectors (OUI 00-0F-AC, 802.11-2020 Tables 9-149 and 9-151)
const fn suite(t: u32) -> u32 {
    0x000f_ac00 | t
}

/// Cipher suites
pub mod cipher {
    use super::suite;
    pub const USE_GROUP: u32 = suite(0);
    pub const WEP40: u32 = suite(1);
    pub const TKIP: u32 = suite(2);
    pub const CCMP: u32 = suite(4);
    pub const WEP104: u32 = suite(5);
    pub const BIP_CMAC_128: u32 = suite(6);
    pub const GCMP: u32 = suite(8);
    pub const GCMP_256: u32 = suite(9);
    pub const CCMP_256: u32 = suite(10);
    pub const BIP_GMAC_128: u32 = suite(11);
    pub const BIP_GMAC_256: u32 = suite(12);
    pub const BIP_CMAC_256: u32 = suite(13);
}

/// AKM suites
pub mod akm {
    use super::suite;
    pub const IEEE8021X: u32 = suite(1);
    pub const PSK: u32 = suite(2);
    pub const FT_8021X: u32 = suite(3);
    pub const FT_PSK: u32 = suite(4);
    pub const IEEE8021X_SHA256: u32 = suite(5);
    pub const PSK_SHA256: u32 = suite(6);
    pub const SAE: u32 = suite(8);
    pub const FT_SAE: u32 = suite(9);
    pub const OWE: u32 = suite(18);
    pub const SAE_EXT_KEY: u32 = suite(24);
}

/// RSN capabilities
pub mod rsn_cap {
    pub const PREAUTH: u16 = 0x0001;
    pub const MFPR: u16 = 0x0040;
    pub const MFPC: u16 = 0x0080;
}

pub const BCAST: Mac = [0xff; 6];
pub const WLAN_REASON_DEAUTH_LEAVING: u16 = 3;

// Linux: rtw89_sband_2ghz / rtw89_sband_5ghz bitrates (core.c:297, 309): rtw89_bitrates
// (core.c:153) in 100 kbit/s; the 5 GHz band starts at the first OFDM rate
pub fn band_rates(band: Band) -> &'static [u16] {
    match band {
        Band::B2g => BITRATES,
        _ => &BITRATES[RATES_5G..],
    }
}

// ---- frame control and header (include/linux/ieee80211.h, not in the local tree) ----

/// The type field of ieee80211_is_mgmt/_ctl/_data
pub const fn fc_type(fc: u16) -> u16 {
    (fc >> 2) & 3
}
/// The subtype field, IEEE80211_FCTL_STYPE
pub const fn fc_stype(fc: u16) -> u16 {
    (fc >> 4) & 0xf
}
// Linux: ieee80211_is_mgmt (include/linux/ieee80211.h)
pub const fn is_mgmt(fc: u16) -> bool {
    fc_type(fc) == T_MGMT
}
// Linux: ieee80211_is_ctl
pub const fn is_ctl(fc: u16) -> bool {
    fc_type(fc) == T_CTL
}
// Linux: ieee80211_is_data
pub const fn is_data(fc: u16) -> bool {
    fc_type(fc) == T_DATA
}
// Linux: ieee80211_is_data_qos
pub const fn is_data_qos(fc: u16) -> bool {
    is_data(fc) && fc_stype(fc) & 0x8 != 0
}
// Linux: ieee80211_is_nullfunc
pub const fn is_nullfunc(fc: u16) -> bool {
    is_data(fc) && fc_stype(fc) == st::NULLFUNC
}
// Linux: ieee80211_is_qos_nullfunc
pub const fn is_qos_nullfunc(fc: u16) -> bool {
    is_data(fc) && fc_stype(fc) == st::QOS_NULLFUNC
}
// Linux: ieee80211_is_any_nullfunc
pub const fn is_any_nullfunc(fc: u16) -> bool {
    is_nullfunc(fc) || is_qos_nullfunc(fc)
}
// Linux: ieee80211_is_beacon
pub const fn is_beacon(fc: u16) -> bool {
    is_mgmt(fc) && fc_stype(fc) == st::BEACON
}
// Linux: ieee80211_is_probe_resp
pub const fn is_probe_resp(fc: u16) -> bool {
    is_mgmt(fc) && fc_stype(fc) == st::PROBE_RESP
}
/// cpu_to_le16(IEEE80211_FTYPE_x | IEEE80211_STYPE_y | flags) as mac80211 writes it
pub const fn fc_of(typ: u16, stype: u16, flags: u16) -> u16 {
    (stype << 4) | (typ << 2) | flags
}

// Linux: ieee80211_hdrlen (net/wireless/util.c)
pub const fn hdr_len(fc: u16) -> usize {
    if is_data(fc) {
        let mut n = if fc & fctl::TODS != 0 && fc & fctl::FROMDS != 0 {
            30
        } else {
            24
        };
        if is_data_qos(fc) {
            n += 2;
            if fc & fctl::ORDER != 0 {
                n += 4;
            }
        }
        return n;
    }
    if is_mgmt(fc) {
        return if fc & fctl::ORDER != 0 { 28 } else { 24 };
    }
    if is_ctl(fc) {
        return if fc & 0x00e0 == 0x00c0 { 10 } else { 16 }; // CTS and ACK are short
    }
    24
}

/// A MAC address from text (`aa:bb:cc:dd:ee:ff` or with `-`); `None` unless six hex bytes
pub fn parse_mac(s: &str) -> Option<Mac> {
    let mut out = [0u8; 6];
    let mut n = 0;
    for part in s.split([':', '-']) {
        if n == 6 {
            return None;
        }
        out[n] = u8::from_str_radix(part, 16).ok()?;
        n += 1;
    }
    (n == 6).then_some(out)
}

/// The %pM format (lib/vsprintf.c)
pub fn mac_str(b: &[u8]) -> String {
    use core::fmt::Write;
    let mut s = String::new();
    for (i, v) in b.iter().enumerate() {
        if i > 0 {
            s.push(':');
        }
        let _ = write!(s, "{v:02x}");
    }
    s
}

/// A MAC address out of a frame (the caller checked the length)
pub(crate) fn mac_at(f: &[u8], at: usize) -> Mac {
    let mut m = [0u8; 6];
    m.copy_from_slice(&f[at..at + 6]);
    m
}

pub(crate) fn le16(f: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([f[at], f[at + 1]])
}

// Linux: struct ieee80211_hdr_3addr as mac80211 fills it: frame control, duration, three
// addresses, sequence control. Duration stays 0 (mac80211's ieee80211_duration is not ported).
pub fn hdr(fc: u16, a1: &Mac, a2: &Mac, a3: &Mac, seq: u16) -> Vec<u8> {
    let mut h = Vec::with_capacity(24);
    h.extend_from_slice(&fc.to_le_bytes());
    h.extend_from_slice(&[0, 0]);
    h.extend_from_slice(a1);
    h.extend_from_slice(a2);
    h.extend_from_slice(a3);
    h.extend_from_slice(&((seq << 4) & 0xfff0).to_le_bytes());
    h
}

/// The struct ieee80211_hdr fields with ieee80211_get_tid
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hdr {
    pub fc: u16,
    pub typ: u16,
    pub stype: u16,
    pub to_ds: bool,
    pub from_ds: bool,
    pub retry: bool,
    pub pm: bool,
    pub protected: bool,
    pub hdr_len: usize,
    pub dur: u16,
    /// `None` when the frame ends inside it
    pub addr1: Option<Mac>,
    /// addr2, addr3, seq and frag need the 24-byte header
    pub addr2: Option<Mac>,
    pub addr3: Option<Mac>,
    pub seq: Option<u16>,
    pub frag: Option<u16>,
    /// the frame is shorter than its header length
    pub short: bool,
    pub addr4: Option<Mac>,
    /// the QoS control field and its TID (mask 0xf, IEEE80211_QOS_CTL_TID_MASK)
    pub qos: Option<u16>,
    pub tid: Option<u16>,
}

// Linux: the struct ieee80211_hdr fields (include/linux/ieee80211.h) with ieee80211_get_tid;
// `None` when not even frame control and duration are there
pub fn hdr_parse(f: &[u8]) -> Option<Hdr> {
    if f.len() < 4 {
        return None;
    }
    let fc = le16(f, 0);
    let n = hdr_len(fc);
    let mut h = Hdr {
        fc,
        typ: fc_type(fc),
        stype: fc_stype(fc),
        to_ds: fc & fctl::TODS != 0,
        from_ds: fc & fctl::FROMDS != 0,
        retry: fc & fctl::RETRY != 0,
        pm: fc & fctl::PM != 0,
        protected: fc & fctl::PROTECTED != 0,
        hdr_len: n,
        dur: le16(f, 2),
        addr1: (f.len() >= 10).then(|| mac_at(f, 4)),
        addr2: None,
        addr3: None,
        seq: None,
        frag: None,
        short: false,
        addr4: None,
        qos: None,
        tid: None,
    };
    if f.len() < 24 {
        return Some(h);
    }
    h.addr2 = Some(mac_at(f, 10));
    h.addr3 = Some(mac_at(f, 16));
    let sc = le16(f, 22);
    h.seq = Some(sc >> 4);
    h.frag = Some(sc & 0xf);
    if f.len() < n {
        h.short = true;
        return Some(h);
    }
    if n >= 30 && is_data(fc) && h.to_ds && h.from_ds {
        h.addr4 = Some(mac_at(f, 24));
    }
    if is_data_qos(fc) {
        let q = le16(f, n - if fc & fctl::ORDER != 0 { 6 } else { 2 });
        h.qos = Some(q);
        h.tid = Some(q & 0xf);
    }
    Some(h)
}

// ---- elements ----

/// struct element (include/linux/ieee80211.h): id, length, data. The length byte is the
/// data's length cut to 8 bits, as the JS driver's Buffer.from([id, len]) writes it.
pub fn elem(id: u8, data: &[u8]) -> Vec<u8> {
    let mut e = Vec::with_capacity(2 + data.len());
    e.push(id);
    e.push(data.len() as u8);
    e.extend_from_slice(data);
    e
}

/// One element of a frame body
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Element {
    pub id: u8,
    pub data: Vec<u8>,
}

/// The elements of a frame body; `truncated`: the last one ran past the end and was left out
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Elements {
    pub list: Vec<Element>,
    pub truncated: bool,
}

impl Elements {
    /// The first element with this ID (the JS `findIe`)
    pub fn find(&self, id: u8) -> Option<&Element> {
        self.list.iter().find(|e| e.id == id)
    }
}

// Linux: for_each_element (include/linux/ieee80211.h) over buf[off..end], stopping at a
// truncated one
pub fn parse_ies(buf: &[u8], off: usize, end: usize) -> Elements {
    let end = end.min(buf.len());
    let mut out = Elements::default();
    let mut off = off;
    while off + 2 <= end {
        let (id, len) = (buf[off], usize::from(buf[off + 1]));
        if off + 2 + len > end {
            out.truncated = true;
            break;
        }
        out.list.push(Element {
            id,
            data: buf[off + 2..off + 2 + len].to_vec(),
        });
        off += 2 + len;
    }
    out
}

// Linux: ieee80211_put_srates_elem (net/mac80211/util.c): Supported Rates (at most 8) and
// Extended Supported Rates; rates in 100 kbit/s as in rtw89_bitrates, written in 500 kbit/s
// units, basic ones with bit 7
pub fn rates_ies(bitrates: &[u16], basic: &[u16]) -> Vec<u8> {
    let bytes: Vec<u8> = bitrates
        .iter()
        .map(|&r| (r / 5) as u8 | if basic.contains(&r) { 0x80 } else { 0 })
        .collect();
    let mut out = elem(eid::SUPP_RATES, &bytes[..bytes.len().min(8)]);
    if bytes.len() > 8 {
        out.extend(elem(eid::EXT_SUPP_RATES, &bytes[8..]));
    }
    out
}

// Linux: ieee80211_get_rates (net/mac80211/mlme.c): AP rate bytes -> bitmap over the band's
// bitrates (sband index), all or only the basic ones (bss_conf->basic_rates,
// link_sta->supp_rates for the TX info)
pub fn rate_bitmap(rate_bytes: &[u8], band: Band, basic_only: bool) -> u32 {
    let tab = band_rates(band);
    let mut map = 0;
    for &b in rate_bytes {
        if basic_only && b & 0x80 == 0 {
            continue;
        }
        let r = u16::from(b & 0x7f) * 5;
        if let Some(i) = tab.iter().position(|&x| x == r) {
            map |= 1 << i;
        }
    }
    map
}

/// What our RSN element says (wpa_gen_wpa_ie_rsn's choices for WPA2-PSK by default)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RsnIeReq<'a> {
    pub group: u32,
    pub pairwise: &'a [u32],
    pub akms: &'a [u32],
    pub caps: u16,
}

impl Default for RsnIeReq<'_> {
    fn default() -> Self {
        RsnIeReq {
            group: cipher::CCMP,
            pairwise: &[cipher::CCMP],
            akms: &[akm::PSK],
            caps: 0,
        }
    }
}

// Linux: none (wpa_supplicant's wpa_gen_wpa_ie_rsn builds it and hands it to the kernel):
// the RSN element of the association request: version, group, the pairwise list, the AKM
// list, capabilities (0: no PMF)
pub fn rsn_ie(r: &RsnIeReq) -> Vec<u8> {
    let mut b = Vec::with_capacity(16 + 4 * (r.pairwise.len() + r.akms.len()));
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&r.group.to_be_bytes());
    b.extend_from_slice(&(r.pairwise.len() as u16).to_le_bytes());
    for s in r.pairwise {
        b.extend_from_slice(&s.to_be_bytes());
    }
    b.extend_from_slice(&(r.akms.len() as u16).to_le_bytes());
    for s in r.akms {
        b.extend_from_slice(&s.to_be_bytes());
    }
    b.extend_from_slice(&r.caps.to_le_bytes());
    elem(eid::RSN, &b)
}

// ---- frames the station sends ----

/// A probe request (defaults: wildcard SSID, 2.4 GHz, no DS element, broadcast)
#[derive(Debug, Clone)]
pub struct ProbeReq<'a> {
    pub own: Mac,
    pub ssid: &'a [u8],
    pub band: Band,
    pub ch: Option<u8>,
    pub dst: Mac,
    pub bssid: Mac,
    pub seq: u16,
}

impl ProbeReq<'_> {
    pub fn new(own: Mac) -> ProbeReq<'static> {
        ProbeReq {
            own,
            ssid: &[],
            band: Band::B2g,
            ch: None,
            dst: BCAST,
            bssid: BCAST,
            seq: 0,
        }
    }
}

// Linux: ieee80211_probereq_get (net/mac80211/tx.c) + ieee80211_build_preq_ies_band
// (net/mac80211/util.c) without HT/VHT/HE: SSID, rates, extended rates, DS parameter set on
// 2.4 GHz
pub fn probe_req(p: &ProbeReq) -> Vec<u8> {
    let mut f = hdr(
        fc_of(T_MGMT, st::PROBE_REQ, 0),
        &p.dst,
        &p.own,
        &p.bssid,
        p.seq,
    );
    f.extend(elem(eid::SSID, p.ssid));
    f.extend(rates_ies(band_rates(p.band), &[]));
    if let (Band::B2g, Some(ch)) = (p.band, p.ch.filter(|&c| c != 0)) {
        f.extend(elem(eid::DS_PARAMS, &[ch]));
    }
    f
}

// Linux: ieee80211_send_auth (net/mac80211/util.c): open system, first frame (algorithm 0,
// transaction 1, status 0)
pub fn auth_open(own: &Mac, bssid: &Mac, seq: u16) -> Vec<u8> {
    let mut f = hdr(fc_of(T_MGMT, st::AUTH, 0), bssid, own, bssid, seq);
    f.extend_from_slice(&[0, 0, 1, 0, 0, 0]);
    f
}

/// An association request (`listen_interval` 5 by default: mac80211's max_listen_interval)
#[derive(Debug, Clone)]
pub struct AssocReq<'a> {
    pub own: Mac,
    pub bssid: Mac,
    pub ssid: &'a [u8],
    pub band: Band,
    /// the AP's rate bytes (`None`: all of ours)
    pub ap_rate_bytes: Option<&'a [u8]>,
    pub ap_capability: u16,
    /// our RSN element, whole
    pub rsn: Option<&'a [u8]>,
    pub listen_interval: u16,
    pub seq: u16,
}

// Linux: ieee80211_send_assoc (net/mac80211/mlme.c) for a legacy station: capability ESS
// (+ short slot and short preamble on 2.4 GHz, + privacy when the BSS has it), the listen
// interval, SSID, the rates both sides have (all of ours when none are common), the RSN
// element. No HT, VHT, HE or WMM element, so the AP treats the link as legacy non-QoS.
pub fn assoc_req(a: &AssocReq) -> Vec<u8> {
    let mut capab = cap::ESS;
    if a.band == Band::B2g {
        capab |= cap::SHORT_SLOT_TIME | cap::SHORT_PREAMBLE;
    }
    if a.ap_capability & cap::PRIVACY != 0 {
        capab |= cap::PRIVACY;
    }
    let ours = band_rates(a.band);
    let mut rates: Vec<u16> = match a.ap_rate_bytes {
        Some(ap) => ours
            .iter()
            .copied()
            .filter(|&r| ap.iter().any(|&b| u16::from(b & 0x7f) * 5 == r))
            .collect(),
        None => ours.to_vec(),
    };
    if rates.is_empty() {
        rates = ours.to_vec();
    }
    let mut f = hdr(
        fc_of(T_MGMT, st::ASSOC_REQ, 0),
        &a.bssid,
        &a.own,
        &a.bssid,
        a.seq,
    );
    f.extend_from_slice(&capab.to_le_bytes());
    f.extend_from_slice(&a.listen_interval.to_le_bytes());
    f.extend(elem(eid::SSID, a.ssid));
    f.extend(rates_ies(&rates, &[]));
    if let Some(rsn) = a.rsn {
        f.extend_from_slice(rsn);
    }
    f
}

// Linux: ieee80211_send_deauth_disassoc (net/mac80211/mlme.c), deauthentication
pub fn deauth(own: &Mac, bssid: &Mac, reason: u16, seq: u16) -> Vec<u8> {
    let mut f = hdr(fc_of(T_MGMT, st::DEAUTH, 0), bssid, own, bssid, seq);
    f.extend_from_slice(&reason.to_le_bytes());
    f
}

// Linux: ieee80211_nullfunc_get (net/mac80211/tx.c): To DS, addr1 = addr3 = BSSID; the PM bit
// stays clear (BitsOS never sleeps, so the AP never buffers for it); `qos`: the QoS null with
// this TID
pub fn null_func(own: &Mac, bssid: &Mac, seq: u16, qos: Option<u16>) -> Vec<u8> {
    let stype = if qos.is_some() {
        st::QOS_NULLFUNC
    } else {
        st::NULLFUNC
    };
    let mut f = hdr(fc_of(T_DATA, stype, fctl::TODS), bssid, own, bssid, seq);
    if let Some(tid) = qos {
        f.extend_from_slice(&(tid & 7).to_le_bytes());
    }
    f
}
