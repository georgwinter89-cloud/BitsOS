//! The FAT32 boot sector (BPB) and FSInfo. FAT32 is recognised by its structure, never by the "FAT32" text
//! (V1 `fs/fatmount.asm:169`, Microsoft's FAT spec §3.5): no fixed root directory, FATSz16 0, FATSz32 > 0 and at
//! least 65525 clusters. Every number is checked against the others and the disk size; a boot sector that does
//! not add up is refused with a reason (never a panic on what a stick holds).

use alloc::vec::Vec;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Geometry {
    /// Bytes per sector (512, 1024, 2048 or 4096).
    pub bps: usize,
    /// Sectors per cluster (a power of two).
    pub spc: u32,
    pub reserved: u64,
    pub fats: u8,
    /// Sectors per FAT.
    pub fat_size: u64,
    /// The FAT copies are mirrored (BPB_ExtFlags bit 7 clear). With mirroring off only one FAT is active.
    pub mirrored: bool,
    pub active_fat: u8,
    pub total: u64,
    pub data_start: u64,
    /// Data clusters: valid cluster numbers are 2 ..= clusters + 1.
    pub clusters: u32,
    pub root: u32,
    /// The FSInfo sector, when the BPB names one inside the reserved area.
    pub fsinfo: Option<u64>,
    pub label: [u8; 11],
    pub serial: u32,
}

impl Geometry {
    pub fn cluster_bytes(&self) -> usize {
        self.bps * self.spc as usize
    }
    /// First sector of a data cluster (callers checked the cluster is valid).
    pub fn cluster_sector(&self, c: u32) -> u64 {
        self.data_start + (c as u64 - 2) * self.spc as u64
    }
    pub fn valid_cluster(&self, c: u32) -> bool {
        c >= 2 && c <= self.clusters + 1
    }
    /// Volumes with one FAT, or with mirroring switched off, are mounted read-only (V1 refused them; a second
    /// FAT copy that is not kept would be overwritten by our writes).
    pub fn writable_layout(&self) -> bool {
        self.fats >= 2 && self.mirrored
    }
}

fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

/// Parses sector 0 of a volume of `dev_sectors` sectors of `dev_bs` bytes.
pub fn parse(b: &[u8], dev_bs: usize, dev_sectors: u64) -> Result<Geometry, &'static str> {
    if b.len() < 512 {
        return Err("boot sector shorter than 512 bytes");
    }
    let bps = u16_at(b, 11) as usize;
    if !matches!(bps, 512 | 1024 | 2048 | 4096) {
        return Err("bytes per sector not 512, 1024, 2048 or 4096");
    }
    if bps != dev_bs {
        return Err("sector size differs from the disk's block size");
    }
    let spc = b[13] as u32;
    if spc == 0 || !spc.is_power_of_two() || bps * spc as usize > 65536 {
        return Err("sectors per cluster not a power of two or clusters above 64 KiB");
    }
    let reserved = u16_at(b, 14) as u64;
    let fats = b[16];
    if reserved == 0 || fats == 0 {
        return Err("no reserved sectors or no FAT");
    }
    if u16_at(b, 17) != 0 || u16_at(b, 22) != 0 {
        return Err("not FAT32 (fixed root directory or 16-bit FAT size)");
    }
    let fat_size = u32_at(b, 36) as u64;
    if fat_size == 0 {
        return Err("not FAT32 (no 32-bit FAT size)");
    }
    let total = match u16_at(b, 19) {
        0 => u32_at(b, 32) as u64,
        n => n as u64,
    };
    if total > dev_sectors {
        return Err("volume larger than its disk or partition");
    }
    let data_start = reserved + fats as u64 * fat_size;
    if data_start >= total {
        return Err("no data area");
    }
    let clusters64 = (total - data_start) / spc as u64;
    if clusters64 < 65525 {
        return Err("not FAT32 (fewer than 65525 clusters)");
    }
    if clusters64 > 0x0FFF_FFF5 {
        return Err("more clusters than FAT32 can address");
    }
    let clusters = clusters64 as u32;
    if fat_size * bps as u64 / 4 < clusters as u64 + 2 {
        return Err("FAT too small for the clusters");
    }
    if u16_at(b, 42) != 0 {
        return Err("unknown FAT32 version");
    }
    let ext = u16_at(b, 40);
    let root = u32_at(b, 44) & 0x0FFF_FFFF;
    let geo_root_ok = root >= 2 && root <= clusters + 1;
    if !geo_root_ok {
        return Err("root cluster outside the volume");
    }
    let fsinfo = match u16_at(b, 48) as u64 {
        0 | 0xFFFF => None,
        s if s < reserved => Some(s),
        _ => None,
    };
    let mut label = [0u8; 11];
    label.copy_from_slice(&b[71..82]);
    Ok(Geometry {
        bps,
        spc,
        reserved,
        fats,
        fat_size,
        mirrored: ext & 0x80 == 0,
        active_fat: (ext & 0x0F) as u8,
        total,
        data_start,
        clusters,
        root,
        fsinfo,
        label,
        serial: u32_at(b, 67),
    })
}

/// What FSInfo holds (FAT spec §5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FsInfo {
    pub free: u32,
    pub next: u32,
}

pub const UNKNOWN: u32 = 0xFFFF_FFFF;

pub fn parse_fsinfo(b: &[u8]) -> Option<FsInfo> {
    if b.len() < 512 || u32_at(b, 0) != 0x4161_5252 || u32_at(b, 484) != 0x6141_7272 || u32_at(b, 508) != 0xAA55_0000 {
        return None;
    }
    Some(FsInfo { free: u32_at(b, 488), next: u32_at(b, 492) })
}

/// The FSInfo sector `old` with new counts (the signatures and reserved bytes stay as they are).
pub fn fsinfo_with(old: &[u8], f: FsInfo) -> Vec<u8> {
    let mut v = old.to_vec();
    v[488..492].copy_from_slice(&f.free.to_le_bytes());
    v[492..496].copy_from_slice(&f.next.to_le_bytes());
    v
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use alloc::vec;

    /// A FAT32 boot sector as mkfs.fat writes one (for unit tests that need no image).
    pub fn sector(spc: u8, total: u32, fat_size: u32) -> Vec<u8> {
        let mut b = vec![0u8; 512];
        b[0..3].copy_from_slice(&[0xEB, 0x58, 0x90]);
        b[11..13].copy_from_slice(&512u16.to_le_bytes());
        b[13] = spc;
        b[14..16].copy_from_slice(&32u16.to_le_bytes());
        b[16] = 2;
        b[21] = 0xF8;
        b[32..36].copy_from_slice(&total.to_le_bytes());
        b[36..40].copy_from_slice(&fat_size.to_le_bytes());
        b[44..48].copy_from_slice(&2u32.to_le_bytes());
        b[48..50].copy_from_slice(&1u16.to_le_bytes());
        b[50..52].copy_from_slice(&6u16.to_le_bytes());
        b[71..82].copy_from_slice(b"BITSOS     ");
        b[510] = 0x55;
        b[511] = 0xAA;
        b
    }

    #[test]
    fn recognised_by_structure() {
        let b = sector(1, 140_000, 1077);
        let g = parse(&b, 512, 140_000).unwrap();
        assert_eq!((g.data_start, g.clusters, g.root, g.fsinfo), (32 + 2154, 140_000 - 2186, 2, Some(1)));
        assert!(g.writable_layout());
        assert_eq!(&g.label, b"BITSOS     ");
        assert_eq!(parse(&b, 512, 139_999), Err("volume larger than its disk or partition"));
        assert_eq!(parse(&b, 4096, 140_000), Err("sector size differs from the disk's block size"));
        let mut small = sector(8, 140_000, 1077);
        small[13] = 8;
        assert_eq!(parse(&small, 512, 140_000), Err("not FAT32 (fewer than 65525 clusters)"));
        let mut f16 = b.clone();
        f16[22] = 1;
        assert!(parse(&f16, 512, 140_000).is_err());
        let mut one = b.clone();
        one[40] = 0x80;
        assert!(!parse(&one, 512, 140_000).unwrap().writable_layout());
        let mut spc3 = b.clone();
        spc3[13] = 3;
        assert!(parse(&spc3, 512, 140_000).is_err());
        let mut tiny_fat = b.clone();
        tiny_fat[36..40].copy_from_slice(&100u32.to_le_bytes());
        assert!(parse(&tiny_fat, 512, 140_000).is_err());
        let mut root = b.clone();
        root[44..48].copy_from_slice(&1u32.to_le_bytes());
        assert!(parse(&root, 512, 140_000).is_err());
        assert!(parse(&b[..100], 512, 140_000).is_err());
    }

    #[test]
    fn fsinfo_round_trip() {
        let mut s = vec![0u8; 512];
        s[0..4].copy_from_slice(&0x4161_5252u32.to_le_bytes());
        s[484..488].copy_from_slice(&0x6141_7272u32.to_le_bytes());
        s[508..512].copy_from_slice(&0xAA55_0000u32.to_le_bytes());
        let n = fsinfo_with(&s, FsInfo { free: 7, next: 9 });
        assert_eq!(parse_fsinfo(&n), Some(FsInfo { free: 7, next: 9 }));
        assert_eq!(parse_fsinfo(&vec![0u8; 512]), None);
    }
}
