//! The mirror's wire format, byte for byte V1's (`BitsOS-Plattform/net/mirror.asm`), so the unchanged host
//! listener `console-listen.mjs` reads it. A datagram, numbers little-endian:
//!
//! ```text
//! 0  "BM", version 1, kind ('S' screen text, 'L' log text, 'B' start, 'E' end)
//! 4  u32 id of the start it belongs to (new for every boot and Reload)
//! 8  u64 position: of the first text byte in its stream; 'B': where the log's text begins; 'E': 0
//! 16 body: text (CP437, at most CHUNK bytes) | 'B': u32 Reload counter, 4 bytes our IPv4, u8 how (0 CONFIG.TXT)
//!          | 'E': u64 screen end, u64 log end
//! ```
//!
//! Also the raw Ethernet + IPv4 + UDP frame for the emergency path (`NetDevice::poll_tx`: no stack), built in
//! a caller's buffer, and the `mirror=a.b.c.d[:port]` parser. Pure code, host-tested; nothing here panics on
//! any input.

/// Text bytes a datagram carries at most (V1 `MIRROR_CHUNK`; 42 + 16 + 1400 fits a 1500-byte MTU).
pub const CHUNK: usize = 1400;
/// Bytes of the datagram head.
pub const HEAD: usize = 16;
/// The listener's port when CONFIG names none, and our source port (V1 `MIRROR_PORT`). Tests never use it.
pub const DEFAULT_PORT: u16 = 6666;
/// Ethernet (14) + IPv4 (20) + UDP (8).
pub const FRAME_HEAD: usize = 42;
/// The largest datagram and frame this module builds.
pub const DGRAM_MAX: usize = HEAD + CHUNK;
pub const FRAME_MAX: usize = FRAME_HEAD + DGRAM_MAX;
const MIN_FRAME: usize = 60;
pub const ETH_BROADCAST: [u8; 6] = [0xFF; 6];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Screen,
    Log,
    Begin,
    End,
}

impl Kind {
    pub fn byte(self) -> u8 {
        match self {
            Kind::Screen => b'S',
            Kind::Log => b'L',
            Kind::Begin => b'B',
            Kind::End => b'E',
        }
    }
    pub fn from_byte(b: u8) -> Option<Kind> {
        Some(match b {
            b'S' => Kind::Screen,
            b'L' => Kind::Log,
            b'B' => Kind::Begin,
            b'E' => Kind::End,
            _ => return None,
        })
    }
}

/// Writes the 16-byte head into `out[..16]`; false if `out` is too short.
pub fn head(out: &mut [u8], kind: Kind, id: u32, pos: u64) -> bool {
    let Some(h) = out.get_mut(..HEAD) else { return false };
    h[0] = b'B';
    h[1] = b'M';
    h[2] = 1;
    h[3] = kind.byte();
    h[4..8].copy_from_slice(&id.to_le_bytes());
    h[8..16].copy_from_slice(&pos.to_le_bytes());
    true
}

/// A text datagram (`Screen`/`Log`) of at most `CHUNK` bytes of `text` into `out`; returns its length.
pub fn text(out: &mut [u8], kind: Kind, id: u32, pos: u64, text: &[u8]) -> Option<usize> {
    let n = text.len().min(CHUNK);
    let end = HEAD + n;
    if !head(out, kind, id, pos) {
        return None;
    }
    out.get_mut(HEAD..end)?.copy_from_slice(&text[..n]);
    Some(end)
}

/// The start datagram: `log_pos` = where the log's text begins, `reload` = Reload counter, `ip` = our address.
pub fn begin(out: &mut [u8], id: u32, log_pos: u64, reload: u32, ip: [u8; 4]) -> Option<usize> {
    if !head(out, Kind::Begin, id, log_pos) {
        return None;
    }
    let b = out.get_mut(HEAD..HEAD + 9)?;
    b[0..4].copy_from_slice(&reload.to_le_bytes());
    b[4..8].copy_from_slice(&ip);
    b[8] = 0; // how it began: CONFIG.TXT (V1's 1 = the Console command, which V2 has not)
    Some(HEAD + 9)
}

/// The end datagram: where the screen and the log stand, so the listener marks what never came.
pub fn end(out: &mut [u8], id: u32, scr_end: u64, log_end: u64) -> Option<usize> {
    if !head(out, Kind::End, id, 0) {
        return None;
    }
    let b = out.get_mut(HEAD..HEAD + 16)?;
    b[0..8].copy_from_slice(&scr_end.to_le_bytes());
    b[8..16].copy_from_slice(&log_end.to_le_bytes());
    Some(HEAD + 16)
}

/// A parsed datagram head (tests, and the scenario's own checks mirror this).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Head {
    pub kind: Kind,
    pub id: u32,
    pub pos: u64,
}

/// The listener's acceptance rule: at least 16 bytes, "BM", version 1, a known kind.
pub fn parse_head(d: &[u8]) -> Option<Head> {
    if d.len() < HEAD || d[0] != b'B' || d[1] != b'M' || d[2] != 1 {
        return None;
    }
    Some(Head {
        kind: Kind::from_byte(d[3])?,
        id: u32::from_le_bytes(d[4..8].try_into().ok()?),
        pos: u64::from_le_bytes(d[8..16].try_into().ok()?),
    })
}

// ---------------------------------------------------------------------------------------------------------
// CP437: the listener draws every byte as BitsOS's CP437 glyph. klog lines are UTF-8, so each char is
// translated back; what CP437 has not becomes '?'. Bytes below 32 are never produced (8 would erase a
// character on the listener's side, 10 would end the line).

const HIGH: &str = "ÇüéâäàåçêëèïîìÄÅÉæÆôöòûùÿÖÜ¢£¥₧ƒáíóúñÑªº¿⌐¬½¼¡«»░▒▓│┤╡╢╖╕╣║╗╝╜╛┐\
└┴┬├─┼╞╟╚╔╩╦╠═╬╧╨╤╥╙╘╒╓╫╪┘┌█▄▌▐▀αßΓπΣσµτΦΘΩδ∞φε∩≡±≥≤⌠⌡÷≈°∙·√ⁿ²■\u{A0}";

/// The CP437 byte for one char.
pub fn cp437(c: char) -> u8 {
    match c {
        ' '..='~' => c as u8,
        _ => HIGH.chars().position(|h| h == c).map_or(b'?', |i| 128 + i as u8),
    }
}

/// Calls `put` with the CP437 bytes of `s`.
pub fn encode(s: &str, mut put: impl FnMut(u8)) {
    for c in s.chars() {
        put(cp437(c));
    }
}

// ---------------------------------------------------------------------------------------------------------
// The emergency frame

/// Addresses of a raw mirror frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameAddr {
    pub src_mac: [u8; 6],
    pub dst_mac: [u8; 6],
    pub src_ip: [u8; 4],
    pub dst_ip: [u8; 4],
    pub src_port: u16,
    pub dst_port: u16,
}

fn sum16(data: &[u8], mut sum: u32) -> u32 {
    let mut chunks = data.chunks_exact(2);
    for c in &mut chunks {
        sum += u16::from_be_bytes([c[0], c[1]]) as u32;
    }
    if let [b] = chunks.remainder() {
        sum += (*b as u32) << 8;
    }
    sum
}

fn fold(mut sum: u32) -> u16 {
    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

/// The UDP checksum over the pseudo header (0 is sent as 0xFFFF, RFC 768).
fn udp_sum(a: &FrameAddr, udp: &[u8]) -> u16 {
    let mut s = sum16(&a.src_ip, 0);
    s = sum16(&a.dst_ip, s);
    s += 17 + udp.len() as u32;
    match fold(sum16(udp, s)) {
        0 => 0xFFFF,
        c => c,
    }
}

/// Ethernet + IPv4 + UDP around the datagram of `dgram_len` bytes that already sits at `out[FRAME_HEAD..]`
/// (built there with `text`/`begin`/`end`, so the emergency path needs one buffer), padded to the 60-byte
/// minimum; returns the frame length. None when `out` is too small or the datagram larger than `DGRAM_MAX`.
pub fn frame(out: &mut [u8], a: &FrameAddr, ip_id: u16, dgram_len: usize) -> Option<usize> {
    if dgram_len > DGRAM_MAX {
        return None;
    }
    let len = FRAME_HEAD + dgram_len;
    let total = len.max(MIN_FRAME);
    let f = out.get_mut(..total)?;
    f[0..6].copy_from_slice(&a.dst_mac);
    f[6..12].copy_from_slice(&a.src_mac);
    f[12..14].copy_from_slice(&0x0800u16.to_be_bytes());
    let ip_len = (20 + 8 + dgram_len) as u16;
    let ip = &mut f[14..34];
    ip[0] = 0x45;
    ip[1] = 0;
    ip[2..4].copy_from_slice(&ip_len.to_be_bytes());
    ip[4..6].copy_from_slice(&ip_id.to_be_bytes());
    ip[6..8].copy_from_slice(&0x4000u16.to_be_bytes()); // don't fragment
    ip[8] = 64;
    ip[9] = 17;
    ip[10..12].copy_from_slice(&[0, 0]);
    ip[12..16].copy_from_slice(&a.src_ip);
    ip[16..20].copy_from_slice(&a.dst_ip);
    let c = fold(sum16(ip, 0));
    ip[10..12].copy_from_slice(&c.to_be_bytes());
    let udp_len = (8 + dgram_len) as u16;
    f[34..36].copy_from_slice(&a.src_port.to_be_bytes());
    f[36..38].copy_from_slice(&a.dst_port.to_be_bytes());
    f[38..40].copy_from_slice(&udp_len.to_be_bytes());
    f[40..42].copy_from_slice(&[0, 0]);
    let c = udp_sum(a, &f[34..len]);
    f[40..42].copy_from_slice(&c.to_be_bytes());
    f[len..total].fill(0);
    Some(total)
}

/// Reads a frame `frame` built: addresses and the datagram, with both checksums verified (tests, fake cards).
pub fn parse_frame(f: &[u8]) -> Option<(FrameAddr, &[u8])> {
    if f.len() < FRAME_HEAD || f[12..14] != [0x08, 0x00] || f[14] != 0x45 || f[23] != 17 {
        return None;
    }
    if fold(sum16(&f[14..34], 0)) != 0 {
        return None;
    }
    let ip_len = u16::from_be_bytes([f[16], f[17]]) as usize;
    let udp_len = u16::from_be_bytes([f[38], f[39]]) as usize;
    if ip_len != udp_len + 20 || udp_len < 8 || 14 + ip_len > f.len() {
        return None;
    }
    let a = FrameAddr {
        dst_mac: f[0..6].try_into().ok()?,
        src_mac: f[6..12].try_into().ok()?,
        src_ip: f[26..30].try_into().ok()?,
        dst_ip: f[30..34].try_into().ok()?,
        src_port: u16::from_be_bytes([f[34], f[35]]),
        dst_port: u16::from_be_bytes([f[36], f[37]]),
    };
    let udp = &f[34..34 + udp_len];
    let mut s = sum16(&a.src_ip, 0);
    s = sum16(&a.dst_ip, s);
    s += 17 + udp_len as u32;
    if fold(sum16(udp, s)) != 0 {
        return None;
    }
    Some((a, &udp[8..]))
}

// ---------------------------------------------------------------------------------------------------------
// CONFIG

/// Where the mirror sends: CONFIG `mirror=a.b.c.d[:port]` (V1's key `console=` is read too).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Target {
    pub ip: [u8; 4],
    pub port: u16,
}

/// `a.b.c.d` or `a.b.c.d:port` (port 6666 if left out, as V1). Refused: 0.0.0.0, broadcast and multicast
/// (the stack routes neither for UDP), 127.x (the mirror is for another machine), port 0. The error says why.
pub fn parse_target(s: &str) -> Result<Target, &'static str> {
    let s = s.trim();
    let (ip, port) = match s.split_once(':') {
        Some((ip, p)) => {
            // Digits only: `parse` would also take "+5".
            let p = p.trim();
            if p.is_empty() || p.len() > 5 || !p.bytes().all(|c| c.is_ascii_digit()) {
                return Err("port is not 1..65535");
            }
            (ip, p.parse::<u16>().map_err(|_| "port is not 1..65535")?)
        }
        None => (s, DEFAULT_PORT),
    };
    if port == 0 {
        return Err("port is not 1..65535");
    }
    let mut parts = ip.trim().split('.');
    let mut a = [0u8; 4];
    for b in &mut a {
        let p = parts.next().ok_or("want a.b.c.d[:port]")?;
        if p.is_empty() || p.len() > 3 || !p.bytes().all(|c| c.is_ascii_digit()) {
            return Err("want a.b.c.d[:port]");
        }
        *b = p.parse().map_err(|_| "an address byte is above 255")?;
    }
    if parts.next().is_some() {
        return Err("want a.b.c.d[:port]");
    }
    match a {
        [0, 0, 0, 0] => Err("0.0.0.0 is no listener"),
        [255, 255, 255, 255] => Err("broadcast is not supported; name the listener's address"),
        [224..=239, ..] => Err("multicast is not supported; name the listener's address"),
        [127, ..] => Err("127.x is this machine; name the listener's address"),
        _ => Ok(Target { ip: a, port }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr() -> FrameAddr {
        FrameAddr {
            src_mac: [0x52, 0x54, 0, 0x12, 0x34, 0x56],
            dst_mac: ETH_BROADCAST,
            src_ip: [10, 0, 2, 15],
            dst_ip: [10, 0, 2, 2],
            src_port: DEFAULT_PORT,
            dst_port: 23300,
        }
    }

    #[test]
    fn heads_match_console_listen() {
        let mut b = [0u8; DGRAM_MAX];
        let n = text(&mut b, Kind::Log, 0xA1B2_C3D4, 0x0102_0304_0506_0708, b"hello\n").unwrap();
        assert_eq!(n, 22);
        assert_eq!(&b[..16], &[b'B', b'M', 1, b'L', 0xD4, 0xC3, 0xB2, 0xA1, 8, 7, 6, 5, 4, 3, 2, 1]);
        assert_eq!(&b[16..22], b"hello\n");
        assert_eq!(parse_head(&b[..n]), Some(Head { kind: Kind::Log, id: 0xA1B2_C3D4, pos: 0x0102_0304_0506_0708 }));
        let n = begin(&mut b, 7, 1234, 3, [192, 168, 100, 20]).unwrap();
        assert_eq!(n, 25);
        assert_eq!(b[3], b'B');
        assert_eq!(u64::from_le_bytes(b[8..16].try_into().unwrap()), 1234);
        assert_eq!(&b[16..25], &[3, 0, 0, 0, 192, 168, 100, 20, 0]);
        let n = end(&mut b, 7, 11, 22).unwrap();
        assert_eq!(n, 32);
        assert_eq!(b[3], b'E');
        assert_eq!(u64::from_le_bytes(b[16..24].try_into().unwrap()), 11, "screen end first");
        assert_eq!(u64::from_le_bytes(b[24..32].try_into().unwrap()), 22, "then the log's");
        let n = text(&mut b, Kind::Screen, 1, 0, &[b'x'; 3000]).unwrap();
        assert_eq!(n, HEAD + CHUNK, "a chunk at most");
        assert!(text(&mut [0u8; 10], Kind::Log, 1, 0, b"x").is_none());
        assert!(parse_head(b"BM\x02L0123456789ab").is_none());
        assert!(parse_head(b"BM\x01X0123456789ab").is_none());
        assert!(parse_head(b"BM\x01L").is_none());
    }

    #[test]
    fn cp437_translation_never_makes_control_bytes() {
        let mut out = alloc::vec::Vec::new();
        encode("a ü→é█\u{1}\u{8}\n~\u{7f}ß", |b| out.push(b));
        assert_eq!(out, [b'a', b' ', 0x81, b'?', 0x82, 0xDB, b'?', b'?', b'?', b'~', b'?', 0xE1]);
        for c in ['\u{0}', '\u{8}', '\n', '\r', '\t', '\u{1b}'] {
            assert_eq!(cp437(c), b'?');
        }
        assert_eq!(cp437('\u{A0}'), 255);
        assert_eq!(HIGH.chars().count(), 128);
    }

    #[test]
    fn frames_carry_valid_checksums_and_round_trip() {
        let mut d = [0u8; DGRAM_MAX];
        let mut f = [0u8; FRAME_MAX];
        for len in [0usize, 1, 2, 5, 17, 1399, 1400] {
            let body = alloc::vec![b'q'; len];
            let n = text(&mut d, Kind::Log, 9, 100, &body).unwrap();
            f[FRAME_HEAD..FRAME_HEAD + n].copy_from_slice(&d[..n]);
            let fl = frame(&mut f, &addr(), 0x1234, n).unwrap();
            assert_eq!(fl, (FRAME_HEAD + n).max(60));
            assert!(fl <= 1514, "fits an Ethernet frame");
            let (a, got) = parse_frame(&f[..fl]).expect("valid frame");
            assert_eq!(a, addr());
            assert_eq!(got, &d[..n]);
            // An independent check of the IPv4 header sum (RFC 1071 over the header gives 0).
            let mut s: u32 = 0;
            for c in f[14..34].chunks(2) {
                s += u16::from_be_bytes([c[0], c[1]]) as u32;
            }
            while s >> 16 != 0 {
                s = (s & 0xFFFF) + (s >> 16);
            }
            assert_eq!(s, 0xFFFF);
            assert_eq!(&f[18..20], &[0x12, 0x34], "IP id");
            assert_eq!(&f[20..22], &[0x40, 0], "DF");
        }
        let mut bad = [0u8; FRAME_MAX];
        let n = text(&mut bad[FRAME_HEAD..], Kind::Log, 9, 100, b"abc").unwrap();
        let fl = frame(&mut bad, &addr(), 1, n).unwrap();
        assert!(parse_frame(&bad[..fl]).is_some());
        bad[FRAME_HEAD + 17] ^= 1;
        assert!(parse_frame(&bad[..fl]).is_none(), "UDP checksum catches a flipped bit");
        assert!(frame(&mut [0u8; 50], &addr(), 1, n).is_none());
        assert!(frame(&mut [0u8; 4000], &addr(), 1, DGRAM_MAX + 1).is_none());
        assert!(parse_frame(&[0u8; 30]).is_none());
    }

    #[test]
    fn targets() {
        assert_eq!(parse_target("10.0.2.2:23300"), Ok(Target { ip: [10, 0, 2, 2], port: 23300 }));
        assert_eq!(parse_target(" 192.168.100.30 "), Ok(Target { ip: [192, 168, 100, 30], port: 6666 }));
        for bad in ["", "1.2.3", "1.2.3.4.5", "1.2.3.256", "1.2.3.4:0", "1.2.3.4:65536", "1.2.3.4:x", "0.0.0.0", "255.255.255.255", "127.0.0.1:5", "224.0.0.1", "a.b.c.d", "1.2.3.+4", "1..2.3"] {
            assert!(parse_target(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn hostile_targets_are_refused_without_panic() {
        let long = "1.".repeat(5000) + "1";
        let huge_port = alloc::format!("1.2.3.4:{}", "9".repeat(4000));
        for bad in [
            "mirror=",
            "1.2.3.4:",
            ":",
            ":6666",
            "1.2.3.4:99999",
            "1.2.3.4:+5",
            "1.2.3.4:-1",
            "1.2.3.4:5:6",
            "1.2.3.4: ",
            "1.2.3.4x",
            "1.2.3.4 5",
            "01234.1.1.1",
            "::1",
            "[fe80::1]:6666",
            "fe80::1",
            "1.2.3.4\0",
            "1.2.3.\u{e4}",
            "\u{1F600}",
            long.as_str(),
            huge_port.as_str(),
        ] {
            assert!(parse_target(bad).is_err(), "{bad:?}");
        }
        // Every byte value in every position: an error or a valid target, never a panic.
        for b in 0..=255u8 {
            let mut v = alloc::vec::Vec::from(*b"10.0.2.2:23300");
            for i in 0..v.len() {
                let old = v[i];
                v[i] = b;
                if let Ok(s) = core::str::from_utf8(&v) {
                    let _ = parse_target(s);
                }
                v[i] = old;
            }
        }
        assert_eq!(parse_target("1.2.3.4:65535"), Ok(Target { ip: [1, 2, 3, 4], port: 65535 }));
        assert_eq!(parse_target("1.2.3.4:00080"), Ok(Target { ip: [1, 2, 3, 4], port: 80 }));
    }
}
