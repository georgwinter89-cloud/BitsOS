//! Reading field values out of a report: little-endian bit order, bit 0 of a byte first
//! (HID 1.11 §8.4, Linux `hid_field_extract`).

use crate::descriptor::Field;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExtractError {
    /// The value has more than 32 bits.
    TooWide,
    /// Some of the value's bits lie past the end of the report.
    PastEnd,
    /// No such value in the field.
    NoSuchValue,
}

/// `bits` (at most 32) bits at bit `offset` of `data`, zero-extended; `PastEnd` if they do not all fit.
pub fn bits(data: &[u8], offset: u64, bits: u32) -> Result<u32, ExtractError> {
    if bits > 32 {
        return Err(ExtractError::TooWide);
    }
    if bits == 0 {
        return Ok(0);
    }
    let end = offset
        .checked_add(bits as u64)
        .ok_or(ExtractError::PastEnd)?;
    if end > data.len() as u64 * 8 {
        return Err(ExtractError::PastEnd);
    }
    Ok(bits_padded(data, offset, bits))
}

/// As `bits`, but bits past the end read as 0 (Linux pads a short report with zeros) and a value
/// wider than 32 bits gives its low 32 bits (Linux `hid_field_extract` warns and does the same).
pub fn bits_padded(data: &[u8], offset: u64, bits: u32) -> u32 {
    let bits = bits.min(32);
    let mut v: u64 = 0;
    let first = offset / 8;
    let shift = (offset % 8) as u32;
    let bytes = (shift + bits).div_ceil(8) as u64;
    for i in 0..bytes {
        let b = first
            .checked_add(i)
            .and_then(|at| usize::try_from(at).ok())
            .and_then(|at| data.get(at));
        v |= (*b.unwrap_or(&0) as u64) << (8 * i);
    }
    let v = v >> shift;
    if bits == 32 {
        v as u32
    } else {
        (v & ((1u64 << bits) - 1)) as u32
    }
}

/// Sign-extends the low `bits` bits (Linux `snto32`).
pub fn sign_extend(value: u32, bits: u32) -> i32 {
    match bits {
        0 => 0,
        1..=31 => ((value << (32 - bits)) as i32) >> (32 - bits),
        _ => value as i32,
    }
}

impl Field {
    /// Raw value `n` of the field from report data that starts after the ID byte.
    pub fn raw(&self, data: &[u8], n: u32) -> Result<u32, ExtractError> {
        if n >= self.count {
            return Err(ExtractError::NoSuchValue);
        }
        bits(data, self.value_offset(n), self.size)
    }

    /// Value `n`, sign-extended when the logical minimum is below 0 (Linux `hid_input_fetch_field`).
    pub fn value(&self, data: &[u8], n: u32) -> Result<i32, ExtractError> {
        let raw = self.raw(data, n)?;
        Ok(self.interpret(raw))
    }

    pub(crate) fn interpret(&self, raw: u32) -> i32 {
        if self.logical_min < 0 {
            sign_extend(raw, self.size)
        } else {
            raw as i32
        }
    }
}
