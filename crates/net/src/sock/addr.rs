//! Socket addresses as programs pass them (`struct sockaddr_un`, `struct sockaddr_in`): parsing with Linux's
//! checks and the bytes `getsockname`/`getpeername`/`recvfrom` give back.

use super::abi::*;
use crate::{Ipv4, SockAddr};
use alloc::vec::Vec;
use uapi::errno::*;
use vfs::Errno;

/// A bound AF_UNIX name exactly as Linux keeps it (`struct unix_address`): the family, then either a path with
/// its NUL (`len` = strlen + 3) or a NUL and the abstract name (`len` = the length the program gave).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct UnixName {
    raw: Vec<u8>,
}

impl UnixName {
    /// What `getsockname` answers for an unbound socket: the family alone.
    pub fn unbound() -> Vec<u8> {
        AF_UNIX.to_le_bytes().to_vec()
    }
    pub fn bytes(&self) -> &[u8] {
        &self.raw
    }
    pub fn is_abstract(&self) -> bool {
        self.raw.get(SUN_PATH_OFF) == Some(&0)
    }
    /// The file name of a path name (without the NUL).
    pub fn path(&self) -> Option<&[u8]> {
        if self.is_abstract() {
            return None;
        }
        let p = &self.raw[SUN_PATH_OFF..];
        Some(p.strip_suffix(&[0]).unwrap_or(p))
    }
    /// The abstract name after its leading NUL.
    pub fn abstract_name(&self) -> Option<&[u8]> {
        self.is_abstract().then(|| &self.raw[SUN_PATH_OFF + 1..])
    }
    /// An autobind name: `\0` and five hex digits (`unix_autobind`, len 8).
    pub fn auto(n: u32) -> UnixName {
        let mut raw = AF_UNIX.to_le_bytes().to_vec();
        raw.push(0);
        let hex = b"0123456789abcdef";
        for shift in [16, 12, 8, 4, 0] {
            raw.push(hex[((n >> shift) & 0xf) as usize]);
        }
        UnixName { raw }
    }
}

/// An AF_UNIX address argument, checked like `unix_validate_addr` + `unix_mkname_bsd`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UnixTarget {
    /// `bind` with only the family (`addr_len == offsetof(sun_path)`): pick a free abstract name.
    Auto,
    Name(UnixName),
}

/// `unix_validate_addr`: more than the family, at most `sizeof(sockaddr_un)`, family AF_UNIX; then the name.
/// `allow_auto`: bind's special case (exactly the family) comes first.
pub fn parse_unix(raw: &[u8], allow_auto: bool) -> Result<UnixTarget, Errno> {
    let fam = family(raw);
    if allow_auto && raw.len() == SUN_PATH_OFF && fam == Some(AF_UNIX) {
        return Ok(UnixTarget::Auto);
    }
    if raw.len() <= SUN_PATH_OFF || raw.len() > SOCKADDR_UN || fam != Some(AF_UNIX) {
        return Err(EINVAL);
    }
    let path = &raw[SUN_PATH_OFF..];
    if path[0] == 0 {
        return Ok(UnixTarget::Name(UnixName { raw: raw.to_vec() }));
    }
    // A path name ends at its first NUL or at the end of what was given (Linux terminates the copy there).
    let n = path.iter().position(|&b| b == 0).unwrap_or(path.len());
    let mut v = Vec::with_capacity(SUN_PATH_OFF + n + 1);
    v.extend_from_slice(&raw[..SUN_PATH_OFF]);
    v.extend_from_slice(&path[..n]);
    v.push(0);
    Ok(UnixTarget::Name(UnixName { raw: v }))
}

pub fn family(raw: &[u8]) -> Option<u16> {
    (raw.len() >= 2).then(|| u16::from_le_bytes([raw[0], raw[1]]))
}

/// `struct sockaddr_in` from a program: the family is the caller's to check (bind and connect differ).
pub fn parse_in(raw: &[u8]) -> Option<SockAddr> {
    if raw.len() < SOCKADDR_IN {
        return None;
    }
    Some(SockAddr { ip: Ipv4([raw[4], raw[5], raw[6], raw[7]]), port: u16::from_be_bytes([raw[2], raw[3]]) })
}

/// The 16 bytes of `struct sockaddr_in` for `a`.
pub fn encode_in(a: SockAddr) -> Vec<u8> {
    let mut v = Vec::with_capacity(SOCKADDR_IN);
    v.extend_from_slice(&AF_INET.to_le_bytes());
    v.extend_from_slice(&a.port.to_be_bytes());
    v.extend_from_slice(&a.ip.0);
    v.extend_from_slice(&[0u8; 8]);
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sun(path: &[u8], len: Option<usize>) -> Vec<u8> {
        let mut v = AF_UNIX.to_le_bytes().to_vec();
        v.extend_from_slice(path);
        if let Some(l) = len {
            v.resize(l, 0);
        }
        v
    }

    #[test]
    fn unix_names_like_linux() {
        assert_eq!(parse_unix(&sun(b"", None), true), Ok(UnixTarget::Auto));
        assert_eq!(parse_unix(&sun(b"", None), false), Err(EINVAL));
        assert_eq!(parse_unix(&[1], true), Err(EINVAL));
        assert_eq!(parse_unix(&sun(b"x", Some(111)), true), Err(EINVAL));
        let mut wrong = sun(b"/tmp/s", None);
        wrong[0] = 2;
        assert_eq!(parse_unix(&wrong, true), Err(EINVAL));
        // A path stops at its NUL; the stored name carries one NUL: strlen + 3.
        let UnixTarget::Name(n) = parse_unix(&sun(b"/tmp/s\0garbage", None), true).unwrap() else { panic!() };
        assert_eq!(n.bytes().len(), 2 + 6 + 1);
        assert_eq!(n.path(), Some(&b"/tmp/s"[..]));
        // ...or at the end of what was given (no NUL at all, 108 characters).
        let long = alloc::vec![b'a'; 108];
        let UnixTarget::Name(n) = parse_unix(&sun(&long, None), true).unwrap() else { panic!() };
        assert_eq!(n.bytes().len(), 111);
        // Abstract names keep every byte the program gave, NULs included.
        let UnixTarget::Name(a) = parse_unix(&sun(b"\0wl\0x", None), true).unwrap() else { panic!() };
        assert!(a.is_abstract());
        assert_eq!(a.abstract_name(), Some(&b"wl\0x"[..]));
        assert_eq!(a.bytes().len(), 7);
        assert_eq!(UnixName::auto(0xabc).bytes(), b"\x01\x00\x0000abc");
    }

    #[test]
    fn inet_round_trip() {
        let a = SockAddr { ip: Ipv4([10, 0, 2, 2]), port: 8080 };
        let raw = encode_in(a);
        assert_eq!(raw.len(), 16);
        assert_eq!(&raw[2..4], &[0x1f, 0x90]);
        assert_eq!(parse_in(&raw), Some(a));
        assert_eq!(parse_in(&raw[..15]), None);
    }
}
