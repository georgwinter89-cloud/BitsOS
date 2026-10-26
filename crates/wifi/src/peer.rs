//! The authenticator side of the WPA2-PSK 4-way and group key handshakes (hostapd's role), for
//! host tests and fake APs only (feature `peer`): a port of V1's `wlan-ref.mjs` Authenticator.
//! It builds messages 1 and 3 and group message 1 and checks what the station answers. Its PTK
//! comes from `crypto::ptk`, which the published vectors check on their own.

use alloc::vec::Vec;

use crate::crypto::{self, Pmk, Ptk};
use crate::eapol::{self, KeyFields, ki};
use crate::secret::{Secret, ct_eq};

/// The RSN element of a WPA2-PSK CCMP AP (wlan-ref.mjs RSN_CCMP_PSK)
pub const RSN_CCMP_PSK: [u8; 22] = [
    0x30, 0x14, 0x01, 0x00, 0x00, 0x0f, 0xac, 0x04, 0x01, 0x00, 0x00, 0x0f, 0xac, 0x04, 0x01, 0x00,
    0x00, 0x0f, 0xac, 0x02, 0x00, 0x00,
];

/// The authenticator of one station
pub struct Authenticator {
    pub pmk: Pmk,
    pub aa: [u8; 6],
    pub spa: [u8; 6],
    pub anonce: [u8; 32],
    pub ap_ie: Vec<u8>,
    pub gtk: Vec<u8>,
    pub gtk_id: u8,
    pub tx: bool,
    pub rsc: [u8; 8],
    pub replay: u64,
    /// the PTK of the station's last good message 2
    pub ptk: Option<Ptk>,
}

impl Authenticator {
    pub fn new(
        pmk: Pmk,
        aa: [u8; 6],
        spa: [u8; 6],
        anonce: [u8; 32],
        gtk: &[u8],
        gtk_id: u8,
    ) -> Authenticator {
        Authenticator {
            pmk,
            aa,
            spa,
            anonce,
            ap_ie: RSN_CCMP_PSK.to_vec(),
            gtk: gtk.to_vec(),
            gtk_id,
            tx: false,
            rsc: [0; 8],
            replay: 1,
            ptk: None,
        }
    }

    fn frame(
        &self,
        info: u16,
        key_len: u16,
        nonce: Option<&[u8; 32]>,
        data: &[u8],
        mic: bool,
    ) -> Vec<u8> {
        let f = KeyFields {
            version: 2,
            info,
            key_len,
            replay: self.replay.to_be_bytes(),
            nonce,
            rsc: self.rsc,
            data,
        };
        eapol::build_frame(
            &f,
            if mic {
                self.ptk.as_ref().map(|p| &p.kck)
            } else {
                None
            },
        )
    }

    /// Message 1: the ANonce, no MIC
    pub fn message1(&self) -> Vec<u8> {
        let f = KeyFields {
            version: 2,
            info: ki::VER_AES | ki::PAIRWISE | ki::ACK,
            key_len: 16,
            replay: self.replay.to_be_bytes(),
            nonce: Some(&self.anonce),
            rsc: [0; 8],
            data: &[],
        };
        eapol::build_frame(&f, None)
    }

    /// The station's message 2: its SNonce gives the PTK; false if the MIC or the frame is wrong
    pub fn message2(&mut self, frame: &[u8]) -> bool {
        let Ok(k) = eapol::parse(frame) else {
            return false;
        };
        let mut snonce = [0u8; 32];
        snonce.copy_from_slice(k.nonce);
        let ptk = crypto::ptk(&self.pmk, &self.aa, &self.spa, &self.anonce, &snonce);
        let ok = k.desc == 2
            && k.info == ki::VER_AES | ki::PAIRWISE | ki::MIC
            && u64::from_be_bytes(k.replay) == self.replay
            && mic_ok(frame, &ptk.kck);
        if ok {
            self.ptk = Some(ptk);
        }
        ok
    }

    /// Key data for message 3 / group 1: our RSN element (message 3), the GTK KDE, `extra`
    pub fn key_data(&self, with_ie: bool, extra: &[u8]) -> Vec<u8> {
        let mut d = Vec::new();
        if with_ie {
            d.extend_from_slice(&self.ap_ie);
        }
        d.extend(eapol::gtk_kde(&self.gtk, self.gtk_id, self.tx));
        d.extend_from_slice(extra);
        eapol::pad_key_data(d)
    }

    /// `plain` wrapped with the KEK of the PTK
    pub fn wrap(&self, plain: &[u8]) -> Vec<u8> {
        let mut out = alloc::vec![0u8; plain.len() + 8];
        let kek = self
            .ptk
            .as_ref()
            .map_or(Secret::new([0; 16]), |p| p.kek.duplicate());
        crypto::aes_wrap(&kek, plain, &mut out);
        out
    }

    /// Message 3 with the GTK (and `extra` elements in its key data); the counter goes up
    pub fn message3(&mut self, extra: &[u8]) -> Vec<u8> {
        self.replay += 1;
        let plain = self.key_data(true, extra);
        let data = self.wrap(&plain);
        let info = ki::VER_AES
            | ki::PAIRWISE
            | ki::INSTALL
            | ki::ACK
            | ki::MIC
            | ki::SECURE
            | ki::ENCRYPTED;
        let anonce = self.anonce;
        self.frame(info, 16, Some(&anonce), &data, true)
    }

    /// Message 4 as it must be: 99 bytes, Secure, the counter of message 3, no nonce, good MIC
    pub fn message4_ok(&self, frame: &[u8]) -> bool {
        let Some(ptk) = &self.ptk else { return false };
        frame.len() == 99
            && eapol::parse(frame).is_ok_and(|k| {
                k.info == ki::VER_AES | ki::PAIRWISE | ki::MIC | ki::SECURE
                    && u64::from_be_bytes(k.replay) == self.replay
                    && k.nonce.iter().all(|&b| b == 0)
            })
            && mic_ok(frame, &ptk.kck)
    }

    /// Group message 1 with a new GTK; the counter goes up
    pub fn group1(&mut self, gtk: &[u8], id: u8) -> Vec<u8> {
        self.gtk = gtk.to_vec();
        self.gtk_id = id;
        self.replay += 1;
        let plain = self.key_data(false, &[]);
        let data = self.wrap(&plain);
        self.frame(
            ki::VER_AES | ki::ACK | ki::MIC | ki::SECURE | ki::ENCRYPTED,
            0,
            None,
            &data,
            true,
        )
    }

    /// Group message 2 as it must be
    pub fn group2_ok(&self, frame: &[u8]) -> bool {
        let Some(ptk) = &self.ptk else { return false };
        frame.len() == 99
            && eapol::parse(frame).is_ok_and(|k| {
                k.info == ki::VER_AES | ki::MIC | ki::SECURE
                    && u64::from_be_bytes(k.replay) == self.replay
            })
            && mic_ok(frame, &ptk.kck)
    }

    /// The TK the station must have
    pub fn tk(&self) -> Option<&Secret<16>> {
        self.ptk.as_ref().map(|p| &p.tk)
    }
}

/// HMAC-SHA1-128 over the frame with its MIC field zero equals the frame's MIC
pub fn mic_ok(frame: &[u8], kck: &Secret<16>) -> bool {
    if frame.len() < eapol::KEY_HDR_LEN {
        return false;
    }
    let mut z = frame.to_vec();
    z[eapol::OFF_MIC..eapol::OFF_MIC + 16].fill(0);
    ct_eq(
        &crypto::eapol_mic(kck, &z),
        &frame[eapol::OFF_MIC..eapol::OFF_MIC + 16],
    )
}
