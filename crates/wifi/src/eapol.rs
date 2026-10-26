//! The station (supplicant) side of the WPA2-PSK 4-way and group key handshakes (IEEE
//! 802.11-2020 12.7.6, 12.7.7): EAPOL-Key frames of the AP in, the answer and the keys out. AKM 2
//! (PSK) with CCMP: key descriptor version 2, HMAC-SHA1-128 MICs, key data in AES key wrap.
//!
//! The semantics are V1's `wlan/eapol.asm`, `eapolmsg.asm`, `eapolkey.asm` and `keydata.asm`
//! (proven on the Mini-PC through kit/wpabits.mjs: join + DHCP on Georg's FritzBox), checked
//! against wpa_supplicant (hostap wpa.c, by function name: wpa_sm_rx_eapol,
//! wpa_supplicant_process_1_of_4/_3_of_4, wpa_supplicant_process_1_of_2). The one change: a
//! retransmitted message 3 (higher counter, the ANonce of the PTK in force, its MIC good) is
//! answered with message 4 again as wpa_supplicant does (the AP resends it when our message 4
//! was lost); V1 dropped it (BAD_STATE). The KRACK guard (CVE-2017-13077/13080): a key in force
//! is never installed again: such a message 3 brings no PTK, a GTK equal in key and length to the
//! one in force brings no GTK (its receive counter stays).
//!
//! Frame layout (12.7.2, big-endian numbers): 0 version, 1 type, 2 body length, 4 descriptor,
//! 5 key information, 7 key length, 9 replay counter, 17 nonce, 49 IV, 65 RSC, 73 reserved,
//! 81 MIC (16), 97 key data length, 99 key data.

use alloc::vec;
use alloc::vec::Vec;
use core::fmt;

use zeroize::Zeroize;

use crate::crypto::{self, Pmk, Ptk};
use crate::secret::{Secret, SecretBuf, SecretVar, ct_eq};

/// EAPOL packet type EAPOL-Key
pub const EAPOL_KEY: u8 = 3;
/// The RSN key descriptor
pub const DESC_RSN: u8 = 2;
/// The fixed part of an EAPOL-Key frame up to the key data
pub const KEY_HDR_LEN: usize = 99;
pub const OFF_INFO: usize = 5;
pub const OFF_KEY_LEN: usize = 7;
pub const OFF_REPLAY: usize = 9;
pub const OFF_NONCE: usize = 17;
pub const OFF_IV: usize = 49;
pub const OFF_RSC: usize = 65;
pub const OFF_MIC: usize = 81;
pub const OFF_DATA_LEN: usize = 97;
/// The longest element there is (802.11-2020 9.4.2.1: ID, a one-byte length, up to 255 bytes):
/// an RSN element longer than this is no element. V1 kept 64 bytes (WPA_IE_MAX) and refused
/// longer ones; an AP with several AKMs and a PMKID list may need more (R1: no BitsOS limit).
pub const IE_MAX: usize = 2 + 255;
/// The longest GTK: the longest group cipher key (GCMP-256, 32 bytes; hostap's
/// `struct wpa_gtk_data` gtk[32]). A property of the ciphers, not a count.
pub const GTK_MAX: usize = 32;

/// Key Information bits (802.11-2020 Figure 12-33)
pub mod ki {
    pub const VERSION: u16 = 7;
    /// descriptor version 2: HMAC-SHA1-128 and AES key wrap
    pub const VER_AES: u16 = 2;
    pub const PAIRWISE: u16 = 1 << 3;
    pub const INDEX: u16 = 0x30;
    pub const INSTALL: u16 = 1 << 6;
    pub const ACK: u16 = 1 << 7;
    pub const MIC: u16 = 1 << 8;
    pub const SECURE: u16 = 1 << 9;
    pub const ERROR: u16 = 1 << 10;
    pub const REQUEST: u16 = 1 << 11;
    pub const ENCRYPTED: u16 = 1 << 12;
    pub const SMK: u16 = 1 << 13;
}

/// What a frame was (V1 WPA_GOT_*)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Got {
    /// message 1: message 2 is the answer
    Msg1,
    /// message 3: message 4 is the answer; a new PTK (and GTK) unless it is a retransmission
    Msg3,
    /// group message 1: group message 2 is the answer
    Group,
}

impl Got {
    pub fn as_str(self) -> &'static str {
        match self {
            Got::Msg1 => "message 1",
            Got::Msg3 => "message 3",
            Got::Group => "group message 1",
        }
    }
}

/// Why a frame was dropped (V1 WPA_BAD_*): the state stays as it was
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bad {
    /// too short, not EAPOL-Key, lengths that do not fit
    Frame = 1,
    /// not RSN, not version 2, not from an authenticator, a bit or key length that does not fit
    Kind = 2,
    /// the replay counter did not go up
    Replay = 3,
    /// message 3 before message 1, a group message before the PTK, a closed context
    State = 4,
    /// message 3's ANonce is not message 1's
    Nonce = 5,
    Mic = 6,
    /// does not unwrap, broken elements, no GTK
    KeyData = 7,
    /// message 3's RSN element is not the beacon's (downgrade check)
    Ie = 8,
}

impl Bad {
    pub fn as_str(self) -> &'static str {
        match self {
            Bad::Frame => "frame",
            Bad::Kind => "kind",
            Bad::Replay => "replay",
            Bad::State => "state",
            Bad::Nonce => "nonce",
            Bad::Mic => "MIC",
            Bad::KeyData => "key data",
            Bad::Ie => "RSN element",
        }
    }
}

impl fmt::Display for Bad {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A group key to install: its key ID, the key and the receive sequence counter of the frame
pub struct GtkInstall {
    pub id: u8,
    pub key: SecretVar<GTK_MAX>,
    /// the RSC field as the frame had it (PN of the group key, little-endian, 6 bytes used)
    pub rsc: [u8; 8],
    /// the Tx bit of the KDE
    pub tx: bool,
}

impl fmt::Debug for GtkInstall {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "GtkInstall {{ id: {}, key: {:?}, rsc: .. }}",
            self.id, self.key
        )
    }
}

/// What a taken frame gives
#[derive(Debug)]
pub struct Accepted {
    pub got: Got,
    /// the EAPOL-Key answer from its version byte
    pub answer: Vec<u8>,
    /// the TK, when this frame put a new PTK in force (never for a retransmitted message 3)
    pub ptk: Option<Secret<16>>,
    /// the GTK, when it is new (never the one in force again)
    pub gtk: Option<GtkInstall>,
}

/// The source of the SNonce (RDRAND in the kernel, fixed bytes in tests); false: no randomness
pub trait Random {
    fn fill(&mut self, out: &mut [u8]) -> bool;
}

/// The handshake context of one link (V1 WPA_CTX): what the caller fills, the state, the keys
pub struct Supplicant {
    pmk: Pmk,
    spa: [u8; 6],
    aa: [u8; 6],
    own_ie: Vec<u8>,
    /// the AP's RSN element from its beacon; `None`: not compared
    ap_ie: Option<Vec<u8>>,
    /// this handshake's SNonce (V1 WPA_SNONCE_OK); `None`: message 1 makes one
    snonce: Option<Secret<32>>,
    /// the last counter that came with a good MIC (V1 WPA_REPLAY_OK/WPA_REPLAY)
    replay: Option<[u8; 8]>,
    anonce: [u8; 32],
    /// message 1's keys until message 3's MIC proves them
    tptk: Option<Ptk>,
    /// the keys in force; `ptk_anonce` is the ANonce they came from
    ptk: Option<Ptk>,
    ptk_anonce: [u8; 32],
    /// the group key in force, its ID and RSC
    gtk: Option<(SecretVar<GTK_MAX>, u8, [u8; 8])>,
    closed: bool,
}

impl fmt::Debug for Supplicant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Supplicant")
            .field("spa", &self.spa)
            .field("aa", &self.aa)
            .field("snonce", &self.snonce.is_some())
            .field("replay", &self.replay.is_some())
            .field("tptk", &self.tptk.is_some())
            .field("ptk", &self.ptk.is_some())
            .field("gtk", &self.gtk.as_ref().map(|g| g.1))
            .field("closed", &self.closed)
            .finish()
    }
}

/// Why a context could not be made
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CtxError {
    /// our RSN element or the beacon's longer than an element can be ([`IE_MAX`])
    IeTooLong { len: usize },
}

impl Supplicant {
    // Linux: none (V1 sta-wpa.asm ws_eapol_open + wpa_reset; hostap wpa_sm_set_own_wpa_ie,
    // wpa_sm_set_ap_rsn_ie): a context with PMK, addresses and RSN elements, no state
    pub fn new(
        pmk: Pmk,
        spa: [u8; 6],
        aa: [u8; 6],
        own_ie: &[u8],
        ap_ie: Option<&[u8]>,
    ) -> Result<Supplicant, CtxError> {
        for ie in [Some(own_ie), ap_ie].into_iter().flatten() {
            if ie.len() > IE_MAX {
                return Err(CtxError::IeTooLong { len: ie.len() });
            }
        }
        Ok(Supplicant {
            pmk,
            spa,
            aa,
            own_ie: own_ie.to_vec(),
            ap_ie: ap_ie.filter(|ie| !ie.is_empty()).map(<[u8]>::to_vec),
            snonce: None,
            replay: None,
            anonce: [0; 32],
            tptk: None,
            ptk: None,
            ptk_anonce: [0; 32],
            gtk: None,
            closed: false,
        })
    }

    // Linux: none (V1 wpa_reset): a new start, no keys, no counter, a new SNonce at the next
    // message 1 (the join's retry)
    pub fn reset(&mut self) {
        self.snonce = None;
        self.replay = None;
        self.anonce.zeroize();
        self.tptk = None;
        self.ptk = None;
        self.ptk_anonce.zeroize();
        self.gtk = None;
        self.closed = false;
    }

    /// The SNonce of the next message 1 (tests, a capture's): kept until a message 3 is taken
    pub fn set_snonce(&mut self, n: [u8; 32]) {
        self.snonce = Some(Secret::new(n));
    }

    /// The context ends with the link (CONTRACTS.md eapol.close): keys wiped, every frame refused
    pub fn close(&mut self) {
        self.reset();
        self.closed = true;
    }

    pub fn is_closed(&self) -> bool {
        self.closed
    }
    /// Whether a PTK is in force
    pub fn has_ptk(&self) -> bool {
        self.ptk.is_some()
    }
    /// The ID of the GTK in force
    pub fn gtk_id(&self) -> Option<u8> {
        self.gtk.as_ref().map(|g| g.1)
    }

    /// A digest of the whole state, keys included (tests only, feature `peer`): a dropped frame
    /// must leave it as it was (V1's tests compared the context byte for byte)
    #[cfg(any(test, feature = "peer"))]
    pub fn fingerprint(&self) -> [u8; 20] {
        let mut v = alloc::vec::Vec::new();
        let ptk = |v: &mut alloc::vec::Vec<u8>, p: &Option<Ptk>| match p {
            Some(p) => {
                v.push(1);
                v.extend_from_slice(p.kck.expose());
                v.extend_from_slice(p.kek.expose());
                v.extend_from_slice(p.tk.expose());
            }
            None => v.push(0),
        };
        match &self.snonce {
            Some(s) => {
                v.push(1);
                v.extend_from_slice(s.expose());
            }
            None => v.push(0),
        }
        v.extend_from_slice(&self.replay.map_or([0xff; 9], |r| {
            let mut b = [0u8; 9];
            b[1..].copy_from_slice(&r);
            b
        }));
        v.extend_from_slice(&self.anonce);
        ptk(&mut v, &self.tptk);
        ptk(&mut v, &self.ptk);
        v.extend_from_slice(&self.ptk_anonce);
        if let Some((k, id, rsc)) = &self.gtk {
            v.push(*id);
            v.push(k.len() as u8);
            v.extend_from_slice(k.expose());
            v.extend_from_slice(rsc);
        }
        v.push(u8::from(self.closed));
        crypto::sha1(&v)
    }

    // Linux: none (V1 wpa_receive / wpa_take; hostap wpa_sm_rx_eapol): one EAPOL frame of the AP
    // from its version byte. The checks every frame passes, then its kind; what a frame changes
    // in the context it changes only once it passed every check.
    pub fn receive(&mut self, frame: &[u8], rng: &mut dyn Random) -> Result<Accepted, Bad> {
        if self.closed {
            return Err(Bad::State);
        }
        let k = parse(frame)?;
        if k.desc != DESC_RSN {
            return Err(Bad::Kind);
        }
        let mask = ki::VERSION | ki::ACK | ki::ERROR | ki::REQUEST | ki::SMK;
        if k.info & mask != ki::VER_AES | ki::ACK {
            return Err(Bad::Kind);
        }
        if let Some(last) = self.replay
            && k.replay <= last
        {
            return Err(Bad::Replay);
        }
        if k.info & ki::PAIRWISE == 0 {
            return self.group(&k);
        }
        if k.info & ki::INDEX != 0 {
            return Err(Bad::Kind);
        }
        if k.info & (ki::MIC | ki::ENCRYPTED) != 0 {
            return self.message3(&k);
        }
        self.message1(&k, rng)
    }

    // V1 wpa_message1 (hostap wpa_supplicant_process_1_of_4): a new ANonce, so new keys (only
    // tried, the PTK in force stays) and message 2 with our SNonce and RSN element
    fn message1(&mut self, k: &KeyFrame, rng: &mut dyn Random) -> Result<Accepted, Bad> {
        let snonce = match &self.snonce {
            Some(s) => s.duplicate(),
            None => {
                let mut n = Secret::new([0u8; 32]);
                if !rng.fill(n.expose_mut()) {
                    return Err(Bad::State); // no randomness: no handshake (never a fixed SNonce)
                }
                n
            }
        };
        let mut anonce = [0u8; 32];
        anonce.copy_from_slice(k.nonce);
        let tptk = crypto::ptk(&self.pmk, &self.aa, &self.spa, &anonce, snonce.expose());
        let answer = build(
            ki::VER_AES | ki::PAIRWISE | ki::MIC,
            k.replay,
            Some(snonce.expose()),
            &self.own_ie,
            &tptk.kck,
        );
        self.anonce = anonce;
        self.snonce = Some(snonce);
        self.tptk = Some(tptk);
        Ok(Accepted {
            got: Got::Msg1,
            answer,
            ptk: None,
            gtk: None,
        })
    }

    // V1 wpa_message3 (hostap wpa_supplicant_process_3_of_4): proves the tried keys and brings
    // the GTK; message 4 answers. A retransmission of the message 3 that put the PTK in force is
    // answered again without a new PTK (the KRACK fix of wpa_supplicant).
    fn message3(&mut self, k: &KeyFrame) -> Result<Accepted, Bad> {
        let need = ki::INSTALL | ki::MIC | ki::SECURE | ki::ENCRYPTED;
        if k.info & need != need || k.key_len != 16 {
            return Err(Bad::Kind);
        }
        let retransmit = self.tptk.is_none();
        let (keys, anonce) = match (&self.tptk, &self.ptk) {
            (Some(t), _) => (t, &self.anonce),
            (None, Some(p)) => (p, &self.ptk_anonce),
            (None, None) => return Err(Bad::State),
        };
        if !ct_eq(k.nonce, anonce) {
            return Err(Bad::Nonce);
        }
        let kd = keys_in(k, keys)?;
        if let Some(ap) = &self.ap_ie
            && kd.rsn_ie.as_deref() != Some(ap.as_slice())
        {
            return Err(Bad::Ie);
        }
        let answer = build(
            ki::VER_AES | ki::PAIRWISE | ki::MIC | ki::SECURE,
            k.replay,
            None,
            &[],
            &keys.kck,
        );
        let ptk = if retransmit {
            None
        } else {
            let t = self.tptk.take();
            self.ptk = t;
            self.ptk_anonce = self.anonce;
            self.snonce = None; // the next handshake takes a new one
            self.ptk.as_ref().map(|p| p.tk.duplicate())
        };
        let gtk = self.accept(k, kd.gtk);
        Ok(Accepted {
            got: Got::Msg3,
            answer,
            ptk,
            gtk,
        })
    }

    // V1 wpa_group (hostap wpa_supplicant_process_1_of_2): a new GTK under the keys in force
    // (never the tried ones of a handshake in between); group message 2
    fn group(&mut self, k: &KeyFrame) -> Result<Accepted, Bad> {
        let mask = ki::MIC | ki::SECURE | ki::ENCRYPTED | ki::INSTALL;
        if k.info & mask != ki::MIC | ki::SECURE | ki::ENCRYPTED {
            return Err(Bad::Kind);
        }
        let Some(keys) = &self.ptk else {
            return Err(Bad::State);
        };
        let kd = keys_in(k, keys)?;
        let answer = build(
            ki::VER_AES | ki::MIC | ki::SECURE,
            k.replay,
            None,
            &[],
            &keys.kck,
        );
        let gtk = self.accept(k, kd.gtk);
        Ok(Accepted {
            got: Got::Group,
            answer,
            ptk: None,
            gtk,
        })
    }

    // V1 wpa_accept: the counter taken; the GTK only if it is not the one in force (key and
    // length as hostap, without an early exit). Installing the key in force again would set its
    // receive counter back and let old broadcasts be replayed (KRACK, CVE-2017-13080).
    fn accept(
        &mut self,
        k: &KeyFrame,
        new: Option<(SecretVar<GTK_MAX>, u8, bool)>,
    ) -> Option<GtkInstall> {
        self.replay = Some(k.replay);
        let (key, id, tx) = new?;
        if let Some((old, _, _)) = &self.gtk
            && old.ct_eq(&key)
        {
            return None;
        }
        let mut rsc = [0u8; 8];
        rsc.copy_from_slice(k.rsc);
        self.gtk = Some((key.duplicate(), id, rsc));
        Some(GtkInstall { id, key, rsc, tx })
    }
}

/// An EAPOL-Key frame's fields (views into the frame)
pub struct KeyFrame<'a> {
    pub version: u8,
    pub desc: u8,
    pub info: u16,
    pub key_len: u16,
    pub replay: [u8; 8],
    pub nonce: &'a [u8],
    pub rsc: &'a [u8],
    pub mic: &'a [u8],
    pub data: &'a [u8],
    /// the frame up to the end of its body (what the MIC covers)
    pub body: &'a [u8],
}

fn be16(f: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([f[at], f[at + 1]])
}

// Linux: none (V1 wpa_take's first checks; hostap wpa_sm_rx_eapol): the frame's fields, or
// BAD_FRAME when it is too short, no EAPOL-Key, or its lengths do not fit (more frame than body
// is padding)
pub fn parse(frame: &[u8]) -> Result<KeyFrame<'_>, Bad> {
    if frame.len() < KEY_HDR_LEN || frame[1] != EAPOL_KEY {
        return Err(Bad::Frame);
    }
    let body = usize::from(be16(frame, 2));
    if body < KEY_HDR_LEN - 4 || body + 4 > frame.len() {
        return Err(Bad::Frame);
    }
    let data_len = usize::from(be16(frame, OFF_DATA_LEN));
    if KEY_HDR_LEN + data_len > body + 4 {
        return Err(Bad::Frame);
    }
    let mut replay = [0u8; 8];
    replay.copy_from_slice(&frame[OFF_REPLAY..OFF_REPLAY + 8]);
    Ok(KeyFrame {
        version: frame[0],
        desc: frame[4],
        info: be16(frame, OFF_INFO),
        key_len: be16(frame, OFF_KEY_LEN),
        replay,
        nonce: &frame[OFF_NONCE..OFF_NONCE + 32],
        rsc: &frame[OFF_RSC..OFF_RSC + 8],
        mic: &frame[OFF_MIC..OFF_MIC + 16],
        data: &frame[KEY_HDR_LEN..KEY_HDR_LEN + data_len],
        body: &frame[..body + 4],
    })
}

/// The fields of an EAPOL-Key frame to build (the authenticator's and the supplicant's)
pub struct KeyFields<'a> {
    pub version: u8,
    pub info: u16,
    pub key_len: u16,
    pub replay: [u8; 8],
    pub nonce: Option<&'a [u8; 32]>,
    pub rsc: [u8; 8],
    pub data: &'a [u8],
}

// Linux: none (hostap wpa_eapol_key_send / V1 wpa_answer): an EAPOL-Key frame; the MIC (if
// `kck`) over the frame with its MIC field zero
pub fn build_frame(f: &KeyFields, kck: Option<&Secret<16>>) -> Vec<u8> {
    let mut b = vec![0u8; KEY_HDR_LEN + f.data.len()];
    b[0] = f.version;
    b[1] = EAPOL_KEY;
    let body = (KEY_HDR_LEN - 4 + f.data.len()) as u16;
    b[2..4].copy_from_slice(&body.to_be_bytes());
    b[4] = DESC_RSN;
    b[OFF_INFO..OFF_INFO + 2].copy_from_slice(&f.info.to_be_bytes());
    b[OFF_KEY_LEN..OFF_KEY_LEN + 2].copy_from_slice(&f.key_len.to_be_bytes());
    b[OFF_REPLAY..OFF_REPLAY + 8].copy_from_slice(&f.replay);
    if let Some(n) = f.nonce {
        b[OFF_NONCE..OFF_NONCE + 32].copy_from_slice(n);
    }
    b[OFF_RSC..OFF_RSC + 8].copy_from_slice(&f.rsc);
    b[OFF_DATA_LEN..OFF_DATA_LEN + 2].copy_from_slice(&(f.data.len() as u16).to_be_bytes());
    b[KEY_HDR_LEN..].copy_from_slice(f.data);
    if let Some(kck) = kck {
        let m = crypto::eapol_mic(kck, &b);
        b[OFF_MIC..OFF_MIC + 16].copy_from_slice(&m);
    }
    b
}

// V1 wpa_answer: version 1 as wpa_supplicant sends (eapol_version=1), key length 0, the counter
// of the frame answered, our nonce or zeros, the key data, the MIC under the keys given
fn build(
    info: u16,
    replay: [u8; 8],
    nonce: Option<&[u8; 32]>,
    data: &[u8],
    kck: &Secret<16>,
) -> Vec<u8> {
    build_frame(
        &KeyFields {
            version: 1,
            info,
            key_len: 0,
            replay,
            nonce,
            rsc: [0; 8],
            data,
        },
        Some(kck),
    )
}

/// What the key data of message 3 / group message 1 holds
struct KeyData {
    rsn_ie: Option<Vec<u8>>,
    gtk: Option<(SecretVar<GTK_MAX>, u8, bool)>,
}

// V1 wpa_keys_in: the MIC checked (constant time), the key data unwrapped and read
fn keys_in(k: &KeyFrame, keys: &Ptk) -> Result<KeyData, Bad> {
    let mut z = k.body.to_vec();
    z[OFF_MIC..OFF_MIC + 16].fill(0);
    let mic = crypto::eapol_mic(&keys.kck, &z);
    if !ct_eq(&mic, k.mic) {
        return Err(Bad::Mic);
    }
    // No bound of our own (V1 had 256 bytes, WPA_KEYDATA_MAX): the frame's body length bounds
    // the key data (`parse`), as wpa_supplicant's wpa_sm_rx_eapol checks key_data_len against
    // the frame only; the MIC is already good here
    if k.data.len() < 24 || !k.data.len().is_multiple_of(8) {
        return Err(Bad::KeyData);
    }
    let mut plain = SecretBuf::zeroed(k.data.len() - 8);
    if !crypto::aes_unwrap(&keys.kek, k.data, plain.expose_mut()) {
        return Err(Bad::KeyData);
    }
    elements(plain.expose())
}

/// The GTK KDE: 00-0F-AC type 1 (802.11-2020 Table 12-9)
const KDE_OUI: [u8; 3] = [0x00, 0x0f, 0xac];
const KDE_GTK: u8 = 1;

// V1 wpa_elements (hostap wpa_supplicant_parse_ies): elements up to the padding (0xDD 0), the RSN
// element of the AP and the GTK KDE (dd, length, 00-0F-AC, 1, key ID, 0, GTK); a single stray
// last byte must be the padding's 0xDD. Bad: an element past the end, a GTK over 32 bytes, none.
fn elements(data: &[u8]) -> Result<KeyData, Bad> {
    let mut out = KeyData {
        rsn_ie: None,
        gtk: None,
    };
    let mut p = 0;
    while p < data.len() {
        let id = data[p];
        let Some(&len) = data.get(p + 1) else {
            if id != 0xdd {
                return Err(Bad::KeyData);
            }
            break;
        };
        let len = usize::from(len);
        if id == 0xdd && len == 0 {
            break; // the padding: the rest does not count
        }
        let end = p + 2 + len;
        if end > data.len() {
            return Err(Bad::KeyData);
        }
        let body = &data[p + 2..end];
        if id == 0x30 {
            out.rsn_ie = Some(data[p..end].to_vec());
        } else if id == 0xdd && len >= 7 && body[..3] == KDE_OUI && body[3] == KDE_GTK {
            let key = SecretVar::from_slice(&body[6..]).ok_or(Bad::KeyData)?;
            out.gtk = Some((key, body[4] & 3, body[4] & 4 != 0));
        }
        p = end;
    }
    if out.gtk.is_none() {
        return Err(Bad::KeyData);
    }
    Ok(out)
}

/// Key data padded as 12.7.2 j): 0xDD then zeros to a multiple of 8, at least 16 bytes (for the
/// authenticator side)
pub fn pad_key_data(mut d: Vec<u8>) -> Vec<u8> {
    if d.len() < 16 || !d.len().is_multiple_of(8) {
        d.push(0xdd);
        while d.len() < 16 || !d.len().is_multiple_of(8) {
            d.push(0);
        }
    }
    d
}

/// The GTK KDE for key data (for the authenticator side)
pub fn gtk_kde(gtk: &[u8], id: u8, tx: bool) -> Vec<u8> {
    let mut v = vec![0xdd, (6 + gtk.len()) as u8, 0x00, 0x0f, 0xac, KDE_GTK];
    v.push((id & 3) | if tx { 4 } else { 0 });
    v.push(0);
    v.extend_from_slice(gtk);
    v
}
