//! OpenPGP framing: packets (RFC 9580 section 4.2), MPIs (3.2) and ASCII armor (section 6). Everything here reads
//! untrusted bytes: every read is checked and fails with a German reason, nothing panics, and nothing allocates
//! more than the input holds. The packet walk follows pacman/lib/libalpm/signing.c:1076 (alpm_extract_keyid).

/// The bound for one key, user ID or signature packet (brief: a field or packet at most 64 KiB).
pub const MAX_PACKET: usize = 64 << 10;

pub const TAG_SIG: u8 = 2;
pub const TAG_KEY: u8 = 6;
pub const TAG_MARKER: u8 = 10;
pub const TAG_TRUST: u8 = 12;
pub const TAG_UID: u8 = 13;
pub const TAG_SUBKEY: u8 = 14;
pub const TAG_UATTR: u8 = 17;

#[derive(Debug)]
pub struct Packet<'a> {
    pub tag: u8,
    pub body: &'a [u8],
}

/// A bounds-checked reader over a byte slice.
pub struct Rd<'a> {
    b: &'a [u8],
    p: usize,
}

pub fn short() -> String {
    "Paket zu kurz (abgeschnitten)".into()
}

impl<'a> Rd<'a> {
    pub fn new(b: &'a [u8]) -> Self {
        Rd { b, p: 0 }
    }
    pub fn pos(&self) -> usize {
        self.p
    }
    pub fn left(&self) -> usize {
        self.b.len() - self.p
    }
    pub fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        if n > self.left() {
            return Err(short());
        }
        let s = &self.b[self.p..self.p + n];
        self.p += n;
        Ok(s)
    }
    pub fn u8(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }
    pub fn u16(&mut self) -> Result<u16, String> {
        let s = self.take(2)?;
        Ok(u16::from_be_bytes([s[0], s[1]]))
    }
    pub fn u32(&mut self) -> Result<u32, String> {
        let s = self.take(4)?;
        Ok(u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
    }
    pub fn rest(&mut self) -> &'a [u8] {
        let s = &self.b[self.p..];
        self.p = self.b.len();
        s
    }
    /// An MPI (RFC 9580 3.2): a two-octet bit count, then the octets. Returned without leading zero octets.
    pub fn mpi(&mut self) -> Result<&'a [u8], String> {
        let bits = self.u16()? as usize;
        let mut v = self.take(bits.div_ceil(8))?;
        while let [0, r @ ..] = v {
            v = r;
        }
        Ok(v)
    }
}

/// Splits a binary packet stream into packets. Partial body lengths (RFC 9580 4.2.1.4) are allowed only for data
/// packets, which never occur in keys or signatures, so every one is refused here; so is the old format's
/// indeterminate length (4.2.2), which 9580 reserves for data packets as well.
pub fn packets(data: &[u8]) -> Result<Vec<Packet<'_>>, String> {
    let mut r = Rd::new(data);
    let mut out = Vec::new();
    while r.left() > 0 {
        let at = r.pos();
        let h = r.u8()?;
        if h & 0x80 == 0 {
            return Err(format!("kein OpenPGP-Paket an Stelle {at} (Kopfbyte {h:#04x})"));
        }
        let (tag, len) = if h & 0x40 != 0 {
            let o = r.u8()? as usize;
            let len = match o {
                0..192 => o,
                192..224 => ((o - 192) << 8) + r.u8()? as usize + 192,
                255 => r.u32()? as usize,
                _ => return Err(format!("Teil-Längen sind in Schlüsseln und Signaturen nicht erlaubt (Stelle {at})")),
            };
            (h & 0x3f, len)
        } else {
            let len = match h & 3 {
                0 => r.u8()? as usize,
                1 => r.u16()? as usize,
                2 => r.u32()? as usize,
                _ => return Err(format!("unbestimmte Paketlänge ist hier nicht erlaubt (Stelle {at})")),
            };
            ((h >> 2) & 0x0f, len)
        };
        if tag == 0 {
            return Err(format!("Pakettyp 0 ist verboten (Stelle {at})"));
        }
        let body = r.take(len).map_err(|_| format!("Paket an Stelle {at} länger als die Eingabe ({len} Bytes)"))?;
        out.push(Packet { tag, body });
    }
    Ok(out)
}

/// The binary form of `input`: armored blocks (any number, as `<keyring>.gpg` files have) are decoded and
/// concatenated; anything that does not start with an armor line is taken as binary.
pub fn dearmor(input: &[u8]) -> Result<Vec<u8>, String> {
    let start = input.iter().position(|c| !c.is_ascii_whitespace()).unwrap_or(input.len());
    if !input[start..].starts_with(b"-----BEGIN PGP ") {
        return Ok(input.to_vec());
    }
    let text = std::str::from_utf8(input).map_err(|_| "Armor ist kein ASCII-Text".to_string())?;
    let mut out = Vec::new();
    let mut lines = text.lines().map(|l| l.trim_end_matches([' ', '\t', '\r']));
    let mut blocks = 0usize;
    while let Some(l) = lines.next() {
        let Some(kind) = l.strip_prefix("-----BEGIN PGP ").and_then(|k| k.strip_suffix("-----")) else {
            if l.trim().is_empty() {
                continue;
            }
            return Err(format!("Text außerhalb eines Armor-Blocks: {:.40}", l));
        };
        if kind != "PUBLIC KEY BLOCK" && kind != "SIGNATURE" {
            return Err(format!("Armor-Art \"{kind}\" wird hier nicht gelesen"));
        }
        // Armor headers ("Key: value") end at the first empty line; 9580 6.2 also allows none at all.
        let mut b64 = String::new();
        let mut crc = None;
        let mut in_headers = true;
        let mut ended = false;
        for l in lines.by_ref() {
            if l == format!("-----END PGP {kind}-----") {
                ended = true;
                break;
            }
            if in_headers {
                if l.is_empty() {
                    in_headers = false;
                    continue;
                }
                if l.contains(": ") {
                    continue;
                }
                in_headers = false;
            }
            if let Some(c) = l.strip_prefix('=') {
                crc = Some(c.to_string());
            } else if crc.is_some() {
                return Err("Armor: Daten nach der Prüfsumme".into());
            } else {
                b64.push_str(l);
            }
        }
        if !ended {
            return Err(format!("Armor-Block \"{kind}\" ohne Ende"));
        }
        let bin = base64(&b64)?;
        if let Some(c) = crc {
            let want = base64(&c)?;
            let got = crc24(&bin).to_be_bytes();
            if want.len() != 3 || want[..] != got[1..] {
                return Err("Armor-Prüfsumme (CRC24) falsch".into());
            }
        }
        out.extend_from_slice(&bin);
        blocks += 1;
    }
    if blocks == 0 {
        return Err("Armor ohne Block".into());
    }
    Ok(out)
}

fn b64val(c: u8) -> Option<u32> {
    Some(match c {
        b'A'..=b'Z' => c - b'A',
        b'a'..=b'z' => c - b'a' + 26,
        b'0'..=b'9' => c - b'0' + 52,
        b'+' => 62,
        b'/' => 63,
        _ => return None,
    } as u32)
}

/// Strict base64 (RFC 4648 section 4, padding required where the length needs it).
pub fn base64(s: &str) -> Result<Vec<u8>, String> {
    let s: Vec<u8> = s.bytes().filter(|c| !c.is_ascii_whitespace()).collect();
    if s.len() % 4 != 0 {
        return Err("Base64-Länge ist kein Vielfaches von 4".into());
    }
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let n = s.len() / 4;
    for (i, q) in s.chunks(4).enumerate() {
        let pad = q.iter().rev().take_while(|&&c| c == b'=').count();
        if pad > 2 || (pad > 0 && i + 1 != n) {
            return Err("Base64: '=' an falscher Stelle".into());
        }
        let mut v = 0u32;
        for &c in &q[..4 - pad] {
            v = (v << 6) | b64val(c).ok_or_else(|| format!("Base64: Zeichen {:?} nicht erlaubt", c as char))?;
        }
        v <<= 6 * pad as u32;
        let b = v.to_be_bytes();
        out.extend_from_slice(&b[1..4 - pad]);
    }
    Ok(out)
}

/// CRC-24 of RFC 9580 section 6.1.
pub fn crc24(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xB704CE;
    for &b in data {
        crc ^= (b as u32) << 16;
        for _ in 0..8 {
            crc <<= 1;
            if crc & 0x100_0000 != 0 {
                crc ^= 0x1864CFB;
            }
        }
    }
    crc & 0xFF_FFFF
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02X}")).collect()
}

/// Parses a hex fingerprint (as in `<keyring>-trusted`); `None` if it is not 40 or 64 hex digits.
pub fn unhex(s: &str) -> Option<Vec<u8>> {
    if !(s.len() == 40 || s.len() == 64) || !s.bytes().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framing_new_old_and_refusals() {
        // new format, one-octet length; old format, one-octet length
        let p = packets(&[0xC2, 0x01, 0xAA, 0x88, 0x01, 0xBB]).unwrap();
        assert_eq!((p[0].tag, p[0].body), (2, &[0xAA][..]));
        assert_eq!((p[1].tag, p[1].body), (2, &[0xBB][..]));
        // two-octet new length: 192 + ((0xC0-192)<<8) + 8 = 200
        let mut v = vec![0xC2, 0xC0, 0x08];
        v.extend(std::iter::repeat_n(0u8, 200));
        assert_eq!(packets(&v).unwrap()[0].body.len(), 200);
        assert!(packets(&[0xC2, 0xE0, 0]).unwrap_err().contains("Teil-Längen"));
        assert!(packets(&[0x8B, 0]).unwrap_err().contains("unbestimmte"));
        assert!(packets(&[0xC2, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]).is_err());
        assert!(packets(&[0x42]).is_err());
        assert!(packets(&[0xC0, 0]).is_err());
    }

    #[test]
    fn base64_and_crc() {
        assert_eq!(base64("T3BlblBHUA==").unwrap(), b"OpenPGP");
        assert!(base64("T3BlblBHUA=").is_err());
        assert!(base64("T3Bl=lBHUA==").is_err());
        assert_eq!(crc24(b""), 0xB704CE);
        let a = "-----BEGIN PGP SIGNATURE-----\n\nT3BlblBHUA==\n=";
        let c = crc24(b"OpenPGP").to_be_bytes();
        let crc = {
            let t = [c[1], c[2], c[3]];
            let v = ((t[0] as u32) << 16) | ((t[1] as u32) << 8) | t[2] as u32;
            let al = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
            (0..4).map(|i| al[((v >> (18 - 6 * i)) & 63) as usize] as char).collect::<String>()
        };
        let good = format!("{a}{crc}\n-----END PGP SIGNATURE-----\n");
        assert_eq!(dearmor(good.as_bytes()).unwrap(), b"OpenPGP");
        let bad = format!("{a}AAAA\n-----END PGP SIGNATURE-----\n");
        assert!(dearmor(bad.as_bytes()).unwrap_err().contains("CRC24"));
        assert!(dearmor(b"-----BEGIN PGP SIGNATURE-----\n\nT3Bl\n").is_err());
    }
}
