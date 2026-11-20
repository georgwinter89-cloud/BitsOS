use crate::layout::*;
use std::vec;
use std::vec::Vec;

const CARD_A_BAR0: u64 = 0xA8_0000_0000;
const CARD_B_BAR0: u64 = 0xA0_0000_0000;
const BAR0_BYTES: u64 = 16 << 30;

/// A blob of `binary_size` bytes, padded with zeros to `read`.
fn blob(binary_size: u16, read: usize) -> Vec<u8> {
    let mut b = vec![0u8; read.max(12)];
    b[..4].copy_from_slice(&BINARY_SIGNATURE.to_le_bytes());
    b[4..6].copy_from_slice(&1u16.to_le_bytes());
    b[10..12].copy_from_slice(&binary_size.to_le_bytes());
    for (i, byte) in b.iter_mut().enumerate().take((binary_size as usize).min(read)).skip(12) {
        *byte = (i % 251) as u8;
    }
    let end = (binary_size as usize).min(read);
    if end > 10 {
        let sum = b[10..end].iter().fold(0u16, |s, &x| s.wrapping_add(x as u16));
        b[8..10].copy_from_slice(&sum.to_le_bytes());
    }
    b.truncate(read);
    b
}

#[test]
fn the_blob_lies_where_the_note_says() {
    assert_eq!(vram_bytes(MEMSIZE_MIB_SEEN), Ok(17_095_983_104));
    assert_eq!(discovery_offset(MEMSIZE_MIB_SEEN), Ok(0x3_FAFF_0000));
    assert_eq!(discovery_phys(CARD_A_BAR0, BAR0_BYTES, MEMSIZE_MIB_SEEN), Some(0xAB_FAFF_0000));
    assert_eq!(discovery_phys(CARD_B_BAR0, BAR0_BYTES, MEMSIZE_MIB_SEEN), Some(0xA3_FAFF_0000));
}

#[test]
fn the_whole_read_must_fit_in_bar0() {
    assert_eq!(discovery_phys(CARD_A_BAR0, 256 << 20, MEMSIZE_MIB_SEEN), None);
    // Exactly to the end of the BAR is allowed, one byte more is not.
    let offset = discovery_offset(MEMSIZE_MIB_SEEN).unwrap();
    let exact = offset + DISCOVERY_READ as u64;
    assert_eq!(discovery_phys(0, exact, MEMSIZE_MIB_SEEN), Some(offset));
    assert_eq!(discovery_phys(0, exact - 1, MEMSIZE_MIB_SEEN), None);
    assert_eq!(discovery_phys(u64::MAX - 8, BAR0_BYTES, MEMSIZE_MIB_SEEN), None);
}

#[test]
fn a_card_that_does_not_answer_or_reports_no_memory_is_refused() {
    assert_eq!(vram_bytes(u32::MAX), Err(MemsizeError::Gone));
    assert_eq!(vram_bytes(0), Err(MemsizeError::Zero));
    assert_eq!(discovery_offset(u32::MAX), Err(MemsizeError::Gone));
    assert_eq!(discovery_phys(CARD_A_BAR0, BAR0_BYTES, 0), None);
    assert_eq!(discovery_offset(1), Ok((1 << 20) - DISCOVERY_FROM_END));
}

#[test]
fn the_constants_are_linux_7s_not_6_6s() {
    assert_eq!(DISCOVERY_READ, 10 * 1024);
    assert_eq!(DISCOVERY_FROM_END, 64 * 1024);
    assert_eq!(GART_BYTES, 512 * 1024 * 1024);
}

#[test]
fn a_blob_of_any_size_up_to_the_read_is_accepted() {
    for size in [12u16, 13, 4096, 8192, 9000, 10 * 1024] {
        let h = accept(&blob(size, DISCOVERY_READ)).unwrap();
        assert_eq!(h, BlobHeader { version_major: 1, version_minor: 0, binary_size: size });
    }
}

#[test]
fn an_8_kib_read_of_a_longer_table_is_refused_not_cut_short() {
    let full = blob(9000, DISCOVERY_READ);
    assert!(accept(&full).is_ok());
    assert_eq!(accept(&full[..8 << 10]), Err(BlobRefusal::Size { binary_size: 9000, read: 8 << 10 }));
}

#[test]
fn a_size_past_the_read_or_below_the_header_is_refused() {
    let b = blob(11 * 1024, DISCOVERY_READ);
    assert_eq!(accept(&b), Err(BlobRefusal::Size { binary_size: 11 * 1024, read: DISCOVERY_READ }));
    for size in [0u16, 1, 11] {
        let b = blob(size, DISCOVERY_READ);
        assert_eq!(accept(&b), Err(BlobRefusal::Size { binary_size: size, read: DISCOVERY_READ }));
    }
}

#[test]
fn a_wrong_signature_is_refused_and_named() {
    let mut b = blob(4096, DISCOVERY_READ);
    b[0] ^= 1;
    assert_eq!(accept(&b), Err(BlobRefusal::Signature { found: BINARY_SIGNATURE ^ 1 }));
    // What an unset VRAM or a dead link reads as.
    assert_eq!(accept(&[0xFF; 64]), Err(BlobRefusal::Signature { found: u32::MAX }));
}

#[test]
fn a_short_read_is_refused() {
    assert_eq!(accept(&[]), Err(BlobRefusal::TooShort { read: 0 }));
    assert_eq!(accept(&blob(4096, DISCOVERY_READ)[..11]), Err(BlobRefusal::TooShort { read: 11 }));
}

#[test]
fn a_flipped_bit_inside_the_blob_is_caught_outside_it_is_not_looked_at() {
    let mut b = blob(4096, DISCOVERY_READ);
    let good = accept(&b).unwrap();
    b[4095] ^= 0x10;
    assert!(matches!(accept(&b), Err(BlobRefusal::Checksum { .. })));
    b[4095] ^= 0x10;
    b[4096] ^= 0x10;
    assert_eq!(accept(&b), Ok(good));
}

#[test]
fn the_checksum_covers_the_size_field_and_starts_right_after_itself() {
    // Hand-summed: size bytes 0x0C and 0x00 are all there is.
    let mut b = vec![0u8; 12];
    b[..4].copy_from_slice(&BINARY_SIGNATURE.to_le_bytes());
    b[10] = 12;
    b[8] = 12;
    assert!(accept(&b).is_ok());
    b[8] = 13;
    assert_eq!(accept(&b), Err(BlobRefusal::Checksum { stored: 13, computed: 12 }));
}

#[test]
fn the_checksum_wraps_at_16_bits() {
    let mut big = vec![0xFFu8; 60000];
    big[..4].copy_from_slice(&BINARY_SIGNATURE.to_le_bytes());
    big[10..12].copy_from_slice(&60000u16.to_le_bytes());
    let sum: u64 = big[10..].iter().map(|&x| x as u64).sum();
    assert!(sum > 0xFFFF, "a sum past 16 bits is the point of this test");
    big[8..10].copy_from_slice(&(sum as u16).to_le_bytes());
    assert!(accept(&big).is_ok());
}

#[test]
fn dma_stops_at_44_bits() {
    assert!(dma_reachable(0, 1 << 44));
    assert!(dma_reachable((1 << 44) - 4096, 4096));
    assert!(!dma_reachable((1 << 44) - 4096, 4097));
    assert!(!dma_reachable(1 << 44, 1));
    assert!(!dma_reachable(u64::MAX, 2));
}

#[test]
fn the_seen_vram_base_is_outside_what_a_16_gib_bar_offset_can_reach() {
    const { assert!(VRAM_MC_BASE_SEEN >= BAR0_BYTES) };
    assert!(discovery_offset(MEMSIZE_MIB_SEEN).unwrap() < 1 << 34);
}
