//! The RSDP (ACPI 6.5 §5.2.5): where the XSDT (or, before revision 2, the RSDT) is.

use crate::bytes::{slice, sum, u32_at, u64_at, u8_at};
use crate::mem::{MemError, PhysMem};

/// Bytes the first checksum covers (ACPI 1.0 structure).
const V1_LEN: usize = 20;
/// Bytes the extended checksum covers. Linux uses the fixed 36 too and ignores the length field.
const V2_LEN: usize = 36;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rsdp {
    pub phys: u64,
    pub revision: u8,
    pub oem_id: [u8; 6],
    pub rsdt: u32,
    /// 0 before revision 2.
    pub xsdt: u64,
    /// Length field (revision 2+); informational.
    pub length: u32,
    pub checksum_ok: bool,
    /// Extended checksum over 36 bytes; false before revision 2.
    pub ext_checksum_ok: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RsdpError {
    Unreadable(MemError),
    BadSignature,
}

pub fn read(mem: &mut dyn PhysMem, phys: u64) -> Result<Rsdp, RsdpError> {
    let mut b = [0u8; V2_LEN];
    mem.read(phys, &mut b[..V1_LEN]).map_err(RsdpError::Unreadable)?;
    if &b[..8] != b"RSD PTR " {
        return Err(RsdpError::BadSignature);
    }
    let revision = b[15];
    let checksum_ok = sum(&b[..V1_LEN]) == 0;
    let mut r = Rsdp {
        phys,
        revision,
        oem_id: slice(&b, 9, 6).and_then(|s| s.try_into().ok()).unwrap_or([0; 6]),
        rsdt: u32_at(&b, 16).unwrap_or(0),
        xsdt: 0,
        length: 0,
        checksum_ok,
        ext_checksum_ok: false,
    };
    if revision >= 2 {
        // The extended part may be unreadable on a broken firmware; then only the RSDT is usable.
        if mem.read(phys + V1_LEN as u64, &mut b[V1_LEN..]).is_ok() {
            r.length = u32_at(&b, 20).unwrap_or(0);
            r.xsdt = u64_at(&b, 24).unwrap_or(0);
            r.ext_checksum_ok = sum(&b) == 0 && u8_at(&b, 32).is_some();
        }
    }
    Ok(r)
}
