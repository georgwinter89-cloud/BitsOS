//! Raw test frames for the drivers' own checks (virtio-net, igc: ARP to the gateway, DHCP DISCOVER to QEMU's
//! user network) before the stack exists (NW-3 brings smoltcp). Builders and bounds-checked parsers; nothing here
//! panics on any input.

use alloc::vec::Vec;

pub const ETH_BROADCAST: [u8; 6] = [0xFF; 6];
pub const ETHERTYPE_IPV4: u16 = 0x0800;
pub const ETHERTYPE_ARP: u16 = 0x0806;
/// IEEE "local experimental" ethertype: frames nobody answers (burst tests).
pub const ETHERTYPE_LOCAL_EXP: u16 = 0x88B5;
const MIN_FRAME: usize = 60;

fn be16(b: &[u8], i: usize) -> Option<u16> {
    Some(u16::from_be_bytes(b.get(i..i + 2)?.try_into().ok()?))
}
fn be32(b: &[u8], i: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(i..i + 4)?.try_into().ok()?))
}
fn arr<const N: usize>(b: &[u8], i: usize) -> Option<[u8; N]> {
    b.get(i..i + N)?.try_into().ok()
}

fn eth(dst: [u8; 6], src: [u8; 6], ty: u16) -> Vec<u8> {
    let mut f = Vec::with_capacity(MIN_FRAME);
    f.extend_from_slice(&dst);
    f.extend_from_slice(&src);
    f.extend_from_slice(&ty.to_be_bytes());
    f
}

fn pad(mut f: Vec<u8>) -> Vec<u8> {
    if f.len() < MIN_FRAME {
        f.resize(MIN_FRAME, 0);
    }
    f
}

/// Internet checksum (RFC 1071) over `data`.
pub fn checksum(data: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    for c in data.chunks(2) {
        sum += (c[0] as u32) << 8 | c.get(1).copied().unwrap_or(0) as u32;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

/// "Who has `target_ip`? Tell `sender_ip`" from `mac`, broadcast.
pub fn arp_request(mac: [u8; 6], sender_ip: [u8; 4], target_ip: [u8; 4]) -> Vec<u8> {
    let mut f = eth(ETH_BROADCAST, mac, ETHERTYPE_ARP);
    f.extend_from_slice(&[0, 1, 8, 0, 6, 4, 0, 1]);
    f.extend_from_slice(&mac);
    f.extend_from_slice(&sender_ip);
    f.extend_from_slice(&[0; 6]);
    f.extend_from_slice(&target_ip);
    pad(f)
}

/// If `f` is an ARP reply from `ip` addressed to `our_mac`, the MAC that owns `ip`.
pub fn parse_arp_reply(f: &[u8], our_mac: [u8; 6], ip: [u8; 4]) -> Option<[u8; 6]> {
    if be16(f, 12)? != ETHERTYPE_ARP || f.get(14..22)? != [0, 1, 8, 0, 6, 4, 0, 2] {
        return None;
    }
    let (sha, spa, tha) = (arr::<6>(f, 22)?, arr::<4>(f, 28)?, arr::<6>(f, 32)?);
    (spa == ip && tha == our_mac).then_some(sha)
}

/// A frame of `len` bytes (at least the 14-byte header) of the local experimental ethertype, broadcast.
pub fn junk(mac: [u8; 6], len: usize, seq: u32) -> Vec<u8> {
    let mut f = eth(ETH_BROADCAST, mac, ETHERTYPE_LOCAL_EXP);
    f.extend_from_slice(&seq.to_be_bytes());
    f.resize(len.max(18), 0x5A);
    f
}

const DHCP_MAGIC: u32 = 0x6382_5363;
const BOOTP_FIXED: usize = 236;

/// A DHCP DISCOVER (broadcast flag set, so the server answers to the broadcast address while we have no IP).
pub fn dhcp_discover(mac: [u8; 6], xid: u32) -> Vec<u8> {
    let mut bootp = alloc::vec![0u8; BOOTP_FIXED];
    bootp[0] = 1; // BOOTREQUEST
    bootp[1] = 1; // Ethernet
    bootp[2] = 6;
    bootp[4..8].copy_from_slice(&xid.to_be_bytes());
    bootp[10..12].copy_from_slice(&0x8000u16.to_be_bytes());
    bootp[28..34].copy_from_slice(&mac);
    bootp.extend_from_slice(&DHCP_MAGIC.to_be_bytes());
    bootp.extend_from_slice(&[53, 1, 1]); // DISCOVER
    bootp.extend_from_slice(&[61, 7, 1]); // client id = hardware type + MAC
    bootp.extend_from_slice(&mac);
    bootp.extend_from_slice(&[55, 3, 1, 3, 6]); // subnet, router, DNS
    bootp.push(255);
    let udp_len = 8 + bootp.len();
    let ip_len = 20 + udp_len;
    let mut ip = alloc::vec![0x45, 0, (ip_len >> 8) as u8, ip_len as u8, 0, 0, 0, 0, 64, 17, 0, 0];
    ip.extend_from_slice(&[0, 0, 0, 0]);
    ip.extend_from_slice(&[255, 255, 255, 255]);
    let c = checksum(&ip);
    ip[10..12].copy_from_slice(&c.to_be_bytes());
    let mut f = eth(ETH_BROADCAST, mac, ETHERTYPE_IPV4);
    f.extend_from_slice(&ip);
    f.extend_from_slice(&68u16.to_be_bytes());
    f.extend_from_slice(&67u16.to_be_bytes());
    f.extend_from_slice(&(udp_len as u16).to_be_bytes());
    f.extend_from_slice(&[0, 0]); // UDP checksum optional over IPv4
    f.extend_from_slice(&bootp);
    pad(f)
}

/// Whether `f` is a DHCP DISCOVER we send (UDP 68 -> 67, option 53 = 1). The stack counts its tries with this.
pub fn is_dhcp_discover(f: &[u8]) -> bool {
    let check = || -> Option<bool> {
        if be16(f, 12)? != ETHERTYPE_IPV4 || *f.get(23)? != 17 {
            return None;
        }
        let ihl = (*f.get(14)? & 0x0F) as usize * 4;
        let udp = 14 + ihl;
        if be16(f, udp)? != 68 || be16(f, udp + 2)? != 67 {
            return None;
        }
        let b = udp + 8;
        if be32(f, b + BOOTP_FIXED)? != DHCP_MAGIC {
            return None;
        }
        let mut i = b + BOOTP_FIXED + 4;
        while let Some(&code) = f.get(i) {
            match code {
                0 => i += 1,
                255 => return None,
                53 => return Some(*f.get(i + 2)? == 1),
                _ => i += 2 + *f.get(i + 1)? as usize,
            }
        }
        None
    };
    check().unwrap_or(false)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DhcpReply {
    /// DHCP message type (2 = OFFER, 5 = ACK, 6 = NAK).
    pub kind: u8,
    pub your_ip: [u8; 4],
    /// Option 54, if present.
    pub server: Option<[u8; 4]>,
}

/// If `f` is a DHCP reply (UDP 67 -> 68) for transaction `xid`, its type and offered address.
pub fn parse_dhcp_reply(f: &[u8], xid: u32) -> Option<DhcpReply> {
    if be16(f, 12)? != ETHERTYPE_IPV4 {
        return None;
    }
    let ip = f.get(14..)?;
    let (vihl, total) = (*ip.first()?, be16(ip, 2)? as usize);
    let ihl = (vihl & 0x0F) as usize * 4;
    if vihl >> 4 != 4 || ihl < 20 || total < ihl + 8 || total > ip.len() || *ip.get(9)? != 17 {
        return None;
    }
    if checksum(ip.get(..ihl)?) != 0 || be16(ip, 6)? & 0x3FFF != 0 {
        return None; // bad header checksum, or a fragment
    }
    let udp = ip.get(ihl..total)?;
    let ulen = be16(udp, 4)? as usize;
    if be16(udp, 0)? != 67 || be16(udp, 2)? != 68 || ulen < 8 || ulen > udp.len() {
        return None;
    }
    let b = udp.get(8..ulen)?;
    if *b.first()? != 2 || be32(b, 4)? != xid || be32(b, BOOTP_FIXED)? != DHCP_MAGIC {
        return None;
    }
    let your_ip = arr::<4>(b, 16)?;
    let (mut kind, mut server) = (None, None);
    let mut i = BOOTP_FIXED + 4;
    while let Some(&code) = b.get(i) {
        match code {
            0 => i += 1,
            255 => break,
            _ => {
                let len = *b.get(i + 1)? as usize;
                let val = b.get(i + 2..i + 2 + len)?;
                match (code, len) {
                    (53, 1) => kind = Some(val[0]),
                    (54, 4) => server = val.try_into().ok(),
                    _ => {}
                }
                i += 2 + len;
            }
        }
    }
    Some(DhcpReply { kind: kind?, your_ip, server })
}

#[cfg(test)]
mod tests {
    use super::*;
    const MAC: [u8; 6] = [0x52, 0x54, 0, 0x12, 0x34, 0x56];

    #[test]
    fn checksum_known_value() {
        // RFC 1071 example header from Wikipedia's "IPv4 header checksum".
        let h = [0x45, 0x00, 0x00, 0x73, 0x00, 0x00, 0x40, 0x00, 0x40, 0x11, 0x00, 0x00, 0xc0, 0xa8, 0x00, 0x01, 0xc0,
            0xa8, 0x00, 0xc7];
        assert_eq!(checksum(&h), 0xb861);
        assert_eq!(checksum(&[0x01]), !0x0100);
    }

    #[test]
    fn arp_request_layout_and_reply() {
        let f = arp_request(MAC, [10, 0, 2, 15], [10, 0, 2, 2]);
        assert_eq!(f.len(), 60);
        assert_eq!(&f[0..6], &ETH_BROADCAST);
        assert_eq!(be16(&f, 12), Some(ETHERTYPE_ARP));
        assert_eq!(&f[38..42], &[10, 0, 2, 2]);
        // Turn it into the reply the gateway would send.
        let gw = [0x52, 0x55, 10, 0, 2, 2];
        let mut r = f.clone();
        r[0..6].copy_from_slice(&MAC);
        r[6..12].copy_from_slice(&gw);
        r[21] = 2;
        r[22..28].copy_from_slice(&gw);
        r[28..32].copy_from_slice(&[10, 0, 2, 2]);
        r[32..38].copy_from_slice(&MAC);
        r[38..42].copy_from_slice(&[10, 0, 2, 15]);
        assert_eq!(parse_arp_reply(&r, MAC, [10, 0, 2, 2]), Some(gw));
        assert_eq!(parse_arp_reply(&r, MAC, [10, 0, 2, 3]), None, "other IP");
        assert_eq!(parse_arp_reply(&f, MAC, [10, 0, 2, 2]), None, "a request is no reply");
        // Cut before the end of the target MAC: never a reply, never a panic.
        for n in 0..38 {
            assert_eq!(parse_arp_reply(&r[..n], MAC, [10, 0, 2, 2]), None);
        }
    }

    /// The OFFER QEMU's slirp sends for `xid`, built by turning our DISCOVER around.
    fn offer(xid: u32) -> Vec<u8> {
        let d = dhcp_discover(MAC, xid);
        let mut f = d.clone();
        // swap ports, op = reply, yiaddr, options = OFFER + server id
        let ip = 14;
        let udp = ip + 20;
        let b = udp + 8;
        f[udp..udp + 2].copy_from_slice(&67u16.to_be_bytes());
        f[udp + 2..udp + 4].copy_from_slice(&68u16.to_be_bytes());
        f[b] = 2;
        f[b + 16..b + 20].copy_from_slice(&[10, 0, 2, 15]);
        let o = b + BOOTP_FIXED + 4;
        f[o..o + 3].copy_from_slice(&[53, 1, 2]);
        f[o + 3..o + 9].copy_from_slice(&[54, 4, 10, 0, 2, 2]);
        f[o + 9] = 255;
        f
    }

    #[test]
    fn discover_is_well_formed() {
        let f = dhcp_discover(MAC, 0xABCD_1234);
        assert_eq!(be16(&f, 12), Some(ETHERTYPE_IPV4));
        assert_eq!(checksum(&f[14..34]), 0, "IP header checksum");
        let total = be16(&f, 16).unwrap() as usize;
        assert_eq!(14 + total, f.len(), "no padding needed, the DISCOVER is longer than 60 bytes");
        assert_eq!(be16(&f, 34), Some(68));
        assert_eq!(be16(&f, 36), Some(67));
        assert_eq!(be32(&f, 42 + 4), Some(0xABCD_1234));
        assert_eq!(&f[42 + 28..42 + 34], &MAC);
        // Our own DISCOVER is not a reply.
        assert_eq!(parse_dhcp_reply(&f, 0xABCD_1234), None);
    }

    #[test]
    fn offer_is_parsed() {
        let f = offer(7);
        let r = parse_dhcp_reply(&f, 7).unwrap();
        assert_eq!(r, DhcpReply { kind: 2, your_ip: [10, 0, 2, 15], server: Some([10, 0, 2, 2]) });
        assert_eq!(parse_dhcp_reply(&f, 8), None, "other transaction");
    }

    #[test]
    fn truncated_and_mangled_replies_never_panic() {
        let f = offer(7);
        for n in 0..f.len() {
            let _ = parse_dhcp_reply(&f[..n], 7);
        }
        // Every single byte flipped: no panic (result may or may not parse).
        for i in 0..f.len() {
            let mut g = f.clone();
            g[i] ^= 0xFF;
            let _ = parse_dhcp_reply(&g, 7);
        }
        // An option length running past the end.
        let mut g = f.clone();
        let o = 14 + 20 + 8 + BOOTP_FIXED + 4;
        g[o + 1] = 200;
        assert_eq!(parse_dhcp_reply(&g, 7), None);
    }

    #[test]
    fn discover_is_recognised() {
        assert!(is_dhcp_discover(&dhcp_discover(MAC, 3)));
        assert!(!is_dhcp_discover(&offer(3)), "an OFFER is no DISCOVER");
        assert!(!is_dhcp_discover(&arp_request(MAC, [10, 0, 2, 15], [10, 0, 2, 2])));
        let d = dhcp_discover(MAC, 3);
        for n in 0..d.len() {
            assert!(!is_dhcp_discover(&d[..n]) || n > 280, "cut at {n}");
        }
    }

    #[test]
    fn junk_frames() {
        let f = junk(MAC, 1514, 9);
        assert_eq!(f.len(), 1514);
        assert_eq!(be16(&f, 12), Some(ETHERTYPE_LOCAL_EXP));
        assert_eq!(be32(&f, 14), Some(9));
        assert_eq!(junk(MAC, 0, 1).len(), 18);
    }
}
