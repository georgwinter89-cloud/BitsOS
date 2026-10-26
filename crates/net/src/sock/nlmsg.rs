//! Netlink message layout (include/uapi/linux/netlink.h, rtnetlink.h; lib/nlattr.c): headers, attributes and a
//! builder for the kernel's answers. Every message and attribute is padded to 4 bytes with zeros.

use alloc::vec::Vec;

pub const NLMSG_HDRLEN: usize = 16;
pub const NLA_HDRLEN: usize = 4;

pub const fn align(n: usize) -> usize {
    (n + 3) & !3
}

// nlmsg_type
pub const NLMSG_NOOP: u16 = 1;
pub const NLMSG_ERROR: u16 = 2;
pub const NLMSG_DONE: u16 = 3;
pub const NLMSG_MIN_TYPE: u16 = 16;

// nlmsg_flags
pub const NLM_F_REQUEST: u16 = 0x1;
pub const NLM_F_MULTI: u16 = 0x2;
pub const NLM_F_ACK: u16 = 0x4;
pub const NLM_F_DUMP_FILTERED: u16 = 0x20;
/// NLM_F_ROOT | NLM_F_MATCH: either bit asks for a dump (`nlh->nlmsg_flags & NLM_F_DUMP`).
pub const NLM_F_DUMP: u16 = 0x300;
pub const NLM_F_CAPPED: u16 = 0x100;
pub const NLM_F_ACK_TLVS: u16 = 0x200;

/// `NLMSGERR_ATTR_MSG`: the extended ack's text.
pub const NLMSGERR_ATTR_MSG: u16 = 1;

/// A message header read from a request.
#[derive(Clone, Copy, Debug)]
pub struct Hdr {
    pub len: u32,
    pub ty: u16,
    pub flags: u16,
    pub seq: u32,
    pub pid: u32,
}

impl Hdr {
    pub fn parse(b: &[u8]) -> Option<Hdr> {
        if b.len() < NLMSG_HDRLEN {
            return None;
        }
        let u32_at = |o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
        Some(Hdr { len: u32_at(0), ty: u16::from_le_bytes([b[4], b[5]]), flags: u16::from_le_bytes([b[6], b[7]]), seq: u32_at(8), pid: u32_at(12) })
    }
    pub fn bytes(&self) -> [u8; NLMSG_HDRLEN] {
        let mut h = [0u8; NLMSG_HDRLEN];
        h[0..4].copy_from_slice(&self.len.to_le_bytes());
        h[4..6].copy_from_slice(&self.ty.to_le_bytes());
        h[6..8].copy_from_slice(&self.flags.to_le_bytes());
        h[8..12].copy_from_slice(&self.seq.to_le_bytes());
        h[12..16].copy_from_slice(&self.pid.to_le_bytes());
        h
    }
}

/// One message being built: header, fixed payload, attributes (`nlmsg_put`, `nla_put*`, `nlmsg_end`).
pub struct Msg {
    b: Vec<u8>,
    nests: Vec<usize>,
}

impl Msg {
    pub fn new(ty: u16, flags: u16, seq: u32, pid: u32, fixed: &[u8]) -> Msg {
        let mut b = Vec::with_capacity(256);
        b.extend_from_slice(&Hdr { len: 0, ty, flags, seq, pid }.bytes());
        b.extend_from_slice(fixed);
        b.resize(align(b.len()), 0);
        Msg { b, nests: Vec::new() }
    }
    pub fn attr(&mut self, ty: u16, data: &[u8]) -> &mut Self {
        let len = NLA_HDRLEN + data.len();
        self.b.extend_from_slice(&(len as u16).to_le_bytes());
        self.b.extend_from_slice(&ty.to_le_bytes());
        self.b.extend_from_slice(data);
        self.b.resize(align(self.b.len()), 0);
        self
    }
    pub fn u8(&mut self, ty: u16, v: u8) -> &mut Self {
        self.attr(ty, &[v])
    }
    pub fn u32(&mut self, ty: u16, v: u32) -> &mut Self {
        self.attr(ty, &v.to_le_bytes())
    }
    /// A string with its NUL (`nla_put_string`).
    pub fn str(&mut self, ty: u16, s: &[u8]) -> &mut Self {
        let mut v = Vec::with_capacity(s.len() + 1);
        v.extend_from_slice(s);
        v.push(0);
        self.attr(ty, &v)
    }
    pub fn nest_start(&mut self, ty: u16) -> &mut Self {
        self.nests.push(self.b.len());
        self.b.extend_from_slice(&0u16.to_le_bytes());
        self.b.extend_from_slice(&(ty | 0x8000).to_le_bytes());
        self
    }
    pub fn nest_end(&mut self) -> &mut Self {
        if let Some(at) = self.nests.pop() {
            let len = (self.b.len() - at) as u16;
            self.b[at..at + 2].copy_from_slice(&len.to_le_bytes());
        }
        self
    }
    /// Appends raw bytes (an echoed request) and pads.
    pub fn raw(&mut self, data: &[u8]) -> &mut Self {
        self.b.extend_from_slice(data);
        self.b.resize(align(self.b.len()), 0);
        self
    }
    pub fn set_flags(&mut self, f: u16) {
        self.b[6..8].copy_from_slice(&f.to_le_bytes());
    }
    /// The finished message (nlmsg_len = everything put).
    pub fn done(mut self) -> Vec<u8> {
        let len = self.b.len() as u32;
        self.b[0..4].copy_from_slice(&len.to_le_bytes());
        self.b
    }
}

/// The attributes after a fixed header (`nlmsg_parse_deprecated`: a short or overlong attribute ends the list,
/// trailing bytes are ignored). Pairs of (type without the nested/byte-order bits, payload).
pub fn attrs(mut b: &[u8]) -> Vec<(u16, &[u8])> {
    let mut v = Vec::new();
    while b.len() >= NLA_HDRLEN {
        let len = u16::from_le_bytes([b[0], b[1]]) as usize;
        let ty = u16::from_le_bytes([b[2], b[3]]) & 0x3fff;
        if len < NLA_HDRLEN || len > b.len() {
            break;
        }
        v.push((ty, &b[NLA_HDRLEN..len]));
        b = &b[align(len).min(b.len())..];
    }
    v
}

/// Attributes in strict mode (`nlmsg_parse_deprecated_strict`, NL_VALIDATE_TRAILING): bytes left after the last
/// well-formed attribute (a malformed one included) are an error (EINVAL).
pub fn attrs_strict(b: &[u8]) -> Result<Vec<(u16, &[u8])>, i64> {
    let v = attrs(b);
    let used: usize = v.iter().map(|(_, d)| align(NLA_HDRLEN + d.len())).sum();
    if used < b.len() {
        return Err(uapi::errno::EINVAL);
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_and_parse() {
        let mut m = Msg::new(16, NLM_F_MULTI, 7, 99, &[1, 2, 3]);
        m.str(3, b"lo").u32(4, 65536).u8(16, 0).nest_start(43).u8(2, 0).nest_end();
        let b = m.done();
        let h = Hdr::parse(&b).unwrap();
        assert_eq!((h.len as usize, h.ty, h.flags, h.seq, h.pid), (b.len(), 16, NLM_F_MULTI, 7, 99));
        assert_eq!(b.len() % 4, 0);
        let a = attrs(&b[NLMSG_HDRLEN + 4..]);
        assert_eq!(a[0], (3, &b"lo\0"[..]));
        assert_eq!(a[1], (4, &65536u32.to_le_bytes()[..]));
        assert_eq!(a[2].0, 16);
        assert_eq!(a[3].0, 43, "nested flag stripped");
        assert_eq!(a[3].1.len(), 8, "one u8 attribute padded");
        // A broken attribute ends the list; strict parsing refuses the leftover.
        let junk = [8u8, 0, 3, 0, b'a', 0, 0, 0, 2, 0, 1, 0];
        assert_eq!(attrs(&junk).len(), 1);
        assert!(attrs_strict(&junk).is_err());
        assert_eq!(attrs_strict(&junk[..8]).unwrap().len(), 1);
    }
}
