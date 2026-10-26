//! Hostile tables never panic (V1 `firmware-test.mjs` idea, done as a property test): random bytes into
//! every parser, and random mutations of the Mini-PC world, sometimes with the checksum fixed afterwards so
//! the parsers behind the checksum see the garbage too.

#[allow(unused_imports)]
use alloc::{string::String, vec, vec::Vec};
use crate::testkit::*;
use crate::parse::parse;

/// xorshift64*: deterministic, no dependency.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next() % n }
    }
}

#[test]
fn random_bytes_into_every_parser() {
    let mut r = Rng(0x00C0_FFEE_1234_5678);
    for _ in 0..20_000 {
        let len = r.below(400) as usize;
        let mut t: Vec<u8> = (0..len).map(|_| r.next() as u8).collect();
        // Half the time make the entry walks start at plausible offsets with small lengths.
        if r.below(2) == 0 {
            for i in (44..t.len()).step_by(7) {
                t[i] = r.below(16) as u8;
            }
        }
        let mut n = crate::Notes::new();
        let _ = crate::fadt::parse(&t, 0, &mut n);
        let _ = crate::madt::parse(&t, 0, &mut n);
        let _ = crate::hpet::parse(&t, 0, &mut n);
        let _ = crate::mcfg::parse(&t, 0, &mut n);
        let _ = crate::ivrs::parse(&t, 0, &mut n);
        let _ = crate::srat::parse(&t, 0, &mut n);
        let _ = crate::s5::find(&t);
        let _ = crate::sdt::Header::parse(&t);
    }
}

#[test]
fn s5_scanner_on_truncated_names() {
    // Every prefix of a good `Name(_S5, ...)`: no panic, and only the complete one is found.
    let mut full = vec![0u8; 36];
    full.extend_from_slice(&s5_aml(5, 5));
    for cut in 36..full.len() {
        assert_eq!(crate::s5::find(&full[..cut]), None, "cut at {cut}");
    }
    assert!(crate::s5::find(&full).is_some());
    // PkgLength with three follow bytes claiming a huge package.
    let mut t = vec![0u8; 36];
    t.extend_from_slice(&[0x08, b'_', b'S', b'5', b'_', 0x12, 0xCF, 0xFF, 0xFF, 0xFF, 0x01, 0x00]);
    assert_eq!(crate::s5::find(&t), None);
}

#[test]
fn mutated_worlds() {
    let mut r = Rng(0x5EED_0000_0000_0042);
    let base = minipc();
    for round in 0..3_000 {
        let mut m = base.clone();
        let flips = 1 + r.below(6);
        for _ in 0..flips {
            let i = r.below(m.blobs.len() as u64) as usize;
            let (_, b) = &mut m.blobs[i];
            if b.is_empty() {
                continue;
            }
            let at = r.below(b.len() as u64) as usize;
            match r.below(4) {
                0 => b[at] = r.next() as u8,
                1 if b.len() >= 8 => {
                    // A hostile length field.
                    let v = [0u32, 1, 35, 36, 37, 0xFFFF, 0x0100_0000, u32::MAX][r.below(8) as usize];
                    b[4..8].copy_from_slice(&v.to_le_bytes());
                }
                2 => b.truncate(at),
                _ => b[at] ^= 1 << r.below(8),
            }
            // Keep the checksum right in most rounds so the parsers see the garbage.
            if r.below(4) != 0 && b.len() >= 36 && &b[..8] != b"RSD PTR " {
                fix_checksum(b, 9);
            }
        }
        let a = parse(&mut m, at::RSDP);
        let _ = crate::report::summary(&a, None);
        assert!(m.bytes_read < 4 << 20, "round {round}: read {} bytes", m.bytes_read);
    }
}

#[test]
fn self_referencing_root_and_overlapping_tables() {
    // The XSDT lists itself and the RSDP; a table overlaps another. Nothing loops, nothing panics.
    let mut m = minipc();
    m.put(at::XSDT, xsdt(&[at::XSDT, at::RSDP, at::FADT, at::FADT + 4, at::MADT]));
    let a = parse(&mut m, at::RSDP);
    assert!(a.fadt.is_some() && a.madt.is_some());
    assert_eq!(a.tables.len(), 5, "every readable header is listed, whatever it says");
}
