//! What the packet encoders share: the error type (Linux has BUG_ON where these return EINVAL) and helpers.

use alloc::vec::Vec;

/// Why an encoder refused; nothing was appended when one is returned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncodeError {
    /// The address does not have the alignment the packet needs.
    Misaligned { addr: u64, align: u64 },
    /// A length or count outside the field's range (or zero where the packet needs at least one).
    Length { what: &'static str, value: u64 },
    /// A field value the hardware does not define.
    Field { what: &'static str, value: u64 },
}

impl EncodeError {
    pub fn errno(&self) -> i32 {
        crate::errno::EINVAL
    }
}

/// Low then high dword of a 64-bit address.
pub(crate) fn push_addr(out: &mut Vec<u32>, addr: u64) {
    out.push(addr as u32);
    out.push((addr >> 32) as u32);
}
