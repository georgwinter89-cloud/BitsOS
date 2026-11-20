//! Writable `/proc/sys` numbers (FS-3: the pipe limits), parsed as linux-7.3.1/kernel/sysctl.c does
//! (`proc_get_long` with `strtoul_lenient`, base 0, :276/:237; one value of `do_proc_vec`, :646).

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
    match parse(data)? {
        Some(n) if n.neg => Err(EINVAL),
        n => Ok(n.map(|n| n.mag)),
    }
}

/// A number as `proc_get_long` reads it: the sign apart from the magnitude, and the index right behind it.
struct Number {
    neg: bool,
    mag: u64,
    end: usize,
}

fn parse(data: &[u8]) -> Result<Option<Number>, Errno> {
    let data = &data[..data.len().min(4095)];
    let Some(start) = data.iter().position(|&c| !is_space(c)) else {
        return Ok(None);
    };
    let rest = &data[start..];
    let tmp = &rest[..rest.len().min(TMPBUFLEN - 1)];
    // A '-' counts as a sign only when more input follows (`*size > 1`); the digit check comes after it.
    let neg = tmp[0] == b'-' && rest.len() > 1;
    let sign = usize::from(neg);
    let (radix, skip) = match &tmp[sign..] {
        [b'0', x, h, ..] if (*x | 0x20) == b'x' && h.is_ascii_hexdigit() => (16, 2),
        [b'0', ..] => (8, 0),
        [d, ..] if d.is_ascii_digit() => (10, 0),
        _ => return Err(EINVAL),
    };
    let mut mag: u64 = 0;
    let mut len = sign + skip;
    while let Some(d) = tmp.get(len).and_then(|&c| (c as char).to_digit(radix)) {
        mag = mag.checked_mul(radix as u64).and_then(|v| v.checked_add(d as u64)).ok_or(EINVAL)?;
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
    Ok(Some(Number { neg, mag, end: start + len }))
}

/// `do_proc_vec` (sysctl.c:646) writing up to `n` values: an empty write changes nothing, one of only white space
/// is EINVAL (`first`, :731), `conv` (index, sign, magnitude) stores each value as it is read and a refusal there
/// is EINVAL. A bad value after a stored one fails the write but keeps what was stored, as Linux. The count
/// answered ends behind the last number and the white space after it (`*lenp -= left`), so "5 junk" is 2.
fn write_vec(data: &[u8], n: usize, mut conv: impl FnMut(usize, bool, u64) -> Result<(), Errno>) -> Result<usize, Errno> {
    if data.is_empty() {
        return Ok(0);
    }
    // Only the first 4095 bytes are looked at, but what lies past them counts as written (`left` was clipped).
    let seen = &data[..data.len().min(4095)];
    let skip = |at: usize| seen[at..].iter().position(|&c| !is_space(c)).map_or(seen.len(), |i| at + i);
    let mut at = 0;
    for i in 0..n {
        at = skip(at);
        if at == seen.len() {
            if i == 0 {
                return Err(EINVAL);
            }
            break;
        }
        let num = parse(&seen[at..])?.ok_or(EINVAL)?;
        conv(i, num.neg, num.mag).map_err(|_| EINVAL)?;
        at += num.end;
    }
    Ok(data.len() - (seen.len() - skip(at)))
}

/// One value of `do_proc_vec` for a one-element table.
fn write_with(data: &[u8], conv: impl FnOnce(bool, u64) -> Result<(), Errno>) -> Result<usize, Errno> {
    let mut conv = Some(conv);
    write_vec(data, 1, |_, neg, mag| conv.take().map_or(Err(EINVAL), |c| c(neg, mag)))
}

/// An unsigned knob (`proc_doulongvec_minmax`, `proc_douintvec`): a sign is EINVAL ("neg && is_unsigned", :706).
pub fn write_value(data: &[u8], set: impl FnOnce(u64) -> Result<(), Errno>) -> Result<usize, Errno> {
    write_with(data, |neg, mag| if neg { Err(EINVAL) } else { set(mag) })
}

/// The int `proc_int_u2k_conv_uop` (:506) makes of a sign and a magnitude: -(INT_MAX + 1)..=INT_MAX or EINVAL.
fn to_int(neg: bool, mag: u64) -> Result<i64, Errno> {
    const INT_MAX: u64 = i32::MAX as u64;
    match neg {
        true if mag <= INT_MAX + 1 => Ok(-(mag as i64)),
        false if mag <= INT_MAX => Ok(mag as i64),
        _ => Err(EINVAL),
    }
}

/// An int knob (`proc_dointvec`, `_minmax`): `set` gets the int and checks the knob's range ("-0" is 0).
pub fn write_int(data: &[u8], set: impl FnOnce(i64) -> Result<(), Errno>) -> Result<usize, Errno> {
    write_with(data, |neg, mag| set(to_int(neg, mag)?))
}

/// An int array (`proc_dointvec` over `n` ints, e.g. kernel.sem): `set` gets each index and int as it is read.
pub fn write_ints(data: &[u8], n: usize, mut set: impl FnMut(usize, i64) -> Result<(), Errno>) -> Result<usize, Errno> {
    write_vec(data, n, |i, neg, mag| set(i, to_int(neg, mag)?))
}

/// An int knob whose range starts at 0 or above: a negative value is out of it (EINVAL).
pub fn non_negative(v: i64) -> Result<u64, Errno> {
    u64::try_from(v).map_err(|_| EINVAL)
}

/// One unsigned number under /proc/sys (`proc_doulongvec_minmax`, `proc_douintvec`): read as "<value>\n",
/// written by root (the file's mode decides who may open it for writing) as `write_value` takes it.
pub struct Knob {
    pub get: fn() -> u64,
    pub set: fn(u64) -> Result<(), Errno>,
}

impl FileSource for Knob {
    fn render(&self, _who: Cred) -> Result<Vec<u8>, Errno> {
        Ok(format!("{}\n", (self.get)()).into_bytes())
    }
    fn write(&self, _who: Cred, data: &[u8]) -> Result<usize, Errno> {
        write_value(data, self.set)
    }
}

/// One int number under /proc/sys (`proc_dointvec`, `_minmax`), written as `write_int` takes it. Read as
/// unsigned: no V2 knob keeps a negative value (the two that take one do not honour it).
pub struct IntKnob {
    pub get: fn() -> u64,
    pub set: fn(i64) -> Result<(), Errno>,
}

impl FileSource for IntKnob {
    fn render(&self, _who: Cred) -> Result<Vec<u8>, Errno> {
        Ok(format!("{}\n", (self.get)()).into_bytes())
    }
    fn write(&self, _who: Cred, data: &[u8]) -> Result<usize, Errno> {
        write_int(data, self.set)
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

    /// L7-VFS: every type answers alike (one `do_proc_vec`): only white space is EINVAL, a refused value is EINVAL
    /// whatever `set` said, and the count ends behind the number's trailing white space.
    #[test]
    fn writes_as_do_proc_vec_answers_them() {
        let ok = |_: u64| Ok(());
        assert_eq!(write_value(b"", ok), Ok(0));
        assert_eq!(write_value(b" \n", ok), Err(EINVAL));
        assert_eq!(write_value(b"\t", ok), Err(EINVAL));
        assert_eq!(write_value(b"4096\n", ok), Ok(5));
        assert_eq!(write_value(b"  7", ok), Ok(3));
        assert_eq!(write_value(b"5 junk", ok), Ok(2));
        assert_eq!(write_value(b"5 \r\n x", ok), Ok(5));
        assert_eq!(write_value(b"5x", ok), Err(EINVAL));
        assert_eq!(write_value(b"9", |_| Err(ENOSPC)), Err(EINVAL));
        let mut long = alloc::vec![b' '; 5000];
        long[0] = b'1';
        assert_eq!(write_value(&long, ok), Ok(5000), "past the first 4095 bytes counts as written");
        long[4500] = b'x';
        assert_eq!(write_value(&long, ok), Ok(5000), "and is not looked at");
        long[4000] = b'x';
        assert_eq!(write_value(&long, ok), Ok(5000 - 95), "the unread rest of the 4095 bytes is not written");
        let got = core::cell::Cell::new(0);
        assert_eq!(write_value(b"0x10\n", |v| { got.set(v); Ok(()) }), Ok(5));
        assert_eq!(got.get(), 16);
        // Signs: unsigned knobs refuse every one, also "-0" (:706); int knobs hand the value on.
        assert_eq!(write_value(b"-0", ok), Err(EINVAL));
        assert_eq!(write_value(b"-1\n", ok), Err(EINVAL));
        let int = core::cell::Cell::new(7i64);
        let keep = |v: i64| {
            int.set(v);
            Ok(())
        };
        assert_eq!(write_int(b"-0\n", keep), Ok(3));
        assert_eq!(int.get(), 0);
        assert_eq!(write_int(b"-1", keep), Ok(2));
        assert_eq!(int.get(), -1);
        assert_eq!(write_int(b"-2147483648", keep), Ok(11));
        assert_eq!(int.get(), -2147483648);
        assert_eq!(write_int(b"-2147483649", keep), Err(EINVAL));
        assert_eq!(write_int(b"2147483647", keep), Ok(10));
        assert_eq!(write_int(b"2147483648", keep), Err(EINVAL));
        assert_eq!(write_int(b"-0x10", keep), Ok(5));
        assert_eq!(int.get(), -16);
        assert_eq!(write_int(b"-", keep), Err(EINVAL), "a lone '-' is no sign (`*size > 1`)");
        assert_eq!(write_int(b"-\n", keep), Err(EINVAL), "a sign needs a digit");
        assert_eq!(write_int(b"--1", keep), Err(EINVAL));
        assert_eq!(write_int(b" -", keep), Err(EINVAL));
        assert_eq!(write_int(b"-5", |v| non_negative(v).map(|_| ())), Err(EINVAL), "a range from 0");
        assert_eq!(write_int(b"-0", |v| non_negative(v).map(|_| ())), Ok(2));
    }

    /// L7-RESTE: an int array (kernel.sem) as `proc_dointvec` writes it: values in order, at most `n`, the count
    /// behind the last one's white space, and a bad later value fails the write after the earlier ones were stored.
    #[test]
    fn int_arrays_as_do_proc_vec_writes_them() {
        let mut got = alloc::vec::Vec::new();
        assert_eq!(write_ints(b"250 32000 32 128 junk", 4, |i, v| { got.push((i, v)); Ok(()) }), Ok(17));
        assert_eq!(got, [(0, 250), (1, 32000), (2, 32), (3, 128)]);
        got.clear();
        assert_eq!(write_ints(b"11\t22\n", 4, |i, v| { got.push((i, v)); Ok(()) }), Ok(6));
        assert_eq!(got, [(0, 11), (1, 22)]);
        got.clear();
        assert_eq!(write_ints(b"1 2 x", 4, |i, v| { got.push((i, v)); Ok(()) }), Err(EINVAL));
        assert_eq!(got, [(0, 1), (1, 2)], "what came before the bad value stays");
        assert_eq!(write_ints(b" \n", 4, |_, _| Ok(())), Err(EINVAL));
        assert_eq!(write_ints(b"", 4, |_, _| Ok(())), Ok(0));
        assert_eq!(write_ints(b"-5 -0", 4, |_, v| if v <= 0 { Ok(()) } else { Err(EINVAL) }), Ok(5));
        assert_eq!(write_ints(b"1 2147483648", 4, |_, _| Ok(())), Err(EINVAL));
        assert_eq!(write_ints(b"1 2", 4, |i, _| if i == 1 { Err(ERANGE) } else { Ok(()) }), Err(EINVAL));
    }
}
