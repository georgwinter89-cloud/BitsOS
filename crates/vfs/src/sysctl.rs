//! Writable `/proc/sys` numbers (FS-3: the pipe limits), parsed as linux-6.6/kernel/sysctl.c does
//! (`proc_get_long` with `strtoul_lenient`, base 0; `do_proc_douintvec_w`, `__do_proc_doulongvec_minmax`).

use crate::inode::Cred;
use crate::synth::FileSource;
use crate::Errno;
use alloc::format;
use alloc::vec::Vec;
use uapi::errno::*;

/// `TMPBUFLEN`: a number of 21 characters or more is refused (it might be cut off).
const TMPBUFLEN: usize = 22;

fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// One unsigned value from a sysctl write: Ok(None) when there is only white space. EINVAL for a sign, no
/// digit, an overflow, 21 characters, or anything but ' ', '\t', '\n' right behind the number.
pub fn parse_ulong(data: &[u8]) -> Result<Option<u64>, Errno> {
    let data = &data[..data.len().min(4095)];
    let Some(start) = data.iter().position(|&c| !is_space(c)) else {
        return Ok(None);
    };
    let rest = &data[start..];
    let tmp = &rest[..rest.len().min(TMPBUFLEN - 1)];
    if tmp[0] == b'-' {
        return Err(EINVAL);
    }
    let (radix, skip) = match tmp {
        [b'0', x, h, ..] if (*x | 0x20) == b'x' && h.is_ascii_hexdigit() => (16, 2),
        [b'0', ..] => (8, 0),
        [d, ..] if d.is_ascii_digit() => (10, 0),
        _ => return Err(EINVAL),
    };
    let mut v: u64 = 0;
    let mut len = skip;
    while let Some(d) = tmp.get(len).and_then(|&c| (c as char).to_digit(radix)) {
        v = v.checked_mul(radix as u64).and_then(|v| v.checked_add(d as u64)).ok_or(EINVAL)?;
        len += 1;
    }
    if len == TMPBUFLEN - 1 {
        return Err(EINVAL);
    }
    if let Some(&next) = rest.get(len)
        && !matches!(next, b' ' | b'\t' | b'\n')
    {
        return Err(EINVAL);
    }
    Ok(Some(v))
}

/// One number under /proc/sys: read as "<value>\n", written by root (the file's mode decides who may open it
/// for writing). `uint` = a `proc_douintvec` file (only white space: EINVAL), else `proc_doulongvec_minmax`
/// (only white space: nothing changes).
pub struct Knob {
    pub get: fn() -> u64,
    pub set: fn(u64) -> Result<(), Errno>,
    pub uint: bool,
}

impl FileSource for Knob {
    fn render(&self, _who: Cred) -> Result<Vec<u8>, Errno> {
        Ok(format!("{}\n", (self.get)()).into_bytes())
    }
    fn write(&self, _who: Cred, data: &[u8]) -> Result<usize, Errno> {
        if data.is_empty() {
            return Ok(0);
        }
        match parse_ulong(data)? {
            Some(v) => (self.set)(v)?,
            None if self.uint => return Err(EINVAL),
            None => {}
        }
        Ok(data.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn numbers_as_linux_parses_them() {
        assert_eq!(parse_ulong(b"4096\n"), Ok(Some(4096)));
        assert_eq!(parse_ulong(b"  65536"), Ok(Some(65536)));
        assert_eq!(parse_ulong(b"0x1000 junk"), Ok(Some(4096)));
        assert_eq!(parse_ulong(b"010"), Ok(Some(8)));
        assert_eq!(parse_ulong(b"0"), Ok(Some(0)));
        assert_eq!(parse_ulong(b"0x"), Err(EINVAL));
        assert_eq!(parse_ulong(b" \n"), Ok(None));
        assert_eq!(parse_ulong(b"-1"), Err(EINVAL));
        assert_eq!(parse_ulong(b"abc"), Err(EINVAL));
        assert_eq!(parse_ulong(b"12a"), Err(EINVAL));
        assert_eq!(parse_ulong(b"099"), Err(EINVAL));
        assert_eq!(parse_ulong(b"18446744073709551616"), Err(EINVAL));
        assert_eq!(parse_ulong(b"18446744073709551615"), Ok(Some(u64::MAX)));
        assert_eq!(parse_ulong(b"000000000000000000001"), Err(EINVAL));
    }
}
