//! Table builders and a fake physical memory for host tests and the synthetic ktests (the V1
//! `acpi-tables.mjs` / `firmware-test.mjs` ideas: Mini-PC-like tables, then the same with one thing broken).

use crate::mem::{MemError, PhysMem};
use alloc::vec;
use alloc::vec::Vec;

/// Physical memory made of blobs at addresses; everything else is unreadable (a hole).
#[derive(Clone, Default)]
pub struct FakeMem {
    pub blobs: Vec<(u64, Vec<u8>)>,
    /// Bytes handed out by `read` (to prove hostile lengths are not followed).
    pub bytes_read: u64,
}

impl FakeMem {
    pub fn put(&mut self, at: u64, b: Vec<u8>) {
        self.blobs.retain(|(a, _)| *a != at);
        self.blobs.push((at, b));
    }
    pub fn get_mut(&mut self, at: u64) -> Option<&mut Vec<u8>> {
        self.blobs.iter_mut().find(|(a, _)| *a == at).map(|(_, b)| b)
    }
    fn byte(&self, p: u64) -> Option<u8> {
        self.blobs.iter().find_map(|(a, b)| p.checked_sub(*a).and_then(|o| b.get(o as usize).copied()))
    }
}

impl PhysMem for FakeMem {
    fn read(&mut self, phys: u64, out: &mut [u8]) -> Result<(), MemError> {
        for (i, o) in out.iter_mut().enumerate() {
            let p = phys.checked_add(i as u64).ok_or(MemError::OutOfRange)?;
            *o = self.byte(p).ok_or(MemError::NotTableMemory)?;
        }
        self.bytes_read += out.len() as u64;
        Ok(())
    }
}

pub fn fix_checksum(t: &mut [u8], at: usize) {
    t[at] = 0;
    let s = crate::bytes::sum(t);
    t[at] = 0u8.wrapping_sub(s);
}

/// A table: 36-byte header (OEM "BITSOS"), body, checksum fixed.
pub fn sdt(sig: &[u8; 4], rev: u8, body: &[u8]) -> Vec<u8> {
    let mut t = vec![0u8; 36];
    t[..4].copy_from_slice(sig);
    t[4..8].copy_from_slice(&((36 + body.len()) as u32).to_le_bytes());
    t[8] = rev;
    t[10..16].copy_from_slice(b"BITSOS");
    t[16..24].copy_from_slice(b"TESTTBL ");
    t.extend_from_slice(body);
    fix_checksum(&mut t, 9);
    t
}

pub fn rsdp(rev: u8, rsdt: u32, xsdt: u64) -> Vec<u8> {
    let mut r = vec![0u8; 36];
    r[..8].copy_from_slice(b"RSD PTR ");
    r[9..15].copy_from_slice(b"ALASKA");
    r[15] = rev;
    r[16..20].copy_from_slice(&rsdt.to_le_bytes());
    r[20..24].copy_from_slice(&36u32.to_le_bytes());
    r[24..32].copy_from_slice(&xsdt.to_le_bytes());
    fix_checksum(&mut r[..20], 8);
    fix_checksum(&mut r, 32);
    r
}

pub fn xsdt(addrs: &[u64]) -> Vec<u8> {
    let body: Vec<u8> = addrs.iter().flat_map(|a| a.to_le_bytes()).collect();
    sdt(b"XSDT", 1, &body)
}

pub fn rsdt(addrs: &[u32]) -> Vec<u8> {
    let body: Vec<u8> = addrs.iter().flat_map(|a| a.to_le_bytes()).collect();
    sdt(b"RSDT", 1, &body)
}

fn gas(space: u8, width: u8, access: u8, addr: u64) -> [u8; 12] {
    let mut g = [0u8; 12];
    g[0] = space;
    g[1] = width;
    g[3] = access;
    g[4..].copy_from_slice(&addr.to_le_bytes());
    g
}

/// The Mini-PC's FADT (HW-MINIPC round 5): revision 6, 276 bytes, reset register I/O 0xB2 <- 0xBE,
/// PM1a control 0x804, PM timer 0x808 (32 bit), SMI_CMD 0xB2, Low Power S0 Idle, no 8042, no legacy.
pub fn fadt_minipc(dsdt: u64) -> Vec<u8> {
    let mut b = vec![0u8; 276 - 36];
    let mut put = |o: usize, v: &[u8]| b[o - 36..o - 36 + v.len()].copy_from_slice(v);
    put(40, &(dsdt as u32).to_le_bytes());
    put(46, &9u16.to_le_bytes());
    put(48, &0xB2u32.to_le_bytes());
    put(52, &[0xA0, 0xA1]);
    put(56, &0x800u32.to_le_bytes());
    put(64, &0x804u32.to_le_bytes());
    put(76, &0x808u32.to_le_bytes());
    put(80, &0x820u32.to_le_bytes());
    put(88, &[4, 2, 0, 4, 8, 0]);
    put(109, &0u16.to_le_bytes());
    let flags = crate::fadt::flags::RESET_REG_SUP | crate::fadt::flags::TMR_VAL_EXT | crate::fadt::flags::LOW_POWER_S0_IDLE;
    put(112, &flags.to_le_bytes());
    put(116, &gas(1, 8, 1, 0xB2));
    put(128, &[0xBE]);
    put(131, &[1]);
    put(140, &dsdt.to_le_bytes());
    put(148, &gas(1, 32, 3, 0x800));
    put(172, &gas(1, 16, 2, 0x804));
    put(208, &gas(1, 32, 3, 0x808));
    put(220, &gas(1, 64, 1, 0x820));
    sdt(b"FACP", 6, &b)
}

/// The Mini-PC's MADT: 16 xAPIC CPUs, I/O APICs 33 @ 0xFEC00000 (GSI 0) and 34 @ 0xFEC01000 (GSI 24),
/// overrides 0->2, 1->1 (flags 7), 9->9 (flags 0xF), NMI on LINT1 of all CPUs, 8259s present.
pub fn madt_minipc() -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&0xFEE0_0000u32.to_le_bytes());
    b.extend_from_slice(&1u32.to_le_bytes());
    for id in 0..16u8 {
        b.extend_from_slice(&[0, 8, id, id, 1, 0, 0, 0]);
    }
    for (id, addr, base) in [(33u8, 0xFEC0_0000u32, 0u32), (34, 0xFEC0_1000, 24)] {
        b.extend_from_slice(&[1, 12, id, 0]);
        b.extend_from_slice(&addr.to_le_bytes());
        b.extend_from_slice(&base.to_le_bytes());
    }
    for (irq, gsi, flags) in [(0u8, 2u32, 0u16), (1, 1, 7), (9, 9, 0xF)] {
        b.extend_from_slice(&[2, 10, 0, irq]);
        b.extend_from_slice(&gsi.to_le_bytes());
        b.extend_from_slice(&flags.to_le_bytes());
    }
    b.extend_from_slice(&[4, 6, 0xFF, 5, 0, 1]);
    sdt(b"APIC", 5, &b)
}

/// HPET at 0xFED00000: AMD (0x1022), 3 comparators, 32-bit counter, legacy capable.
pub fn hpet_minipc() -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&0x1022_8201u32.to_le_bytes());
    b.extend_from_slice(&gas(0, 64, 0, 0xFED0_0000));
    b.extend_from_slice(&[0, 0x80, 0x00, 0]);
    sdt(b"HPET", 1, &b)
}

pub fn mcfg(entries: &[(u64, u16, u8, u8)]) -> Vec<u8> {
    let mut b = vec![0u8; 8];
    for &(base, seg, s, e) in entries {
        b.extend_from_slice(&base.to_le_bytes());
        b.extend_from_slice(&seg.to_le_bytes());
        b.extend_from_slice(&[s, e, 0, 0, 0, 0]);
    }
    sdt(b"MCFG", 1, &b)
}

/// IVRS with one IVHD type 0x40 for the IOMMU at 00:00.2, registers at 0xFD200000.
pub fn ivrs_minipc() -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&0x0020_3043u32.to_le_bytes());
    b.extend_from_slice(&[0u8; 8]);
    let mut h = vec![0x40u8, 0xB0];
    h.extend_from_slice(&40u16.to_le_bytes());
    h.extend_from_slice(&0x0002u16.to_le_bytes());
    h.extend_from_slice(&0x40u16.to_le_bytes());
    h.extend_from_slice(&0xFD20_0000u64.to_le_bytes());
    h.extend_from_slice(&0u16.to_le_bytes());
    h.extend_from_slice(&0u16.to_le_bytes());
    h.resize(40, 0);
    b.extend_from_slice(&h);
    sdt(b"IVRS", 2, &b)
}

/// SRAT with one domain: two CPUs (xAPIC and x2APIC entries) and one memory range.
pub fn srat_one_domain() -> Vec<u8> {
    let mut b = vec![1u8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    b.extend_from_slice(&[0, 16, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    let mut x2 = vec![2u8, 24, 0, 0];
    x2.extend_from_slice(&0u32.to_le_bytes());
    x2.extend_from_slice(&300u32.to_le_bytes());
    x2.extend_from_slice(&1u32.to_le_bytes());
    x2.resize(24, 0);
    b.extend_from_slice(&x2);
    let mut m = vec![1u8, 40, 0, 0, 0, 0, 0, 0];
    m.extend_from_slice(&0u32.to_le_bytes());
    m.extend_from_slice(&0u32.to_le_bytes());
    m.extend_from_slice(&0u32.to_le_bytes());
    m.extend_from_slice(&1u32.to_le_bytes());
    m.extend_from_slice(&[0u8; 4]);
    m.extend_from_slice(&1u32.to_le_bytes());
    m.resize(40, 0);
    b.extend_from_slice(&m);
    sdt(b"SRAT", 3, &b)
}

/// `Name(_S5, Package(4){a, b, Zero, Zero})` with BytePrefix constants, as the Mini-PC's DSDT has it.
pub fn s5_aml(a: u8, b: u8) -> Vec<u8> {
    vec![0x08, b'_', b'S', b'5', b'_', 0x12, 0x08, 0x04, 0x0A, a, 0x0A, b, 0x00, 0x00]
}

/// A DSDT: some AML noise, then `aml`.
pub fn dsdt(aml: &[u8]) -> Vec<u8> {
    let mut b = vec![0x10, 0x05, b'\\', b'_', b'S', b'B'];
    b.extend_from_slice(aml);
    sdt(b"DSDT", 2, &b)
}

/// Offsets of the Mini-PC-like world from its base. The FADT crosses a 4 KiB page boundary on purpose.
pub mod off {
    pub const RSDP: u64 = 0;
    pub const XSDT: u64 = 0x1000;
    pub const RSDT: u64 = 0x1800;
    pub const FADT: u64 = 0x2F80;
    pub const MADT: u64 = 0x3200;
    pub const HPET: u64 = 0x4000;
    pub const MCFG: u64 = 0x5000;
    pub const IVRS: u64 = 0x6000;
    pub const SRAT: u64 = 0x7000;
    pub const SSDT1: u64 = 0x8000;
    pub const SSDT2: u64 = 0x9000;
    pub const DSDT: u64 = 0xC000;
    /// Bytes the world spans.
    pub const SPAN: u64 = 0xD000;
}

/// Addresses of the world at its host-test base.
pub mod at {
    use super::off;
    pub const BASE: u64 = 0x9A00_0000;
    pub const RSDP: u64 = BASE + off::RSDP;
    pub const XSDT: u64 = BASE + off::XSDT;
    pub const RSDT: u64 = BASE + off::RSDT;
    pub const FADT: u64 = BASE + off::FADT;
    pub const MADT: u64 = BASE + off::MADT;
    pub const HPET: u64 = BASE + off::HPET;
    pub const MCFG: u64 = BASE + off::MCFG;
    pub const IVRS: u64 = BASE + off::IVRS;
    pub const SRAT: u64 = BASE + off::SRAT;
    pub const SSDT1: u64 = BASE + off::SSDT1;
    pub const SSDT2: u64 = BASE + off::SSDT2;
    pub const DSDT: u64 = BASE + off::DSDT;
}

/// RSDP rev 2 -> XSDT (and an RSDT with the same tables) -> FADT, MADT, HPET, MCFG, IVRS, SRAT, two SSDTs;
/// DSDT with `\_S5` = 5. `base` must be below 4 GiB (the RSDT holds 32-bit addresses).
pub fn minipc_at(base: u64) -> FakeMem {
    let a = |o: u64| base + o;
    let mut m = FakeMem::default();
    let list = [off::FADT, off::MADT, off::HPET, off::MCFG, off::IVRS, off::SRAT, off::SSDT1, off::SSDT2].map(a);
    m.put(a(off::RSDP), rsdp(2, a(off::RSDT) as u32, a(off::XSDT)));
    m.put(a(off::XSDT), xsdt(&list));
    m.put(a(off::RSDT), rsdt(&list.map(|x| x as u32)));
    m.put(a(off::FADT), fadt_minipc(a(off::DSDT)));
    m.put(a(off::MADT), madt_minipc());
    m.put(a(off::HPET), hpet_minipc());
    m.put(a(off::MCFG), mcfg(&[(0xE000_0000, 0, 0, 255)]));
    m.put(a(off::IVRS), ivrs_minipc());
    m.put(a(off::SRAT), srat_one_domain());
    m.put(a(off::SSDT1), sdt(b"SSDT", 2, &[0x10, 0x00]));
    m.put(a(off::SSDT2), sdt(b"SSDT", 2, &[0x10, 0x00]));
    m.put(a(off::DSDT), dsdt(&s5_aml(5, 5)));
    m
}

pub fn minipc() -> FakeMem {
    minipc_at(at::BASE)
}
