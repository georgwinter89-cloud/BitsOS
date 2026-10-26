//! The two checksums ext4 uses: crc32c (Castagnoli) for metadata_csum, as Linux's `ext4_chksum` computes it
//! (the raw register, no inversion before or after: linux-6.6/fs/ext4/ext4.h:2449), and crc16 (poly 0x8005,
//! reflected; linux-6.6/lib/crc16.c) for the old `gdt_csum` group descriptors. Software tables: the kernel is
//! built without SSE, and the SSE4.2 `crc32` instruction would need a target-feature function.

const fn table32() -> [[u32; 256]; 8] {
    let mut t = [[0u32; 256]; 8];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { (c >> 1) ^ 0x82F6_3B78 } else { c >> 1 };
            k += 1;
        }
        t[0][i] = c;
        i += 1;
    }
    let mut s = 1;
    while s < 8 {
        let mut i = 0;
        while i < 256 {
            let p = t[s - 1][i];
            t[s][i] = (p >> 8) ^ t[0][(p & 0xFF) as usize];
            i += 1;
        }
        s += 1;
    }
    t
}

static T32: [[u32; 256]; 8] = table32();

/// crc32c of `data` continued from `crc` (slicing by 8).
pub fn crc32c(mut crc: u32, data: &[u8]) -> u32 {
    let mut chunks = data.chunks_exact(8);
    for c in &mut chunks {
        let lo = crc ^ u32::from_le_bytes([c[0], c[1], c[2], c[3]]);
        crc = T32[7][(lo & 0xFF) as usize]
            ^ T32[6][(lo >> 8 & 0xFF) as usize]
            ^ T32[5][(lo >> 16 & 0xFF) as usize]
            ^ T32[4][(lo >> 24) as usize]
            ^ T32[3][c[4] as usize]
            ^ T32[2][c[5] as usize]
            ^ T32[1][c[6] as usize]
            ^ T32[0][c[7] as usize];
    }
    for &b in chunks.remainder() {
        crc = (crc >> 8) ^ T32[0][((crc ^ b as u32) & 0xFF) as usize];
    }
    crc
}

const fn table16() -> [u16; 256] {
    let mut t = [0u16; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u16;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { (c >> 1) ^ 0xA001 } else { c >> 1 };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
}

static T16: [u16; 256] = table16();

pub fn crc16(mut crc: u16, data: &[u8]) -> u16 {
    for &b in data {
        crc = (crc >> 8) ^ T16[((crc ^ b as u16) & 0xFF) as usize];
    }
    crc
}

#[cfg(test)]
mod t {
    use super::*;
    #[test]
    fn known_values() {
        // The standard check value of CRC-32C is with inversion before and after.
        assert_eq!(!crc32c(!0, b"123456789"), 0xE306_9283);
        assert_eq!(!crc32c(!0, b"a much longer input that crosses several eight-byte chunks"), {
            let mut c = !0u32;
            for &b in b"a much longer input that crosses several eight-byte chunks" {
                c = (c >> 8) ^ T32[0][((c ^ b as u32) & 0xFF) as usize];
            }
            !c
        });
        // CRC-16/ARC check value.
        assert_eq!(crc16(0, b"123456789"), 0xBB3D);
    }
}
