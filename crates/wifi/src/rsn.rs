//! The RSN element (IEEE 802.11-2020 9.4.2.24): ours for the association request and the
//! handshake, the AP's from its beacon, and the check whether BitsOS can join that network.
//! Chip independent; rtw89's txrx has the same parser for its own tests (WL-2), this one is the
//! station's.

use alloc::vec::Vec;

/// Element ID of the RSN element
pub const EID_RSN: u8 = 48;

const fn suite(t: u32) -> u32 {
    0x000f_ac00 | t
}

/// Cipher suites (802.11-2020 Table 9-149)
pub mod cipher {
    use super::suite;
    pub const WEP40: u32 = suite(1);
    pub const TKIP: u32 = suite(2);
    pub const CCMP: u32 = suite(4);
    pub const WEP104: u32 = suite(5);
    pub const BIP_CMAC_128: u32 = suite(6);
    pub const GCMP: u32 = suite(8);
}

/// AKM suites (802.11-2020 Table 9-151)
pub mod akm {
    use super::suite;
    pub const IEEE8021X: u32 = suite(1);
    pub const PSK: u32 = suite(2);
    pub const PSK_SHA256: u32 = suite(6);
    pub const SAE: u32 = suite(8);
}

/// RSN capabilities
pub mod cap {
    pub const MFPR: u16 = 0x0040;
    pub const MFPC: u16 = 0x0080;
}

/// The RSN element body as wpa_supplicant's wpa_parse_wpa_ie_rsn reads it
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rsn {
    pub version: u16,
    pub group: u32,
    pub pairwise: Vec<u32>,
    pub akms: Vec<u32>,
    pub caps: u16,
    /// why the parse stopped: a version other than 1, or a suite list running past the element
    pub bad: Option<&'static str>,
    pub mfpc: bool,
    pub mfpr: bool,
}

fn le16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}
fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

// Linux: none (hostap wpa_parse_wpa_ie_rsn, wpa_common.c): the body of an RSN element; omitted
// trailing fields take the defaults (CCMP group and pairwise, AKM 00-0F-AC:1). `None` without a
// version field. Every read is bounds-checked: a beacon from the air never panics.
pub fn parse(body: &[u8]) -> Option<Rsn> {
    if body.len() < 2 {
        return None;
    }
    let mut r = Rsn {
        version: le16(body, 0),
        group: cipher::CCMP,
        pairwise: alloc::vec![cipher::CCMP],
        akms: alloc::vec![akm::IEEE8021X],
        caps: 0,
        bad: None,
        mfpc: false,
        mfpr: false,
    };
    if r.version != 1 {
        r.bad = Some("version");
        return Some(r);
    }
    let mut p = 2;
    let list = |p: &mut usize| -> Result<Option<Vec<u32>>, ()> {
        if *p + 2 > body.len() {
            return Ok(None);
        }
        let n = usize::from(le16(body, *p));
        *p += 2;
        if *p + 4 * n > body.len() {
            return Err(());
        }
        let out = (0..n).map(|i| be32(body, *p + 4 * i)).collect();
        *p += 4 * n;
        Ok(Some(out))
    };
    let res = (|| -> Result<(), ()> {
        if p + 4 <= body.len() {
            r.group = be32(body, p);
            p += 4;
        }
        if let Some(pw) = list(&mut p)? {
            r.pairwise = pw;
        }
        if let Some(ak) = list(&mut p)? {
            r.akms = ak;
        }
        if p + 2 <= body.len() {
            r.caps = le16(body, p);
        }
        Ok(())
    })();
    if res.is_err() {
        r.bad = Some("RSN list truncated");
    }
    r.mfpc = r.caps & cap::MFPC != 0;
    r.mfpr = r.caps & cap::MFPR != 0;
    Some(r)
}

/// An element from its header: the body of an RSN element (`None` if `ie` is not one, or torn)
pub fn element_body(ie: &[u8]) -> Option<&[u8]> {
    if ie.len() < 2 || ie[0] != EID_RSN || usize::from(ie[1]) + 2 != ie.len() {
        return None;
    }
    Some(&ie[2..])
}

// Linux: none (hostap wpa_gen_wpa_ie_rsn; V1 wpa_rsn_ccmp_psk, sta-mlme.mjs ownRsnIe): ours for
// WPA2-PSK: version 1, the AP's group cipher, CCMP pairwise, AKM PSK, capabilities 0 (no PMF)
pub fn own_ie(group: u32) -> Vec<u8> {
    let mut b = Vec::with_capacity(22);
    b.extend_from_slice(&[EID_RSN, 20]);
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&group.to_be_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&cipher::CCMP.to_be_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&akm::PSK.to_be_bytes());
    b.extend_from_slice(&0u16.to_le_bytes());
    b
}

/// Why BitsOS does not join a BSS
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unusable {
    /// open or WPA1 network, or an RSN element BitsOS cannot read
    NoRsn,
    /// no WPA2-PSK (AKM 2)
    NoPsk,
    /// no CCMP pairwise cipher
    NoCcmp,
    /// the group cipher is not CCMP (8852B: no hardware TKIP, rtw89_cam_sec_key_add cam.c)
    GroupNotCcmp,
    /// the AP requires PMF (802.11w): not ported (spec/wlan.md §2.4.7)
    PmfRequired,
}

impl Unusable {
    pub fn as_str(self) -> &'static str {
        match self {
            Unusable::NoRsn => "no usable RSN element (open or WPA1 network)",
            Unusable::NoPsk => "no WPA2-PSK (AKM 2)",
            Unusable::NoCcmp => "no CCMP pairwise cipher",
            Unusable::GroupNotCcmp => "group cipher is not CCMP (8852B: no hardware TKIP)",
            Unusable::PmfRequired => "the AP requires PMF",
        }
    }
}

impl core::fmt::Display for Unusable {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

// Linux: rtw89_cam_sec_key_add (cam.c) refuses TKIP (hw_tkip_crypto false); the rest is BitsOS's
// choice (sta-mlme.mjs bssUsable): WPA2-PSK, CCMP pairwise and group, PMF not required
pub fn usable(rsn: Option<&Rsn>) -> Result<(), Unusable> {
    let Some(r) = rsn.filter(|r| r.bad.is_none()) else {
        return Err(Unusable::NoRsn);
    };
    if !r.akms.contains(&akm::PSK) {
        return Err(Unusable::NoPsk);
    }
    if !r.pairwise.contains(&cipher::CCMP) {
        return Err(Unusable::NoCcmp);
    }
    if r.group != cipher::CCMP {
        return Err(Unusable::GroupNotCcmp);
    }
    if r.mfpr {
        return Err(Unusable::PmfRequired);
    }
    Ok(())
}
