//! The country of the channel switch (JS js/chan-regd.mjs): the TX power regulation per band
//! from the firmware's REGD element, and the channel flags cfg80211 would hand the driver for
//! that country (wireless-regdb, outside this tree; only DE and the world rules are ported).

use crate::phy::Band;
use crate::tables::ChanRule;
use crate::tables::chan::{RULES_DE_2G, RULES_DE_5G, RULES_WW_2G, RULES_WW_5G};

use super::fw::Regd;

/// The efuse country is not programmed (FF FF), so Linux follows the network stack
/// (rtw89_regd_init_hint, regd.c:738): BitsOS runs in Germany
pub const DEFAULT_COUNTRY: [u8; 2] = *b"DE";

/// What cfg80211 tells the driver about a channel in the current country
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChanFlags {
    pub allowed: bool,
    /// IEEE80211_CHAN_NO_IR: scan passively, transmit only after the AP's beacon
    pub no_ir: bool,
    /// IEEE80211_CHAN_RADAR: DFS
    pub radar: bool,
}

// Linux: rtw89_regd_find_reg_by_name (regd.c:307) on the REGD element's map: the country's
// entry, worldwide ("00") for one the map does not have
pub fn find_regd(map: &[Regd], alpha2: [u8; 2]) -> Regd {
    map.iter()
        .find(|r| r.alpha2 == alpha2)
        .copied()
        .unwrap_or(Regd::WW)
}

// Linux: none in rtw89. cfg80211 hands the driver only the channels the country allows, with
// IEEE80211_CHAN_NO_IR / _RADAR (net/wireless/reg.c, outside this tree); mac80211 scans those
// passively and a station transmits there only after the AP's beacon. Only DE is ported, every
// other country gets the world rules ("00"); 20 MHz channels inside each rule (36, 40, ... below
// 149; 149, 153, ... from it). DE 5725-5875 MHz also covers 169/173, but the firmware's ETSI
// table has no limit there: left out (generated rules, tables::chan).
pub fn channel_flags(regd: Option<&Regd>, band: Band, ch: u32) -> ChanFlags {
    let alpha2 = regd.map_or(DEFAULT_COUNTRY, |r| r.alpha2);
    let de = alpha2 == DEFAULT_COUNTRY;
    let rules: &[ChanRule] = match (band, de) {
        (Band::B2g, true) => RULES_DE_2G,
        (Band::B5g, true) => RULES_DE_5G,
        (Band::B2g, false) => RULES_WW_2G,
        (Band::B5g, false) => RULES_WW_5G,
        (Band::B6g, _) => &[],
    };
    let on_grid = band == Band::B2g
        || if ch < 149 {
            ch.is_multiple_of(4)
        } else {
            ch % 4 == 1
        };
    let rule = rules
        .iter()
        .find(|r| on_grid && ch >= u32::from(r.first) && ch <= u32::from(r.last));
    match rule {
        Some(r) => ChanFlags {
            allowed: true,
            no_ir: r.no_ir,
            radar: r.radar,
        },
        None => ChanFlags {
            allowed: false,
            no_ir: true,
            radar: false,
        },
    }
}
