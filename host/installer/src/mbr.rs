//! The stick's partition table: an MBR with one FAT32 partition of type 0Ch from 1 MiB to the last whole MiB.
//! BitsOS writes its stick only when the table is plain MBR with no Windows or EFI type (block crate's R8a check), so
//! no GPT, no EFh; the old GPT copies at both ends of the disk are wiped with the rest of the start and the end.

pub const SECTOR: u64 = 512;
pub const START_LBA: u64 = 2048;
pub const TYPE_FAT32_LBA: u8 = 0x0C;
/// FAT32 counts sectors in 32 bits.
pub const MAX_SECTORS: u64 = u32::MAX as u64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Part {
    pub start: u64,
    pub sectors: u64,
    pub kind: u8,
}

/// The one partition on a disk of `disk_sectors` 512-byte sectors.
pub fn layout(disk_sectors: u64) -> Result<Part, String> {
    let usable = disk_sectors.saturating_sub(START_LBA + 2048);
    let sectors = (usable / 2048 * 2048).min(MAX_SECTORS / 2048 * 2048);
    if sectors < (64 << 20) / SECTOR {
        return Err("the disk is too small for BitsOS (at least 128 MB)".into());
    }
    Ok(Part { start: START_LBA, sectors, kind: TYPE_FAT32_LBA })
}

pub fn build(p: Part, signature: u32) -> [u8; 512] {
    let mut s = [0u8; 512];
    s[440..444].copy_from_slice(&signature.to_le_bytes());
    let e = &mut s[446..462];
    e[0] = 0x80;
    // CHS fields beyond 1024 cylinders: the usual "use the LBA fields" marker.
    e[1..4].copy_from_slice(&[0xFE, 0xFF, 0xFF]);
    e[4] = p.kind;
    e[5..8].copy_from_slice(&[0xFE, 0xFF, 0xFF]);
    e[8..12].copy_from_slice(&(p.start as u32).to_le_bytes());
    e[12..16].copy_from_slice(&(p.sectors as u32).to_le_bytes());
    s[510] = 0x55;
    s[511] = 0xAA;
    s
}

/// Sector 0 is itself a FAT32 boot sector (a stick formatted without a table, as `xtask image` makes them).
pub fn is_fat32_boot(s: &[u8]) -> bool {
    s.len() >= 512 && (s[0] == 0xEB || s[0] == 0xE9) && &s[82..90] == b"FAT32   " && s[510] == 0x55 && s[511] == 0xAA
}

/// Where a FAT32 volume of this disk starts: the whole disk, or the first MBR partition of a FAT type.
pub fn find_fat(s0: &[u8], disk_sectors: u64) -> Option<Part> {
    if is_fat32_boot(s0) {
        return Some(Part { start: 0, sectors: disk_sectors, kind: 0 });
    }
    if s0.len() < 512 || s0[510] != 0x55 || s0[511] != 0xAA {
        return None;
    }
    (0..4).find_map(|i| {
        let e = &s0[446 + 16 * i..462 + 16 * i];
        let start = u32::from_le_bytes(e[8..12].try_into().ok()?) as u64;
        let sectors = u32::from_le_bytes(e[12..16].try_into().ok()?) as u64;
        (matches!(e[4], 0x0B | 0x0C) && (e[0] == 0 || e[0] == 0x80) && start > 0 && start + sectors <= disk_sectors).then_some(Part { start, sectors, kind: e[4] })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_partition_from_one_mib() {
        let p = layout(60_555_264).unwrap();
        assert_eq!(p.start, 2048);
        assert_eq!(p.sectors % 2048, 0);
        assert!(p.start + p.sectors <= 60_555_264 - 2048, "the last MiB stays free (an old GPT copy is wiped there)");
        let s = build(p, 0x1234_5678);
        assert_eq!(find_fat(&s, 60_555_264), Some(p));
        assert_eq!(s[446], 0x80);
        assert_eq!(s[450], 0x0C);
        assert!(layout(100_000).is_err());
        // Larger than FAT32 can count: capped.
        assert!(layout(10_000_000_000).unwrap().sectors <= MAX_SECTORS);
    }
}
