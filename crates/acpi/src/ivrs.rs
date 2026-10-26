//! The IVRS (AMD I/O Virtualization Technology spec 48882, §5.2): presence and the IOMMU units it names.
//! Only the IVHD block headers are read; device entries and IVMD blocks come with an IOMMU driver.

use crate::bytes::{u16_at, u32_at, u64_at, u8_at};
use crate::{Note, Notes};
use alloc::vec::Vec;

const SIG: [u8; 4] = *b"IVRS";
const BLOCKS: usize = 48;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Iommu {
    /// IVHD type 0x10, 0x11 or 0x40.
    pub ivhd_type: u8,
    pub flags: u8,
    /// PCI requester ID of the IOMMU (bus << 8 | dev << 3 | fn).
    pub device_id: u16,
    pub cap_offset: u16,
    pub base: u64,
    pub segment: u16,
    pub info: u16,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Ivrs {
    pub iv_info: u32,
    pub iommus: Vec<Iommu>,
    /// IVMD (memory definition) blocks, counted only.
    pub ivmd_blocks: u32,
}

pub fn parse(t: &[u8], phys: u64, notes: &mut Notes) -> Option<Ivrs> {
    if t.len() < BLOCKS {
        notes.push(Note::table(SIG, phys, "too_short", t.len() as u64));
        return None;
    }
    let mut r = Ivrs { iv_info: u32_at(t, 36)?, ..Ivrs::default() };
    let mut o = BLOCKS;
    while o < t.len() {
        let (Some(ty), Some(len)) = (u8_at(t, o), u16_at(t, o + 2)) else {
            notes.push(Note::table(SIG, phys, "block_cut", o as u64));
            break;
        };
        let len = len as usize;
        if len < 4 || o + len > t.len() {
            notes.push(Note::table(SIG, phys, "block_length", o as u64));
            break;
        }
        let b = &t[o..o + len];
        match ty {
            0x10 | 0x11 | 0x40 => match ivhd(ty, b) {
                Some(u) => r.iommus.push(u),
                None => notes.push(Note::table(SIG, phys, "ivhd_short", o as u64)),
            },
            0x20..=0x22 => r.ivmd_blocks += 1,
            _ => {}
        }
        o += len;
    }
    Some(r)
}

fn ivhd(ty: u8, b: &[u8]) -> Option<Iommu> {
    Some(Iommu {
        ivhd_type: ty,
        flags: u8_at(b, 1)?,
        device_id: u16_at(b, 4)?,
        cap_offset: u16_at(b, 6)?,
        base: u64_at(b, 8)?,
        segment: u16_at(b, 16)?,
        info: u16_at(b, 18)?,
    })
}
