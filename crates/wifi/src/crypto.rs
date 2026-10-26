//! The cryptography of WPA2-PSK (AKM 00-0F-AC:2, CCMP, key descriptor version 2) on RustCrypto:
//! the passphrase to PSK mapping (PBKDF2-SHA1, IEEE 802.11-2020 J.4), the 802.11 PRF and the
//! pairwise key hierarchy (12.7.1.2, 12.7.1.3), the EAPOL-Key MIC (HMAC-SHA1-128) and the AES key
//! unwrap of the key data (RFC 3394). V1 had all of it in assembler (`crypto/`); hostap's
//! sha1_prf, wpa_pmk_to_ptk, wpa_eapol_key_mic and aes_unwrap are the reference for the field
//! order. The AES is RustCrypto's software one (feature `force-soft`): the kernel has no SSE
//! code generation (R10), so AES-NI stays out.

use aes::cipher::generic_array::GenericArray;
use aes::cipher::{BlockDecrypt, BlockEncrypt, NewBlockCipher};
use hmac::{Hmac, Mac};
use sha1::{Digest, Sha1};
use zeroize::Zeroize;

use crate::secret::{Passphrase, Secret};

pub type Pmk = Secret<32>;
type HmacSha1 = Hmac<Sha1>;

/// SHA-1 of `data` (FIPS 180-4)
pub fn sha1(data: &[u8]) -> [u8; 20] {
    Sha1::digest(data).into()
}

/// HMAC-SHA1 (RFC 2104) of the concatenated `parts`
pub fn hmac_sha1(key: &[u8], parts: &[&[u8]]) -> [u8; 20] {
    // HMAC takes keys of every length (longer ones are hashed first): new_from_slice never fails
    let Ok(mut m) = <HmacSha1 as Mac>::new_from_slice(key) else {
        return [0; 20];
    };
    for p in parts {
        m.update(p);
    }
    m.finalize().into_bytes().into()
}

/// PBKDF2 with HMAC-SHA1 (RFC 8018 5.2) into `out`
pub fn pbkdf2_sha1(pass: &[u8], salt: &[u8], rounds: u32, out: &mut [u8]) {
    pbkdf2::pbkdf2_hmac::<Sha1>(pass, salt, rounds, out);
}

/// Why a passphrase or hex PSK was refused (the value itself is never part of it, R8i)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PskError {
    /// 802.11 J.4.1: 8 to 63 characters (V1 msg_ws_psk_len)
    Length { len: usize },
    /// a character outside ASCII 32..126 (J.4.1)
    NotPrintable,
    /// 64 characters that are not all hex digits (V1 msg_ws_psk_hex)
    NotHex,
    /// the SSID is empty or longer than 32 bytes
    Ssid { len: usize },
}

impl core::fmt::Display for PskError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            PskError::Length { len } => {
                write!(
                    f,
                    "wlan_psk= must be 8 to 63 characters or 64 hex digits (it has {len})"
                )
            }
            PskError::NotPrintable => {
                f.write_str("wlan_psk= has a character outside ASCII 32..126")
            }
            PskError::NotHex => f.write_str("wlan_psk= of 64 characters is not hex"),
            PskError::Ssid { len } => write!(f, "the SSID must be 1 to 32 bytes (it has {len})"),
        }
    }
}

// Linux: none (802.11-2020 J.4.1, hostap pbkdf2_sha1 as wpa_supplicant's wpa_passphrase): PSK =
// PBKDF2(HMAC-SHA1, passphrase, SSID, 4096, 256 bits)
pub fn psk(pass: &[u8], ssid: &[u8]) -> Result<Pmk, PskError> {
    if !(8..=63).contains(&pass.len()) {
        return Err(PskError::Length { len: pass.len() });
    }
    if pass.iter().any(|&c| !(32..=126).contains(&c)) {
        return Err(PskError::NotPrintable);
    }
    if ssid.is_empty() || ssid.len() > 32 {
        return Err(PskError::Ssid { len: ssid.len() });
    }
    let mut out = Secret::new([0u8; 32]);
    pbkdf2_sha1(pass, ssid, 4096, out.expose_mut());
    Ok(out)
}

// Linux: none (V1 wlan/rtw89/sta-wpa.asm ws_wpa_pmk): CONFIG.TXT's wlan_psk= is a passphrase,
// or with 64 characters the PSK itself in hex
pub fn pmk_from_config(value: &Passphrase, ssid: &[u8]) -> Result<Pmk, PskError> {
    let v = value.expose();
    if v.len() != 64 {
        return psk(v, ssid);
    }
    if ssid.is_empty() || ssid.len() > 32 {
        return Err(PskError::Ssid { len: ssid.len() });
    }
    let mut out = Secret::new([0u8; 32]);
    for (i, pair) in v.chunks(2).enumerate() {
        let (Some(h), Some(l)) = (hex_digit(pair[0]), hex_digit(pair[1])) else {
            return Err(PskError::NotHex); // `out` is wiped on drop
        };
        out.expose_mut()[i] = (h << 4) | l;
    }
    Ok(out)
}

fn hex_digit(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

// Linux: none (hostap sha1_prf, sha1-prf.c; 802.11-2020 12.7.1.2): PRF-n(K, A, B) =
// HMAC-SHA1(K, A || 0 || B || i) for i = 0, 1, ..., cut to out.len()
pub fn prf(key: &[u8], label: &[u8], data: &[u8], out: &mut [u8]) {
    let mut done = 0;
    let mut i = 0u8;
    while done < out.len() {
        let mut h = hmac_sha1(key, &[label, &[0], data, &[i]]);
        let n = (out.len() - done).min(h.len());
        out[done..done + n].copy_from_slice(&h[..n]);
        h.zeroize();
        done += n;
        i = i.wrapping_add(1);
    }
}

/// The pairwise transient key of CCMP: KCK (EAPOL-Key MIC), KEK (key data), TK (the frames)
pub struct Ptk {
    pub kck: Secret<16>,
    pub kek: Secret<16>,
    pub tk: Secret<16>,
}

impl Ptk {
    pub fn duplicate(&self) -> Ptk {
        Ptk {
            kck: self.kck.duplicate(),
            kek: self.kek.duplicate(),
            tk: self.tk.duplicate(),
        }
    }
}

impl core::fmt::Debug for Ptk {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("<PTK>")
    }
}

// Linux: none (hostap wpa_pmk_to_ptk, wpa_common.c): PTK = PRF-384(PMK, "Pairwise key
// expansion", Min(AA,SPA) || Max(AA,SPA) || Min(ANonce,SNonce) || Max(ANonce,SNonce))
pub fn ptk(pmk: &Pmk, aa: &[u8; 6], spa: &[u8; 6], anonce: &[u8; 32], snonce: &[u8; 32]) -> Ptk {
    let (a1, a2) = if aa[..] <= spa[..] {
        (aa, spa)
    } else {
        (spa, aa)
    };
    let (n1, n2) = if anonce[..] <= snonce[..] {
        (anonce, snonce)
    } else {
        (snonce, anonce)
    };
    let mut data = [0u8; 6 + 6 + 32 + 32];
    data[..6].copy_from_slice(a1);
    data[6..12].copy_from_slice(a2);
    data[12..44].copy_from_slice(n1);
    data[44..].copy_from_slice(n2);
    let mut out = Secret::new([0u8; 48]);
    prf(
        pmk.expose(),
        b"Pairwise key expansion",
        &data,
        out.expose_mut(),
    );
    let o = out.expose();
    let part = |at: usize| {
        let mut b = [0u8; 16];
        b.copy_from_slice(&o[at..at + 16]);
        Secret::new(b)
    };
    Ptk {
        kck: part(0),
        kek: part(16),
        tk: part(32),
    }
}

// Linux: none (hostap wpa_eapol_key_mic, descriptor version 2): HMAC-SHA1 of the frame (its MIC
// field as zeros), cut to 128 bits
pub fn eapol_mic(kck: &Secret<16>, frame: &[u8]) -> [u8; 16] {
    let mut h = hmac_sha1(kck.expose(), &[frame]);
    let mut m = [0u8; 16];
    m.copy_from_slice(&h[..16]);
    h.zeroize();
    m
}

/// One AES-128 block encryption (FIPS 197), for the vectors and the CCMP test peer
pub fn aes128_encrypt_block(key: &[u8; 16], block: &mut [u8; 16]) {
    let c = aes::Aes128::new(GenericArray::from_slice(key));
    let b = GenericArray::from_mut_slice(block);
    c.encrypt_block(b);
}

/// One AES-128 block decryption
pub fn aes128_decrypt_block(key: &[u8; 16], block: &mut [u8; 16]) {
    let c = aes::Aes128::new(GenericArray::from_slice(key));
    let b = GenericArray::from_mut_slice(block);
    c.decrypt_block(b);
}

// Linux: none (hostap aes_unwrap, RFC 3394 2.2.2): `wrapped` (n + 1 semiblocks, n >= 2) into
// `out` (n semiblocks). False when the lengths do not fit or the integrity value is not
// A6A6A6A6A6A6A6A6; `out` is wiped then.
pub fn aes_unwrap(kek: &Secret<16>, wrapped: &[u8], out: &mut [u8]) -> bool {
    if wrapped.len() < 24 || !wrapped.len().is_multiple_of(8) || out.len() != wrapped.len() - 8 {
        out.zeroize();
        return false;
    }
    let k = aes_kw::KekAes128::new(GenericArray::from_slice(kek.expose()));
    if k.unwrap(wrapped, out).is_ok() {
        return true;
    }
    out.zeroize();
    false
}

// Linux: none (hostap aes_wrap, RFC 3394 2.2.1): the authenticator's side, for test peers and
// the fake AP; `plain` n semiblocks (n >= 2) into `out` (n + 1)
pub fn aes_wrap(kek: &Secret<16>, plain: &[u8], out: &mut [u8]) -> bool {
    if plain.len() < 16 || !plain.len().is_multiple_of(8) || out.len() != plain.len() + 8 {
        return false;
    }
    let k = aes_kw::KekAes128::new(GenericArray::from_slice(kek.expose()));
    k.wrap(plain, out).is_ok()
}
