//! The boot volume's identity: `STICK.ID` and the label and serial in the FAT boot sector (block 0 of the
//! volume, which the STICK record carries as its first 96 bytes).

use alloc::string::String;

/// 32 hex digits (surrounding whitespace allowed) -> 16 bytes.
pub fn parse_stick_id(text: &[u8]) -> Option<[u8; 16]> {
    let t = core::str::from_utf8(text).ok()?.trim();
    if t.len() != 32 || !t.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut id = [0u8; 16];
    for (i, b) in id.iter_mut().enumerate() {
        *b = u8::from_str_radix(&t[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(id)
}

/// Label and serial of a FAT boot sector (FAT32 extended BPB at 0x40, FAT12/16 at 0x24).
#[derive(Debug, PartialEq, Eq)]
pub struct FatId {
    pub fat32: bool,
    pub serial: u32,
    pub label: String,
}

impl FatId {
    /// The serial as Linux and Windows show it (`5892-386D`).
    pub fn serial_text(&self) -> String {
        alloc::format!("{:04X}-{:04X}", self.serial >> 16, self.serial & 0xFFFF)
    }
}

pub fn fat_id(block0: &[u8]) -> Option<FatId> {
    if block0.len() < 90 || !(block0[0] == 0xEB || block0[0] == 0xE9) {
        return None;
    }
    let bytes_per_sector = u16::from_le_bytes([block0[11], block0[12]]);
    if !matches!(bytes_per_sector, 512 | 1024 | 2048 | 4096) {
        return None;
    }
    // FAT32: sectors-per-FAT (16 bit) is 0 and the extended signature sits at 0x42.
    let fat32 = u16::from_le_bytes([block0[22], block0[23]]) == 0;
    let base = if fat32 { 0x40 } else { 0x24 };
    // Extended boot signature 0x29 (0x28 has the serial but no label).
    let sig = block0[base + 2];
    if sig != 0x29 && sig != 0x28 {
        return None;
    }
    let s = base + 3;
    let serial = u32::from_le_bytes([block0[s], block0[s + 1], block0[s + 2], block0[s + 3]]);
    let label = if sig == 0x29 {
        block0[s + 4..s + 15].iter().map(|&c| if c.is_ascii_graphic() || c == b' ' { c as char } else { '?' }).collect::<String>()
    } else {
        String::new()
    };
    Some(FatId { fat32, serial, label: String::from(label.trim_end()) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stick_ids() {
        let id = parse_stick_id(b"00112233445566778899aabbccddeeff\n").unwrap();
        assert_eq!(id[0], 0x00);
        assert_eq!(id[15], 0xFF);
        assert!(parse_stick_id(b"0011").is_none());
        assert!(parse_stick_id(b"+0112233445566778899aabbccddeeff").is_none());
        assert!(parse_stick_id(b"zz112233445566778899aabbccddeeff").is_none());
        assert!(parse_stick_id(&[0xff; 32]).is_none());
    }

    fn fat32_sector(serial: u32, label: &[u8; 11]) -> [u8; 96] {
        let mut b = [0u8; 96];
        b[0] = 0xEB;
        b[11..13].copy_from_slice(&512u16.to_le_bytes());
        b[0x42] = 0x29;
        b[0x43..0x47].copy_from_slice(&serial.to_le_bytes());
        b[0x47..0x52].copy_from_slice(label);
        b
    }

    #[test]
    fn fat32_label_and_serial() {
        let f = fat_id(&fat32_sector(0x5892_386D, b"BITSOS     ")).unwrap();
        assert!(f.fat32);
        assert_eq!(f.label, "BITSOS");
        assert_eq!(f.serial_text(), "5892-386D");
    }

    #[test]
    fn fat16_and_garbage() {
        let mut b = [0u8; 96];
        b[0] = 0xE9;
        b[11..13].copy_from_slice(&512u16.to_le_bytes());
        b[22] = 9;
        b[0x26] = 0x29;
        b[0x27..0x2B].copy_from_slice(&0x1234_ABCDu32.to_le_bytes());
        b[0x2B..0x36].copy_from_slice(b"NO NAME    ");
        let f = fat_id(&b).unwrap();
        assert!(!f.fat32);
        assert_eq!((f.label.as_str(), f.serial_text().as_str()), ("NO NAME", "1234-ABCD"));
        assert!(fat_id(&[0u8; 96]).is_none());
        assert!(fat_id(&[0xEB; 10]).is_none());
        // An MBR (partition table, no BPB) is not taken for a FAT sector.
        let mut mbr = [0u8; 96];
        mbr[0] = 0xEB;
        assert!(fat_id(&mbr).is_none());
    }
}
