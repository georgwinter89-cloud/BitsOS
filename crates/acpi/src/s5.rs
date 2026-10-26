//! `\_S5` (soft off) without an AML interpreter: a byte scan for `Name(_S5, Package(...))` as V1
//! `acpi/s5.asm` does it. The first good one counts (also inside If/Else); an `_S5` a Method returns is
//! not a `Name` and is refused. Every read stays inside the package and the table.

use crate::bytes::u8_at;

const NAME_OP: u8 = 0x08;
const ROOT_CHAR: u8 = 0x5C; // '\'
const PACKAGE_OP: u8 = 0x12;
const ZERO_OP: u8 = 0x00;
const ONE_OP: u8 = 0x01;
const BYTE_PREFIX: u8 = 0x0A;
const WORD_PREFIX: u8 = 0x0B;
const DWORD_PREFIX: u8 = 0x0C;

/// SLP_TYP values for PM1a and PM1b control (3 bits each).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct S5 {
    pub typ_a: u8,
    pub typ_b: u8,
}

/// Searches the AML of one table (the bytes after its 36-byte header).
pub fn find(table: &[u8]) -> Option<S5> {
    let aml = table.get(crate::sdt::HEADER_LEN..)?;
    let mut i = 0;
    while i + 4 <= aml.len() {
        if &aml[i..i + 4] == b"_S5_"
            && let Some(s) = name_at(aml, i)
        {
            return Some(s);
        }
        i += 1;
    }
    None
}

/// `aml[i..i+4]` is "_S5_": a `NameOp` (or `NameOp '\'`) before it and a package of constants after it.
fn name_at(aml: &[u8], i: usize) -> Option<S5> {
    let before = aml.get(..i)?;
    if !matches!(before, [.., NAME_OP] | [.., NAME_OP, ROOT_CHAR]) {
        return None;
    }
    let mut p = i + 4;
    if u8_at(aml, p)? != PACKAGE_OP {
        return None;
    }
    p += 1;
    let (len, len_bytes) = pkg_length(aml, p)?;
    let end = p.checked_add(len)?;
    if end > aml.len() || p + len_bytes >= end {
        return None;
    }
    p += len_bytes;
    let count = aml[p];
    p += 1;
    if count == 0 {
        return None;
    }
    let (a, next) = constant(&aml[..end], p)?;
    let b = if count == 1 { a } else { constant(&aml[..end], next)?.0 };
    Some(S5 { typ_a: a, typ_b: b })
}

/// AML PkgLength at `p`: the value (counting itself) and how many bytes it takes (ACPI 6.5 §20.2.4).
pub fn pkg_length(aml: &[u8], p: usize) -> Option<(usize, usize)> {
    let lead = u8_at(aml, p)?;
    let follow = (lead >> 6) as usize;
    if follow == 0 {
        return Some(((lead & 0x3F) as usize, 1));
    }
    let mut v = (lead & 0x0F) as usize;
    for k in 0..follow {
        v |= (u8_at(aml, p + 1 + k)? as usize) << (4 + 8 * k);
    }
    Some((v, 1 + follow))
}

/// A constant 0..=7 at `p` (ZeroOp, OneOp, Byte/Word/DWord prefix): value and the offset after it.
fn constant(aml: &[u8], p: usize) -> Option<(u8, usize)> {
    let op = u8_at(aml, p)?;
    let (v, n) = match op {
        ZERO_OP => (0u64, 0),
        ONE_OP => (1, 0),
        BYTE_PREFIX => (u8_at(aml, p + 1)? as u64, 1),
        WORD_PREFIX => (crate::bytes::u16_at(aml, p + 1)? as u64, 2),
        DWORD_PREFIX => (crate::bytes::u32_at(aml, p + 1)? as u64, 4),
        _ => return None,
    };
    (v <= 7).then_some((v as u8, p + 1 + n))
}
