//! What mac80211 checks on received frames before they go up, chip independent: the CCMP replay
//! check per TID (ieee80211_crypto_ccmp_decrypt's PN compare), the duplicate check
//! (ieee80211_rx_h_check_dup) and the data frame checks of a station (ieee80211_frame_allowed,
//! ieee80211_802_1x_port_control, ieee80211_drop_unencrypted), as sta-link.mjs runs them.

/// IEEE80211_NUM_TIDS. mac80211 keeps NUM_TIDS + 1 receive counters per key: one per TID
/// (non-QoS data shares TID 0's, security_idx 0 of ieee80211_parse_qos) and one for management
/// frames (only with PMF). 802.11's numbers, not a limit of BitsOS (R1).
pub const NUM_TIDS: usize = 16;
/// The counter index of management frames (PMF)
pub const MGMT_IDX: usize = NUM_TIDS;

/// The receive counters of one key, starting at the key's RSC from the handshake
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RxPn {
    rsc: u64,
    last: [Option<u64>; NUM_TIDS + 1],
    /// frames dropped as replays
    pub replays: u64,
}

impl RxPn {
    /// `rsc`: the receive sequence counter of the handshake (the GTK's from its frame, 0 for the PTK)
    pub fn new(rsc: u64) -> RxPn {
        RxPn {
            rsc,
            last: [None; NUM_TIDS + 1],
            replays: 0,
        }
    }

    /// The RSC field of an EAPOL-Key frame (little-endian PN, 6 bytes used)
    pub fn rsc_of(field: &[u8; 8]) -> u64 {
        let mut b = [0u8; 8];
        b[..6].copy_from_slice(&field[..6]);
        u64::from_le_bytes(b)
    }

    // Linux: the replay check of ieee80211_crypto_ccmp_decrypt (linux-6.6/net/mac80211/wpa.c:506,
    // the memcmp at :548): a PN not above the last one of its counter is a replay. True = drop. An index past the counters is
    // a replay (never one a parser gives).
    pub fn replay(&mut self, sec_idx: usize, pn: u64) -> bool {
        let Some(slot) = self.last.get_mut(sec_idx) else {
            self.replays = self.replays.saturating_add(1);
            return true;
        };
        if pn <= slot.unwrap_or(self.rsc) {
            self.replays = self.replays.saturating_add(1);
            return true;
        }
        *slot = Some(pn);
        false
    }
}

/// mac80211's last sequence control per counter of the AP's station (ieee80211_rx_h_check_dup)
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DupCheck {
    last: [Option<u16>; NUM_TIDS + 1],
    /// retransmissions dropped
    pub dups: u64,
}

/// What the duplicate check needs of a frame
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DupInfo {
    /// a control frame never counts
    pub control: bool,
    /// addr1 is a group address
    pub group: bool,
    /// a (QoS) null function frame never counts
    pub nullfunc: bool,
    pub retry: bool,
    /// QoS data: its TID; everything else counts on the 17th counter
    pub tid: Option<u8>,
    pub seq_ctrl: u16,
}

impl DupCheck {
    pub fn new() -> DupCheck {
        DupCheck::default()
    }
    // Linux: ieee80211_rx_h_check_dup (linux-6.6/net/mac80211/rx.c:1411; sta-link.mjs duplicate): a unicast
    // retransmission (retry bit, same sequence control as the last frame of its counter) from
    // our AP is dropped. True = drop. The caller has checked that the frame is from our AP.
    pub fn duplicate(&mut self, f: &DupInfo) -> bool {
        if f.control || f.group || f.nullfunc {
            return false;
        }
        let idx = f.tid.map_or(NUM_TIDS, |t| usize::from(t & 0xf));
        if f.retry && self.last[idx] == Some(f.seq_ctrl) {
            self.dups = self.dups.saturating_add(1);
            return true;
        }
        self.last[idx] = Some(f.seq_ctrl);
        false
    }
}

/// Why a data frame from our AP does not go up (sta-link.mjs ethOf)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataDrop {
    /// protected, but the chip did not decrypt it
    NotDecrypted,
    /// protected under a key ID we have no key for
    NoKey,
    /// its PN was not above the last of its counter
    Replay,
    /// not EAPOL and the port is not authorized yet
    PortClosed,
    /// unprotected although keys are in (ieee80211_drop_unencrypted)
    Unencrypted,
}

impl DataDrop {
    pub fn as_str(self) -> &'static str {
        match self {
            DataDrop::NotDecrypted => "not decrypted",
            DataDrop::NoKey => "no key for its key ID",
            DataDrop::Replay => "replay",
            DataDrop::PortClosed => "port not authorized",
            DataDrop::Unencrypted => "unencrypted with keys in",
        }
    }
}

/// A data frame from our AP as the station's checks see it
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DataInfo {
    pub protected: bool,
    pub decrypted: bool,
    pub key_id: u8,
    pub pn: u64,
    pub sec_idx: usize,
    /// EAPOL (0x888E) to our address: the control port takes it before authorization
    pub eapol_to_us: bool,
}

/// What the station knows when a data frame comes
pub trait Keys {
    /// The receive counters of the pairwise key (key ID 0) or of the group key `id`
    fn rx_pn(&mut self, key_id: u8) -> Option<&mut RxPn>;
    /// Any key installed
    fn any(&self) -> bool;
}

// Linux: none (sta-link.mjs ethOf) as mac80211 runs them: ieee80211_crypto_ccmp_decrypt
// (linux-6.6/net/mac80211/wpa.c:506), ieee80211_802_1x_port_control
// (linux-6.6/net/mac80211/rx.c:2379), ieee80211_frame_allowed (linux-6.6/net/mac80211/rx.c:2550),
// ieee80211_drop_unencrypted (linux-6.6/net/mac80211/rx.c:2387)
pub fn data_allowed(f: &DataInfo, authorized: bool, keys: &mut dyn Keys) -> Result<(), DataDrop> {
    if f.protected {
        if !f.decrypted {
            return Err(DataDrop::NotDecrypted);
        }
        let Some(pn) = keys.rx_pn(f.key_id) else {
            return Err(DataDrop::NoKey);
        };
        if pn.replay(f.sec_idx, f.pn) {
            return Err(DataDrop::Replay);
        }
    }
    if f.eapol_to_us {
        return Ok(());
    }
    if !authorized {
        return Err(DataDrop::PortClosed);
    }
    if !f.protected && keys.any() {
        return Err(DataDrop::Unencrypted);
    }
    Ok(())
}
