//! The scan list: what beacons and probe responses said about each BSS (cfg80211's BSS table in
//! small), kept as sta-scan.mjs `collect` keeps it: the newest frame wins, but the best RSSI and a
//! beacon's DTIM period stay; strongest first. A `Vec`, as many BSS as the air has (R1).

use alloc::vec::Vec;

use crate::rsn::{self, Rsn, Unusable};

pub type Mac = [u8; 6];

/// The band of a channel
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Band {
    B2g,
    B5g,
}

/// One BSS as a beacon or probe response showed it (sta-mlme.mjs bssFromFrame)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bss {
    pub bssid: Mac,
    pub ssid: Vec<u8>,
    pub band: Band,
    pub ch: u8,
    /// TUs
    pub beacon_int: u16,
    pub capability: u16,
    /// the supported and extended rates as sent (500 kbit/s units, bit 7 = basic)
    pub rate_bytes: Vec<u8>,
    /// the RSN element with its header, as sent (message 3 must carry the same)
    pub rsn_ie: Option<Vec<u8>>,
    /// a beacon's TIM element (probe responses have none)
    pub dtim_period: Option<u8>,
    /// dBm; `None` without a PPDU status
    pub rssi: Option<i32>,
}

impl Bss {
    /// The parsed RSN element
    pub fn rsn(&self) -> Option<Rsn> {
        self.rsn_ie
            .as_deref()
            .and_then(rsn::element_body)
            .and_then(rsn::parse)
    }
    /// Whether BitsOS can join it (WPA2-PSK, CCMP, no PMF required)
    pub fn usable(&self) -> Result<(), Unusable> {
        rsn::usable(self.rsn().as_ref())
    }
    /// The group cipher for our RSN element (CCMP when the AP's element is unreadable)
    pub fn group_cipher(&self) -> u32 {
        self.rsn().map_or(rsn::cipher::CCMP, |r| r.group)
    }
}

/// The BSS seen in one scan
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanList {
    entries: Vec<Bss>,
}

impl ScanList {
    pub fn new() -> ScanList {
        ScanList::default()
    }

    // Linux: none (sta-scan.mjs collect; cfg80211_inform_bss_frame updates its entry): newest
    // wins, keeps the best RSSI and the DTIM period of a beacon
    pub fn add(&mut self, mut bss: Bss) {
        let Some(old) = self.entries.iter_mut().find(|b| b.bssid == bss.bssid) else {
            self.entries.push(bss);
            return;
        };
        if let Some(o) = old.rssi
            && bss.rssi.is_none_or(|n| o > n)
        {
            bss.rssi = Some(o);
        }
        if bss.dtim_period.is_none() {
            bss.dtim_period = old.dtim_period;
        }
        *old = bss;
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Strongest first (no RSSI last); equal ones in the order they were first seen
    pub fn sorted(&self) -> Vec<Bss> {
        let mut v = self.entries.clone();
        v.sort_by_key(|b| core::cmp::Reverse(b.rssi.unwrap_or(-999)));
        v
    }

    /// The BSS to join for `ssid` (bringup.mjs: the first usable one of that name, strongest
    /// first); `Err` with why the strongest one of that name is unusable, or `None` if none has it
    pub fn choose(&self, ssid: &[u8]) -> Option<Result<Bss, Unusable>> {
        let named: Vec<Bss> = self
            .sorted()
            .into_iter()
            .filter(|b| b.ssid == ssid)
            .collect();
        let first = named.first()?;
        if let Some(b) = named.iter().find(|b| b.usable().is_ok()) {
            return Some(Ok(b.clone()));
        }
        Some(Err(first.usable().err().unwrap_or(Unusable::NoRsn)))
    }
}
