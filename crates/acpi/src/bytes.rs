//! Bounded little-endian reads. Every table field goes through these, so a short or hostile table yields
//! `None` instead of a panic.

pub fn u8_at(b: &[u8], o: usize) -> Option<u8> {
    b.get(o).copied()
}

pub fn u16_at(b: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(o..o.checked_add(2)?)?.try_into().ok()?))
}

pub fn u32_at(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(o..o.checked_add(4)?)?.try_into().ok()?))
}

pub fn u64_at(b: &[u8], o: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(o..o.checked_add(8)?)?.try_into().ok()?))
}

/// Bytes `o..o+n`, if all are inside.
pub fn slice(b: &[u8], o: usize, n: usize) -> Option<&[u8]> {
    b.get(o..o.checked_add(n)?)
}

/// Sum of all bytes mod 256 (ACPI checksums make it 0).
pub fn sum(b: &[u8]) -> u8 {
    b.iter().fold(0u8, |s, x| s.wrapping_add(*x))
}

/// Printable form of a 4-byte signature or an OEM id (non-printable bytes as '?', trailing spaces cut).
pub fn text(b: &[u8]) -> alloc::string::String {
    let s: alloc::string::String = b.iter().map(|&c| if (0x20..0x7F).contains(&c) { c as char } else { '?' }).collect();
    alloc::string::String::from(s.trim_end())
}
